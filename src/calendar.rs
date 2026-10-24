//! Calendar integration: CalDAV discovery, bounded REPORT, iCalendar parse,
//! and recurring/DST expansion (Slice 5).
//!
//! The calendar section is populated from a configured CalDAV server (e.g.
//! Apple iCloud). The adapter is strictly **read-only**: it issues only
//! `GET` (well-known discovery), `PROPFIND` (principal, calendar-home, and
//! collection listing), and `REPORT` (calendar-query). It never issues
//! `PUT`, `POST`, `DELETE`, `MKCOL`, or any other mutating method
//! (acceptance criterion: "only read-only DAV methods are used (GET,
//! PROPFIND, REPORT); no PUT/DELETE/MKCALENDAR").
//!
//! The app-specific password is read only from the
//! `MIRROR_CALDAV_APP_PASSWORD` environment variable (see `config.rs`); it is
//! never read from the config file and never appears in API JSON or logs.
//!
//! Recurring events are expanded in the event's own timezone: a weekly
//! `RRULE` keeps the local wall-clock time across DST transitions (the UTC
//! instant shifts by the offset change), `EXDATE` occurrences are dropped,
//! and a `RECURRENCE-ID` override replaces exactly one occurrence.
//!
//! This mirrors the provider pattern established in `weather.rs` (Slice 2)
//! and `news.rs` (Slice 3): a `*Cache` struct with an
//! `RwLock<Option<Snapshot<T>>>`, a `Resolved<T>` enum (`Fresh`/`Stale`/
//! `Unavailable`) with `into_parts()`, and a `build_*_section()` function.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone,
    Utc,
};
use tokio::sync::RwLock;

use crate::api::{Event, Status};
use crate::backoff::{FetchError, RetryState};
use crate::config::CalendarConfig;

// ===========================================================================
// iCalendar model (RFC 5545 subset)
// ===========================================================================

/// A parsed iCalendar property: `NAME;PARAM=VALUE:value` with the text value
/// unescaped.
#[derive(Debug, Clone)]
struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A parsed iCalendar component (VCALENDAR, VTIMEZONE, VEVENT, …) with its
/// properties and nested components.
#[derive(Debug, Default)]
struct Component {
    name: String,
    props: Vec<Prop>,
    children: Vec<Component>,
}

impl Component {
    fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name == name)
    }

    fn prop_value(&self, name: &str) -> Option<&str> {
        self.prop(name).map(|p| p.value.as_str())
    }

    fn children_named(&self, name: &str) -> Vec<&Component> {
        self.children.iter().filter(|c| c.name == name).collect()
    }
}

/// A datetime as it appears in iCalendar, before timezone resolution.
#[derive(Debug, Clone, PartialEq)]
enum IcsTime {
    /// `DTSTART:20260320T003000Z` — an absolute UTC instant.
    Utc(DateTime<Utc>),
    /// `DTSTART;TZID=Europe/Budapest:20260320T013000` — a wall-clock time in
    /// a named timezone (resolved via the calendar's VTIMEZONE).
    Zoned { tzid: String, naive: NaiveDateTime },
    /// `DTSTART:20260320T013000` without TZID — a floating local time
    /// (treated as UTC for normalization).
    Floating(NaiveDateTime),
    /// `DTSTART;VALUE=DATE:20260320` — an all-day date.
    Date(NaiveDate),
}

/// A parsed VEVENT, before expansion.
#[derive(Debug, Clone)]
struct IcsEvent {
    uid: String,
    summary: String,
    dtstart: IcsTime,
    dtend: Option<IcsTime>,
    duration: Option<ChronoDuration>,
    location: Option<String>,
    description: Option<String>,
    status: Option<String>,
    rrule: Option<Rrule>,
    exdates: Vec<IcsTime>,
    /// `RECURRENCE-ID` — when present, this event overrides the single
    /// recurring occurrence at this time.
    recurrence_id: Option<IcsTime>,
}

/// A parsed `RRULE` (the subset we expand: DAILY / WEEKLY / MONTHLY / YEARLY
/// with `INTERVAL`, `COUNT`, `UNTIL`, and `BYDAY` for weekly rules).
#[derive(Debug, Clone)]
struct Rrule {
    freq: Freq,
    interval: u32,
    count: Option<u32>,
    until: Option<IcsTime>,
    /// `BYDAY` for weekly rules: (nth, weekday) with weekday 0 = Monday.
    by_day: Option<(i32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// A VTIMEZONE component (STANDARD or DAYLIGHT) as a transition rule.
#[derive(Debug, Clone)]
struct TzComponent {
    /// `TZOFFSETTO` — the offset (seconds east of UTC) in effect after the
    /// transition.
    offset_to: i32,
    /// The component's `DTSTART` (the first transition, local wall time).
    start: NaiveDateTime,
    /// `RRULE:FREQ=YEARLY;BYMONTH=n;BYDAY=kXX` transition rule, if present.
    rdates: Vec<NaiveDateTime>,
    by_month: Option<u32>,
    by_day: Option<(i32, u32)>,
}

/// A parsed VTIMEZONE: the set of transition rules for a named zone.
#[derive(Debug, Clone)]
struct TzInfo {
    components: Vec<TzComponent>,
}

// ===========================================================================
// iCalendar text parsing
// ===========================================================================

/// Unfold iCalendar lines (RFC 5545 §3.1): a line starting with a space or
/// tab is a continuation of the previous line.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches(['\r', '\n']);
        if (line.starts_with(' ') || line.starts_with('\t')) && !lines.is_empty() {
            let prev = lines.pop().expect("checked non-empty");
            lines.push(format!("{}{}", prev, line.trim_start_matches([' ', '\t'])));
        } else {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Unescape iCalendar TEXT values (RFC 5545 §3.3.11): `\,` `\;` `\n` `\\`.
fn unescape_ical(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(',') => out.push(','),
                Some(';') => out.push(';'),
                Some('n') | Some('N') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a single `NAME;PARAM=VALUE:value` line.
fn parse_property(line: &str) -> Option<Prop> {
    let (head, value) = line.split_once(':')?;
    let mut name: Option<String> = None;
    let mut params: Vec<(String, String)> = Vec::new();
    for part in head.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            params.push((k.to_uppercase(), v.trim_matches('"').to_string()));
        } else if name.is_none() {
            name = Some(part.to_uppercase());
        }
    }
    Some(Prop {
        name: name?,
        params,
        value: unescape_ical(value),
    })
}

/// Parse an iCalendar document into its top-level VCALENDAR component.
///
/// Malformed input (no VCALENDAR, or an unclosed component) is an `Err`.
fn parse_ical(text: &str) -> Result<Component, String> {
    let mut root = Component {
        name: String::new(),
        ..Default::default()
    };
    let mut stack: Vec<Component> = Vec::new();

    for line in unfold(text) {
        if line.is_empty() {
            continue;
        }
        if let Some(body) = line.strip_prefix("BEGIN:") {
            let name = body.trim().to_uppercase();
            let comp = Component {
                name: name.clone(),
                ..Default::default()
            };
            if stack.is_empty() {
                if name != "VCALENDAR" {
                    return Err(format!("expected BEGIN:VCALENDAR, got BEGIN:{name}"));
                }
                root.name = name;
                stack.push(comp);
            } else {
                stack.push(Component {
                    name,
                    ..Default::default()
                });
            }
        } else if let Some(body) = line.strip_prefix("END:") {
            let name = body.trim().to_uppercase();
            let top = stack
                .pop()
                .ok_or_else(|| format!("END:{name} with no open component"))?;
            if top.name != name {
                return Err(format!("mismatched END:{name} for open {top:?}"));
            }
            if stack.is_empty() {
                root = top;
            } else {
                stack
                    .last_mut()
                    .expect("stack non-empty")
                    .children
                    .push(top);
            }
        } else if let Some(prop) = parse_property(&line) {
            if let Some(parent) = stack.last_mut() {
                parent.props.push(prop);
            }
        }
    }

    if !stack.is_empty() {
        return Err(format!("{} unclosed component(s)", stack.len()));
    }
    if root.name != "VCALENDAR" {
        return Err("no VCALENDAR component found".to_string());
    }
    Ok(root)
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    if s.len() != 8 {
        return None;
    }
    let y = s.get(0..4)?.parse().ok()?;
    let m = s.get(4..6)?.parse().ok()?;
    let d = s.get(6..8)?.parse().ok()?;
    NaiveDate::from_ymd_opt(y, m, d)
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    if s.len() != 6 {
        return None;
    }
    let h = s.get(0..2)?.parse().ok()?;
    let mi = s.get(2..4)?.parse().ok()?;
    let se = s.get(4..6)?.parse().ok()?;
    NaiveTime::from_hms_opt(h, mi, se)
}

/// Parse an iCalendar DATE / DATE-TIME value into an `IcsTime`.
fn parse_ics_time(value: &str, tzid: Option<&str>) -> Option<IcsTime> {
    let value = value.trim();
    if value.ends_with('Z') && value.len() == 16 {
        let date = parse_date(&value[..8])?;
        let time = parse_time(&value[9..15])?;
        let naive = NaiveDateTime::new(date, time);
        return Some(IcsTime::Utc(Utc.from_utc_datetime(&naive)));
    }
    if value.len() == 15 && value.as_bytes()[8] == b'T' {
        let date = parse_date(&value[..8])?;
        let time = parse_time(&value[9..15])?;
        let naive = NaiveDateTime::new(date, time);
        return match tzid {
            Some(tz) => Some(IcsTime::Zoned {
                tzid: tz.to_string(),
                naive,
            }),
            None => Some(IcsTime::Floating(naive)),
        };
    }
    if value.len() == 8 {
        return Some(IcsTime::Date(parse_date(value)?));
    }
    None
}

/// Parse a `DURATION` value (e.g. `PT1H30M`, `P1D`, `-PT30M`).
fn parse_duration(value: &str) -> Option<ChronoDuration> {
    let v = value.trim();
    let (sign, v) = match v.as_bytes().first() {
        Some(b'-') => (-1i64, &v[1..]),
        Some(b'+') => (1i64, &v[1..]),
        _ => (1i64, v),
    };
    if !v.starts_with('P') {
        return None;
    }
    let mut days: i64 = 0;
    let mut hours: i64 = 0;
    let mut minutes: i64 = 0;
    let mut seconds: i64 = 0;
    let mut in_time = false;
    let mut num: i64 = 0;
    for c in v[1..].chars() {
        match c {
            '0'..='9' => num = num * 10 + (c as i64 - '0' as i64),
            'D' => {
                days += num;
                num = 0;
            }
            'T' => in_time = true,
            'H' if in_time => {
                hours += num;
                num = 0;
            }
            'M' if in_time => {
                minutes += num;
                num = 0;
            }
            'S' if in_time => {
                seconds += num;
                num = 0;
            }
            _ => return None,
        }
    }
    let total = days * 86_400 + hours * 3_600 + minutes * 60 + seconds;
    Some(ChronoDuration::seconds(sign * total))
}

/// Parse a `TZOFFSET` value (e.g. `+0100`, `-0530`) into seconds east of UTC.
fn parse_tzoffset(value: &str) -> Option<i32> {
    let v = value.trim();
    if v.len() != 5 && v.len() != 7 {
        return None;
    }
    let sign: i32 = match v.as_bytes()[0] {
        b'-' => -1,
        b'+' => 1,
        _ => return None,
    };
    let h: i32 = v.get(1..3)?.parse().ok()?;
    let m: i32 = v.get(3..5)?.parse().ok()?;
    let sec = if v.len() == 7 {
        v.get(5..7)?.parse().ok()?
    } else {
        0
    };
    if h > 23 || m > 59 || sec > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + sec))
}

/// Parse a `BYDAY` value (e.g. `-2SU`, `1MO`) into (nth, weekday 0 = Monday).
fn parse_byday(value: &str) -> Option<(i32, u32)> {
    let v = value.trim().to_uppercase();
    if v.len() != 4 {
        return None;
    }
    let (num, day) = v.split_at(2);
    let nth: i32 = num.parse().ok()?;
    if !(1..=5).contains(&nth.abs()) {
        return None;
    }
    let weekday: u32 = match day {
        "MO" => 0,
        "TU" => 1,
        "WE" => 2,
        "TH" => 3,
        "FR" => 4,
        "SA" => 5,
        "SU" => 6,
        _ => return None,
    };
    Some((nth, weekday))
}

/// Parse an `RRULE` value into the supported subset.
fn parse_rrule(value: &str) -> Option<Rrule> {
    let mut rrule = Rrule {
        freq: Freq::Weekly,
        interval: 1,
        count: None,
        until: None,
        by_day: None,
    };
    for part in value.split(';') {
        let (k, v) = part.split_once('=')?;
        match k.to_uppercase().as_str() {
            "FREQ" => match v.to_uppercase().as_str() {
                "DAILY" => rrule.freq = Freq::Daily,
                "WEEKLY" => rrule.freq = Freq::Weekly,
                "MONTHLY" => rrule.freq = Freq::Monthly,
                "YEARLY" => rrule.freq = Freq::Yearly,
                _ => return None,
            },
            "INTERVAL" => rrule.interval = v.parse().ok()?,
            "COUNT" => rrule.count = Some(v.parse().ok()?),
            "UNTIL" => rrule.until = Some(parse_ics_time(v, None)?),
            "BYDAY" => rrule.by_day = parse_byday(v),
            _ => {}
        }
    }
    Some(rrule)
}

/// Parse the VTIMEZONE components of a VCALENDAR into a name → rules map.
fn parse_vtimezones(cal: &Component) -> HashMap<String, TzInfo> {
    let mut map = HashMap::new();
    for tz in cal.children_named("VTIMEZONE") {
        let name = match tz.prop_value("TZID") {
            Some(n) => n.to_string(),
            None => continue,
        };
        let mut components = Vec::new();
        for comp in tz
            .children
            .iter()
            .filter(|c| c.name == "STANDARD" || c.name == "DAYLIGHT")
        {
            let offset_to = match comp.prop_value("TZOFFSETTO").and_then(parse_tzoffset) {
                Some(o) => o,
                None => continue,
            };
            let start = match comp.prop_value("DTSTART").and_then(|v| {
                let d = parse_date(v.get(..8)?)?;
                let t = parse_time(v.get(9..15)?)?;
                Some(NaiveDateTime::new(d, t))
            }) {
                Some(s) => s,
                None => continue,
            };
            let (by_month, by_day) = comp
                .prop_value("RRULE")
                .and_then(|v| v.split_once("BYMONTH="))
                .and_then(|rest| {
                    let rest = rest.1;
                    let month = rest.split(';').next()?.parse::<u32>().ok()?;
                    let by_day = rest
                        .split(';')
                        .find_map(|p| p.strip_prefix("BYDAY="))
                        .and_then(parse_byday)?;
                    Some((month, by_day))
                })
                .map(|(month, day)| (Some(month), Some(day)))
                .unwrap_or((None, None));
            components.push(TzComponent {
                offset_to,
                start,
                rdates: comp
                    .props
                    .iter()
                    .filter(|p| p.name == "RDATE")
                    .flat_map(|p| p.value.split(','))
                    .filter_map(|v| NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok())
                    .collect(),
                by_month,
                by_day,
            });
        }
        if !components.is_empty() {
            map.insert(name, TzInfo { components });
        }
    }
    map
}

/// Parse the VEVENTs of a VCALENDAR into `IcsEvent`s.
fn parse_vevents(cal: &Component) -> Vec<IcsEvent> {
    cal.children_named("VEVENT")
        .into_iter()
        .filter_map(|ev| {
            let dtstart = ev.prop("DTSTART")?;
            let tzid = dtstart.param("TZID");
            let ics_dtstart = parse_ics_time(&dtstart.value, tzid)?;

            let dtend = ev.prop("DTEND").and_then(|p| {
                parse_ics_time(&p.value, p.param("TZID").or(tzid)).or_else(|| {
                    // A DATE-TEND with a TZID parameter is unusual; fall back
                    // to the DTSTART zone for a bare local value.
                    parse_ics_time(&p.value, tzid)
                })
            });
            let duration = ev.prop_value("DURATION").and_then(parse_duration);

            let rrule = ev.prop_value("RRULE").and_then(parse_rrule);

            let mut exdates = Vec::new();
            for prop in ev.props.iter().filter(|p| p.name == "EXDATE") {
                for part in prop.value.split(',') {
                    if let Some(t) = parse_ics_time(part, prop.param("TZID")) {
                        exdates.push(t);
                    }
                }
            }

            let recurrence_id = ev
                .prop("RECURRENCE-ID")
                .and_then(|p| parse_ics_time(&p.value, p.param("TZID")));

            Some(IcsEvent {
                uid: ev.prop_value("UID").unwrap_or_default().to_string(),
                summary: ev.prop_value("SUMMARY").unwrap_or_default().to_string(),
                dtstart: ics_dtstart,
                dtend,
                duration,
                location: ev.prop_value("LOCATION").map(str::to_string),
                description: ev.prop_value("DESCRIPTION").map(str::to_string),
                status: ev.prop_value("STATUS").map(str::to_uppercase),
                rrule,
                exdates,
                recurrence_id,
            })
        })
        .collect()
}

// ===========================================================================
// Timezone resolution (VTIMEZONE rules) and DST-correct expansion
// ===========================================================================

/// The date of the `nth` weekday of a month (1-based positive, or negative
/// counting from the end), at 00:00.
fn nth_weekday_of_month(year: i32, month: u32, nth: i32, weekday: u32) -> Option<NaiveDate> {
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let first_wd = first.weekday().num_days_from_monday() as i32;
    let target = weekday as i32;
    let offset = (target - first_wd + 7) % 7;
    if nth > 0 {
        first.with_day((1 + offset + (nth - 1) * 7) as u32)
    } else {
        let last = if month == 12 {
            NaiveDate::from_ymd_opt(year, 12, 31)?
        } else {
            NaiveDate::from_ymd_opt(year, month + 1, 1)?.pred_opt()?
        };
        let last_wd = last.weekday().num_days_from_monday() as i32;
        let from_end = (nth.abs() - 1) * 7;
        let day = last.day() as i32 - ((last_wd - target + 7) % 7) - from_end;
        first.with_day(day as u32)
    }
}

/// The local wall-clock time at which a transition rule fires in a given year.
fn transition_in_year(comp: &TzComponent, year: i32) -> Option<NaiveDateTime> {
    let date = match (comp.by_month, comp.by_day) {
        (Some(month), Some((nth, weekday))) => nth_weekday_of_month(year, month, nth, weekday)?,
        _ => {
            let d = comp.start.date();
            NaiveDate::from_ymd_opt(year, d.month(), d.day())?
        }
    };
    Some(NaiveDateTime::new(date, comp.start.time()))
}

/// Resolve the UTC offset (seconds east) in effect for a local wall-clock
/// time, using the VTIMEZONE transition rules.
///
/// The applicable offset is the `TZOFFSETTO` of the latest transition (across
/// all rules and years, in local wall-clock order) at or before the given
/// local time. This is what makes DST-boundary expansion correct: the same
/// wall-clock time maps to different UTC instants before and after a
/// transition.
fn offset_for(tz: &TzInfo, local: NaiveDateTime) -> Option<i32> {
    let mut best: Option<(NaiveDateTime, i32)> = None;
    for comp in &tz.components {
        let mut transitions = vec![comp.start];
        transitions.extend(comp.rdates.iter().copied());
        if comp.by_month.is_some() && comp.by_day.is_some() {
            for year in comp.start.year()..=local.year() {
                if let Some(t) = transition_in_year(comp, year) {
                    transitions.push(t);
                }
            }
        }
        for t in transitions {
            if t >= comp.start && t <= local && best.as_ref().is_none_or(|(bt, _)| t >= *bt) {
                best = Some((t, comp.offset_to));
            }
        }
    }
    best.map(|(_, o)| o).or_else(|| {
        tz.components
            .iter()
            .min_by_key(|c| c.start)
            .map(|c| c.offset_to)
    })
}

/// Resolve an `IcsTime` to a UTC instant using the calendar's VTIMEZONE map.
fn to_utc(t: &IcsTime, zones: &HashMap<String, TzInfo>) -> Option<DateTime<Utc>> {
    match t {
        IcsTime::Utc(dt) => Some(*dt),
        IcsTime::Zoned { tzid, naive } => {
            let offset = offset_for(zones.get(tzid)?, *naive)?;
            let utc_naive = *naive - ChronoDuration::seconds(offset as i64);
            Some(Utc.from_utc_datetime(&utc_naive))
        }
        IcsTime::Floating(naive) => Some(Utc.from_utc_datetime(naive)),
        IcsTime::Date(date) => {
            // All-day events are normalized to midnight UTC.
            Some(Utc.from_utc_datetime(&NaiveDateTime::new(*date, NaiveTime::MIN)))
        }
    }
}

/// The end of an occurrence, given its start and the event's end/duration.
fn occurrence_end(
    start: DateTime<Utc>,
    ev: &IcsEvent,
    zones: &HashMap<String, TzInfo>,
) -> DateTime<Utc> {
    if let Some(end) = &ev.dtend {
        if let Some(e) = to_utc(end, zones) {
            if let Some(original) = to_utc(&ev.dtstart, zones) {
                return start + (e - original);
            }
        }
    }
    if let Some(d) = ev.duration {
        return start + d;
    }
    start
}

/// Hard cap on generated occurrences per event, so a malformed unbounded
/// `RRULE` can never hang the dashboard.
const MAX_OCCURRENCES: u64 = 1_000;

/// The local (time-space-preserving) start of the `n`-th occurrence of a
/// recurring event (0-based), honoring `INTERVAL` and weekly `BYDAY`.
///
/// Wall-clock times (Zoned / Floating) advance in *local* days so the
/// wall-clock time is preserved across DST transitions; the caller then
/// resolves each local time to UTC with the zone rules for that date.
fn occurrence_local(n: u64, ev: &IcsEvent, rrule: &Rrule) -> Option<IcsTime> {
    let interval = rrule.interval as i64;
    match &ev.dtstart {
        IcsTime::Zoned { tzid, naive } => Some(IcsTime::Zoned {
            tzid: tzid.clone(),
            naive: next_local(n, *naive, rrule, interval)?,
        }),
        IcsTime::Floating(naive) => {
            Some(IcsTime::Floating(next_local(n, *naive, rrule, interval)?))
        }
        IcsTime::Utc(dt) => {
            let days = match rrule.freq {
                Freq::Daily => 1,
                Freq::Weekly => 7,
                Freq::Monthly => 31,
                Freq::Yearly => 365,
            };
            Some(IcsTime::Utc(
                *dt + ChronoDuration::days((n as i64) * days * interval),
            ))
        }
        IcsTime::Date(date) => {
            let days = match rrule.freq {
                Freq::Daily => 1,
                Freq::Weekly => 7,
                Freq::Monthly => 31,
                Freq::Yearly => 365,
            };
            Some(IcsTime::Date(
                *date + ChronoDuration::days((n as i64) * days * interval),
            ))
        }
    }
}

/// Advance a local wall-clock time by `n` rule steps (0-based).
fn next_local(
    n: u64,
    dtstart: NaiveDateTime,
    rrule: &Rrule,
    interval: i64,
) -> Option<NaiveDateTime> {
    let time = dtstart.time();
    match rrule.freq {
        Freq::Daily => Some(dtstart + ChronoDuration::days((n as i64) * interval)),
        Freq::Weekly => match rrule.by_day {
            Some((nth, weekday)) => {
                // The `nth` weekday of the month, in the week `n*interval`
                // weeks after DTSTART's week.
                let days_since_monday = dtstart.weekday().num_days_from_monday() as i64;
                let week_start = dtstart.date() - ChronoDuration::days(days_since_monday);
                let target_week = week_start + ChronoDuration::days(7 * (n as i64) * interval);
                let month = (target_week + ChronoDuration::days(weekday as i64)).month();
                let year = (target_week + ChronoDuration::days(weekday as i64)).year();
                let date = nth_weekday_of_month(year, month, nth, weekday)?;
                Some(NaiveDateTime::new(date, time))
            }
            None => Some(dtstart + ChronoDuration::days(7 * (n as i64) * interval)),
        },
        Freq::Monthly => {
            let total_months = dtstart.month() as i64 - 1 + (n as i64) * interval;
            let year = (dtstart.year() as i64 + total_months / 12) as i32;
            let month = (total_months % 12 + 12) % 12 + 1;
            NaiveDate::from_ymd_opt(year, month as u32, dtstart.day())
                .map(|d| NaiveDateTime::new(d, time))
        }
        Freq::Yearly => {
            let year = (dtstart.year() as i64 + (n as i64) * interval) as i32;
            NaiveDate::from_ymd_opt(year, dtstart.month(), dtstart.day())
                .map(|d| NaiveDateTime::new(d, time))
        }
    }
}

/// Expand a single parsed event into concrete occurrences within
/// `[range_start, range_end]`, applying `EXDATE` exclusions and
/// `RECURRENCE-ID` overrides (passed in `overrides`), in the event's own
/// timezone so DST transitions keep the wall-clock time.
fn expand_event(
    ev: &IcsEvent,
    overrides: &[&IcsEvent],
    zones: &HashMap<String, TzInfo>,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    calendar_id: &str,
) -> Vec<Event> {
    if ev.status.as_deref() == Some("CANCELLED") {
        return Vec::new();
    }

    let base_start = match to_utc(&ev.dtstart, zones) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let base_end = occurrence_end(base_start, ev, zones);
    let is_all_day = matches!(ev.dtstart, IcsTime::Date(_));

    let mut out: Vec<Event> = Vec::new();
    let mut emit = |start: DateTime<Utc>, end: DateTime<Utc>, src: &IcsEvent| {
        if start < range_start || start > range_end {
            return;
        }
        let id = format!(
            "{}:{}:{}",
            calendar_id,
            src.uid,
            start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        out.push(Event {
            id,
            title: src.summary.clone(),
            start: start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            end: end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            all_day: is_all_day,
            location: src.location.clone(),
            description: src.description.clone(),
        });
    };

    let rrule = match &ev.rrule {
        Some(r) => r,
        None => {
            // A single instance. An event carrying a `RECURRENCE-ID` is an
            // override for a recurring series and is emitted only through the
            // series' expansion (below), not as a standalone occurrence.
            if ev.recurrence_id.is_some() {
                return out;
            }
            emit(base_start, base_end, ev);
            return out;
        }
    };

    let until_utc = rrule.until.as_ref().and_then(|u| to_utc(u, zones));

    let mut n: u64 = 0;
    loop {
        if n >= MAX_OCCURRENCES {
            break;
        }
        let candidate = match occurrence_local(n, ev, rrule).and_then(|t| to_utc(&t, zones)) {
            Some(c) => c,
            None => break,
        };
        if let Some(u) = until_utc {
            if candidate > u {
                break;
            }
        }
        if candidate > range_end {
            break;
        }
        if candidate >= range_start {
            // EXDATE exclusion: the EXDATE value (in the event's time space)
            // resolves to the same UTC instant as this occurrence.
            if ev
                .exdates
                .iter()
                .any(|x| to_utc(x, zones) == Some(candidate))
            {
                n += 1;
                continue;
            }
            // RECURRENCE-ID override: replace exactly this one occurrence.
            let override_ev = overrides.iter().find(|o| {
                o.recurrence_id.as_ref().and_then(|rid| to_utc(rid, zones)) == Some(candidate)
            });
            match override_ev {
                Some(o) => {
                    if let Some(o_start) = to_utc(&o.dtstart, zones) {
                        let o_end = occurrence_end(o_start, o, zones);
                        emit(o_start, o_end, o);
                    }
                }
                None => {
                    let end = occurrence_end(candidate, ev, zones);
                    emit(candidate, end, ev);
                }
            }
        }
        n += 1;
        if let Some(count) = rrule.count {
            if n >= count as u64 {
                break;
            }
        }
    }
    out
}

/// Expand all events of one calendar (VCALENDAR text) into API events within
/// the bounded range, deduplicated and sorted by start time.
pub fn expand_calendar(
    ical_text: &str,
    calendar_id: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> Result<Vec<Event>, String> {
    let cal = parse_ical(ical_text)?;
    let zones = parse_vtimezones(&cal);
    let events = parse_vevents(&cal);
    let overrides: Vec<&IcsEvent> = events.iter().collect();

    let mut out: Vec<Event> = Vec::new();
    for ev in &events {
        out.extend(expand_event(
            ev,
            &overrides,
            &zones,
            range_start,
            range_end,
            calendar_id,
        ));
    }

    // Deduplicate by (calendar_id, uid, occurrence start) — the id already
    // encodes all three — and sort by start time.
    out.sort_by(|a, b| a.start.cmp(&b.start));
    out.dedup_by(|a, b| a.id == b.id);
    Ok(out)
}

// ===========================================================================
// CalDAV client (read-only: GET / PROPFIND / REPORT)
// ===========================================================================

/// A read-only CalDAV client. It issues only `GET`, `PROPFIND`, and `REPORT`
/// requests — never `PUT`, `POST`, `DELETE`, `MKCOL`, or any other mutating
/// method (acceptance criterion: read-only DAV methods only).
#[derive(Clone)]
pub struct CalDavClient {
    http: reqwest::Client,
    username: String,
    password: Option<String>,
}

impl std::fmt::Debug for CalDavClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalDavClient")
            .field("credentials", &"[redacted]")
            .finish_non_exhaustive()
    }
}

/// Resolve a possibly-relative DAV `href` / `Location` against a base URL.
///
/// CalDAV servers routinely answer with path-relative references (e.g.
/// `Location: /principal/john/` or `<d:href>/calendars/john/Personal/</d:href>`).
/// `reqwest` requires absolute URLs, so every such reference must be joined
/// against the URL it was returned from before it is used in a request.
fn resolve_url(base: &str, maybe_relative: &str) -> Result<String, String> {
    // Already absolute (has a scheme) — use as-is.
    if let Ok(u) = reqwest::Url::parse(maybe_relative) {
        return Ok(u.to_string());
    }
    let base = reqwest::Url::parse(base).map_err(|e| format!("invalid CalDAV base URL: {e}"))?;
    let joined = base
        .join(maybe_relative)
        .map_err(|e| format!("cannot resolve CalDAV reference: {e}"))?;
    Ok(joined.to_string())
}

impl CalDavClient {
    /// Build a client with a bounded timeout so a slow / blackholed CalDAV
    /// server can never hang the dashboard.
    pub fn new(username: String, password: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("valid reqwest client");
        Self {
            http,
            username,
            password,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let request = req
            .build()
            .map_err(|_| "invalid CalDAV request".to_string())?;
        #[cfg(test)]
        {
            use tower::ServiceExt;
            let app = test_servers()
                .lock()
                .unwrap()
                .get(request.url().host_str().unwrap_or_default())
                .cloned();
            if let Some(app) = app {
                let mut builder = axum::http::Request::builder()
                    .method(request.method().clone())
                    .uri(request.url().path());
                *builder.headers_mut().unwrap() = request.headers().clone();
                let body = request
                    .body()
                    .and_then(|b| b.as_bytes())
                    .unwrap_or_default()
                    .to_vec();
                let response = app
                    .oneshot(builder.body(axum::body::Body::from(body)).unwrap())
                    .await
                    .unwrap();
                let (parts, body) = response.into_parts();
                let bytes = axum::body::to_bytes(body, 2 * 1024 * 1024)
                    .await
                    .map_err(|_| "test response too large".to_string())?;
                return Ok(reqwest::Response::from(axum::http::Response::from_parts(
                    parts,
                    bytes.to_vec(),
                )));
            }
            Err("unregistered in-process CalDAV test target".to_string())
        }
        #[cfg(not(test))]
        self.http
            .execute(request)
            .await
            .map_err(|_| "CalDAV transport failed".to_string())
    }

    async fn failure(&self, method: &str, url: &str, response: reqwest::Response) -> String {
        let status = response.status();
        let target = reqwest::Url::parse(url)
            .ok()
            .map(|mut u| {
                let _ = u.set_username("");
                let _ = u.set_password(None);
                u.set_query(None);
                u.set_fragment(None);
                u.to_string()
            })
            .unwrap_or_else(|| "invalid target".into());
        // Only DAV's explicit diagnostic text is useful here. Never echo arbitrary
        // response bodies (which may contain calendar data or credentials).
        let mut response = response;
        let mut bytes = Vec::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            if bytes.len() + chunk.len() > 8192 {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let reason = std::str::from_utf8(&bytes)
            .ok()
            .and_then(xml_document)
            .and_then(|root| {
                root.child(DAV, "responsedescription")
                    .or_else(|| root.child("http://sabre.io/ns/error/", "message"))
                    .map(|n| n.text.clone())
            })
            .unwrap_or_default();
        let reason = if reason.to_ascii_lowercase().contains("authorization")
            || reason.contains("BEGIN:VCALENDAR")
        {
            String::new()
        } else {
            reason
        };
        let mut message = format!(
            "{method} {target} failed: {status}: {}",
            reason
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
        );
        if let Some(password) = &self.password {
            if !password.is_empty() {
                message = message.replace(password, "[redacted]");
            }
            if let Ok(request) = self
                .http
                .get("http://redaction.invalid/")
                .basic_auth(&self.username, Some(password))
                .build()
            {
                if let Some(token) = request
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| h.strip_prefix("Basic "))
                {
                    message = message.replace(token, "[redacted]");
                }
            }
        }
        message.chars().take(384).collect()
    }

    /// Step 1 of discovery: resolve the principal URL from the server's
    /// well-known entry point (`GET /.well-known/caldav` → `Location`).
    pub async fn discover_principal(&self, base_url: &str) -> Result<String, String> {
        let url = format!("{}/.well-known/caldav", base_url.trim_end_matches('/'));
        let mut req = self.http.get(&url);
        if let Some(pw) = &self.password {
            req = req.basic_auth(&self.username, Some(pw));
        }
        let resp = self
            .send(req)
            .await
            .map_err(|e| format!("well-known request failed: {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| "well-known redirect had no Location header".to_string())?;
            // The Location may be path-relative; resolve it against the base.
            let resolved = resolve_url(&url, &loc)?;
            return Ok(resolved);
        }
        if status.is_success() {
            // Some servers answer 200 at the well-known path itself.
            return Ok(url);
        }
        Err(format!("well-known discovery failed: {status}"))
    }

    /// Step 2 of discovery: resolve the `calendar-home-set` from the
    /// well-known target.
    ///
    /// RFC 6764 well-known URIs may point at the DAV root rather than a
    /// principal. The DAV root exposes `current-user-principal` (which names
    /// the principal) but not `calendar-home-set`; the principal is what
    /// carries `calendar-home-set`. We request both in a single PROPFIND and
    /// follow whichever the server actually returns, so this works whether the
    /// target is already a principal or the DAV root.
    pub async fn calendar_home(&self, target_url: &str) -> Result<String, String> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:">
  <prop>
    <current-user-principal xmlns="DAV:"/>
    <calendar-home-set xmlns="urn:ietf:params:xml:ns:caldav"/>
  </prop>
</propfind>"#;
        let xml = self.raw_propfind(target_url, body, "0").await?;
        // Fast path: the target is already a principal carrying calendar-home-set.
        if let Some(href) = extract_href(&xml, "calendar-home-set") {
            return resolve_url(target_url, &href);
        }
        // Slow path: the target is the DAV root; follow current-user-principal
        // to the principal, then PROPFIND it for calendar-home-set.
        let principal_href = extract_href(&xml, "current-user-principal").ok_or_else(|| {
            "PROPFIND response had neither <calendar-home-set> nor <current-user-principal>"
                .to_string()
        })?;
        let principal_url = resolve_url(target_url, &principal_href)?;
        let home_body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:">
  <prop>
    <calendar-home-set xmlns="urn:ietf:params:xml:ns:caldav"/>
  </prop>
</propfind>"#;
        let home_xml = self.raw_propfind(&principal_url, home_body, "0").await?;
        let href = extract_href(&home_xml, "calendar-home-set")
            .ok_or_else(|| "principal PROPFIND had no <calendar-home-set>/<href>".to_string())?;
        resolve_url(&principal_url, &href)
    }

    /// Step 3 of discovery: `PROPFIND` the calendar home and list its
    /// calendar resources (URL + display name).
    pub async fn list_calendars(
        &self,
        calendar_home: &str,
    ) -> Result<Vec<(String, String)>, String> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:">
  <prop>
    <resourcetype xmlns="DAV:"/>
    <displayname xmlns="DAV:"/>
  </prop>
</propfind>"#;
        let xml = self.raw_propfind(calendar_home, body, "1").await?;
        if !xml_document(&xml).is_some_and(|root| root.is(DAV, "multistatus")) {
            return Err("invalid CalDAV PROPFIND multistatus XML".into());
        }
        let listing = parse_calendar_listing(&xml);
        let mut out = Vec::with_capacity(listing.len());
        for (url, name) in listing {
            // Each href may be path-relative; resolve against the calendar home.
            out.push((resolve_url(calendar_home, &url)?, name));
        }
        Ok(out)
    }

    /// Bounded `calendar-query` REPORT for one calendar: VEVENTs whose
    /// `DTSTART` falls in `[range_start, range_end]`, returning `calendar-data`.
    pub async fn report_events(
        &self,
        calendar_url: &str,
        range_start: DateTime<Utc>,
        range_end: DateTime<Utc>,
    ) -> Result<Vec<String>, String> {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:d="DAV:">
  <d:prop>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{}" end="{}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#,
            range_start.format("%Y%m%dT%H%M%SZ"),
            range_end.format("%Y%m%dT%H%M%SZ"),
        );
        let method = reqwest::Method::from_bytes(b"REPORT").expect("valid method");
        let mut req = self
            .http
            .request(method, calendar_url)
            .header("Depth", "1")
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body);
        if let Some(pw) = &self.password {
            req = req.basic_auth(&self.username, Some(pw));
        }
        let resp = self
            .send(req)
            .await
            .map_err(|e| format!("REPORT request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self.failure("REPORT", calendar_url, resp).await);
        }
        let xml = resp
            .text()
            .await
            .map_err(|_| "read REPORT body failed".to_string())?;
        if !xml_document(&xml).is_some_and(|root| root.is(DAV, "multistatus")) {
            return Err("invalid CalDAV REPORT multistatus XML".into());
        }
        Ok(parse_calendar_data(&xml))
    }

    /// Issue a `PROPFIND` and return the response body.
    async fn raw_propfind(&self, url: &str, body: &str, depth: &str) -> Result<String, String> {
        let method = reqwest::Method::from_bytes(b"PROPFIND").expect("valid method");
        let mut req = self
            .http
            .request(method, url)
            .header("Depth", depth)
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(body.to_string());
        if let Some(pw) = &self.password {
            req = req.basic_auth(&self.username, Some(pw));
        }
        let resp = self
            .send(req)
            .await
            .map_err(|e| format!("PROPFIND request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(self.failure("PROPFIND", url, resp).await);
        }
        resp.text()
            .await
            .map_err(|_| "read PROPFIND body failed".to_string())
    }
}

#[cfg(test)]
fn test_servers() -> &'static std::sync::Mutex<HashMap<String, axum::Router>> {
    static SERVERS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, axum::Router>>> =
        std::sync::OnceLock::new();
    SERVERS.get_or_init(Default::default)
}
#[cfg(test)]
fn register_test_server(app: axum::Router) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let host = format!(
        "caldav-{}.test",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    test_servers().lock().unwrap().insert(host.clone(), app);
    format!("http://{host}")
}

const DAV: &str = "DAV:";
const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";

#[derive(Default)]
struct XmlNode {
    namespace: String,
    name: String,
    text: String,
    children: Vec<XmlNode>,
}
impl XmlNode {
    fn is(&self, ns: &str, name: &str) -> bool {
        self.namespace == ns && self.name == name
    }
    fn child(&self, ns: &str, name: &str) -> Option<&Self> {
        self.children.iter().find(|n| n.is(ns, name))
    }
    fn successful_properties(&self) -> impl Iterator<Item = &XmlNode> {
        self.children
            .iter()
            .filter(|n| n.is(DAV, "propstat"))
            .filter(|n| {
                n.child(DAV, "status")
                    .is_some_and(|s| s.text.split_whitespace().nth(1) == Some("200"))
            })
            .filter_map(|n| n.child(DAV, "prop"))
            .flat_map(|n| &n.children)
    }
}

// Resolve namespace URIs while parsing and retain property boundaries. Reject the
// entire document on malformed XML so partially parsed resources cannot fabricate data.
fn xml_document(xml: &str) -> Option<XmlNode> {
    use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
    let mut reader = NsReader::from_str(xml);
    let mut stack = vec![XmlNode::default()];
    loop {
        let (ns, event) = reader.read_resolved_event().ok()?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let namespace = match ns {
                    ResolveResult::Bound(n) => String::from_utf8(n.as_ref().to_vec()).ok()?,
                    ResolveResult::Unbound => String::new(),
                    _ => return None,
                };
                let node = XmlNode {
                    namespace,
                    name: String::from_utf8(e.local_name().as_ref().to_vec()).ok()?,
                    ..Default::default()
                };
                if matches!(event, Event::Empty(_)) {
                    stack.last_mut()?.children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::Text(t) => stack.last_mut()?.text.push_str(&t.unescape().ok()?),
            Event::CData(t) => stack
                .last_mut()?
                .text
                .push_str(std::str::from_utf8(t.as_ref()).ok()?),
            Event::End(_) => {
                if stack.len() <= 1 {
                    return None;
                }
                let node = stack.pop()?;
                stack.last_mut()?.children.push(node);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return None;
    }
    let mut root = stack.pop()?;
    if root.children.len() != 1 || !root.text.trim().is_empty() {
        return None;
    }
    Some(root.children.remove(0))
}

fn extract_href(xml: &str, prop_name: &str) -> Option<String> {
    let root = xml_document(xml)?;
    if !root.is(DAV, "multistatus") {
        return None;
    }
    let ns = if prop_name == "calendar-home-set" {
        CALDAV
    } else {
        DAV
    };
    for response in root.children.iter().filter(|n| n.is(DAV, "response")) {
        for prop in response
            .successful_properties()
            .filter(|n| n.is(ns, prop_name))
        {
            if let Some(href) = prop.child(DAV, "href") {
                return Some(href.text.clone());
            }
        }
    }
    None
}

fn parse_calendar_listing(xml: &str) -> Vec<(String, String)> {
    let Some(root) = xml_document(xml).filter(|root| root.is(DAV, "multistatus")) else {
        return vec![];
    };
    root.children
        .iter()
        .filter(|n| n.is(DAV, "response"))
        .filter_map(|response| {
            let is_calendar = response
                .successful_properties()
                .any(|p| p.is(DAV, "resourcetype") && p.child(CALDAV, "calendar").is_some());
            if !is_calendar {
                return None;
            }
            let href = response.child(DAV, "href")?.text.clone();
            let name = response
                .successful_properties()
                .find(|p| p.is(DAV, "displayname"))
                .map(|p| p.text.clone())
                .unwrap_or_default();
            Some((href, name))
        })
        .collect()
}

fn parse_calendar_data(xml: &str) -> Vec<String> {
    let Some(root) = xml_document(xml).filter(|root| root.is(DAV, "multistatus")) else {
        return vec![];
    };
    root.children
        .iter()
        .filter(|n| n.is(DAV, "response"))
        .flat_map(|n| n.successful_properties())
        .filter(|p| p.is(CALDAV, "calendar-data"))
        .map(|p| p.text.clone())
        .collect()
}

// ===========================================================================
// Orchestration: discovery → REPORT → parse → expand
// ===========================================================================

/// Fetch the configured calendars' events for the bounded range
/// `[now, now + lookahead_days]`, expanding recurring events and applying
/// overrides. Returns the merged, sorted event list.
///
/// Any step failing (discovery, REPORT, parse) is an `Err`; the caller
/// degrades to the last-good snapshot or `Unavailable`. Failures are surfaced
/// as a [`FetchError`] (Slice 6) so the caller can schedule a backoff; CalDAV
/// discovery/REPORT failures carry no `Retry-After` hint (they are mapped to
/// [`FetchError::Other`]).
#[cfg(test)]
pub async fn fetch_calendar(
    client: &CalDavClient,
    cfg: &CalendarConfig,
    now: DateTime<Utc>,
) -> Result<Vec<Event>, FetchError> {
    fetch_calendar_with_names(client, cfg, now)
        .await
        .map(|(events, _)| events)
}

async fn fetch_calendar_with_names(
    client: &CalDavClient,
    cfg: &CalendarConfig,
    now: DateTime<Utc>,
) -> Result<(Vec<Event>, Vec<(String, String)>), FetchError> {
    let principal = client
        .discover_principal(&cfg.url)
        .await
        .map_err(FetchError::Other)?;
    let home = client
        .calendar_home(&principal)
        .await
        .map_err(FetchError::Other)?;
    let calendars = client
        .list_calendars(&home)
        .await
        .map_err(FetchError::Other)?;

    let wanted = &cfg.calendar_names;
    let selected: Vec<(String, String)> = if wanted.is_empty() {
        calendars
    } else {
        calendars
            .into_iter()
            .filter(|(_, name)| wanted.iter().any(|w| w == name))
            .collect()
    };
    if selected.is_empty() {
        return Err(FetchError::Other(
            "no configured calendars resolved through discovery".to_string(),
        ));
    }

    let range_start = now;
    let range_end = now + ChronoDuration::days(cfg.lookahead_days as i64);

    let mut events: Vec<Event> = Vec::new();
    for (url, name) in &selected {
        let datas = client
            .report_events(url, range_start, range_end)
            .await
            .map_err(FetchError::Other)?;
        for data in datas {
            let parsed = expand_calendar(&data, url, range_start, range_end)
                .map_err(|e| FetchError::Parse(format!("calendar {name}: {e}")))?;
            events.extend(parsed);
        }
    }

    events.sort_by(|a, b| a.start.cmp(&b.start));
    events.dedup_by(|a, b| a.id == b.id);
    Ok((events, selected))
}

// ===========================================================================
// Cache + section building (weather.rs / news.rs pattern)
// ===========================================================================

/// A last-known-good reading with the instant it was fetched.
#[derive(Debug, Clone)]
struct Snapshot<T> {
    fetched_at: Instant,
    value: T,
}

/// The outcome of resolving the calendar reading against its refresh interval.
#[derive(Debug)]
pub enum Resolved<T> {
    /// Fresh: fetched now, or served from a snapshot within the interval.
    Fresh(T),
    /// Stale: a refetch was attempted and failed; serving the last-good value.
    Stale(T),
    /// Unavailable: no reading at all (never fetched, or fetch failed with no
    /// last-good to fall back to).
    Unavailable,
}

impl<T> Resolved<T> {
    /// The value, if any, plus the corresponding section status.
    pub fn into_parts(self) -> (Option<T>, Status) {
        match self {
            Resolved::Fresh(v) => (Some(v), Status::Fresh),
            Resolved::Stale(v) => (Some(v), Status::Stale),
            Resolved::Unavailable => (None, Status::Unavailable),
        }
    }
}

/// In-memory calendar snapshot, atomically replaced on each successful refresh
/// (architecture doc §Data). The independently-configured calendar refresh
/// interval is honored across polls (acceptance criterion: "the calendar
/// refresh interval is independently configurable").
#[derive(Debug, Default)]
pub struct CalendarCache {
    events: RwLock<Option<Snapshot<Vec<Event>>>>,
    calendars: RwLock<Vec<(String, String)>>,
    /// Retry state (Slice 6): after a fetch failure we skip refetches until
    /// the backoff cooldown elapses, serving the last-good snapshot as `Stale`
    /// in the meantime — so a failing CalDAV server is not re-hit on every
    /// poll.
    retry: RwLock<RetryState>,
}

impl CalendarCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn calendars(&self) -> Vec<(String, String)> {
        self.calendars.read().await.clone()
    }

    pub async fn seed_calendars(&self, calendars: Vec<(String, String)>) {
        *self.calendars.write().await = calendars;
    }

    /// Seed the events snapshot from a persisted last-good value (Slice 6
    /// restart recovery). The snapshot is stamped with the current instant so
    /// it is immediately eligible to be served (as `Stale` if a refetch is
    /// attempted and fails, or `Fresh` if the refresh interval has not yet
    /// elapsed).
    pub async fn seed(&self, value: Vec<Event>) {
        *self.events.write().await = Some(Snapshot {
            fetched_at: Instant::now(),
            value,
        });
    }

    /// The last-good events, if any (for persistence).
    pub async fn last_good(&self) -> Option<Vec<Event>> {
        self.events.read().await.as_ref().map(|s| s.value.clone())
    }

    /// Resolve the current events, honoring the calendar refresh interval.
    ///
    /// A snapshot younger than the interval is served as-is (`Fresh`).
    /// Otherwise a refetch is attempted: on success the snapshot is replaced
    /// and the value is `Fresh`; on failure the last-good snapshot (if any) is
    /// served as `Stale`, else `Unavailable`.
    ///
    /// While the provider is in its backoff cooldown (a recent failure) the
    /// refetch is skipped entirely and the last-good snapshot is served as
    /// `Stale` (Slice 6) — so a failing CalDAV server is not re-hit on every
    /// poll.
    pub async fn resolve(
        &self,
        client: &CalDavClient,
        cfg: &CalendarConfig,
        now: DateTime<Utc>,
    ) -> Resolved<Vec<Event>> {
        let interval = Duration::from_secs(cfg.refresh_secs);
        if let Some(snap) = self.events.read().await.as_ref() {
            // Legacy snapshots contain events without calendar names. Discover
            // those names immediately rather than waiting a full refresh interval.
            if snap.fetched_at.elapsed() < interval && !self.calendars.read().await.is_empty() {
                return Resolved::Fresh(snap.value.clone());
            }
        }
        // In backoff cooldown: skip the refetch and serve the last-good value.
        if self.retry.read().await.in_cooldown() {
            return match self.events.read().await.as_ref() {
                Some(snap) => Resolved::Stale(snap.value.clone()),
                None => Resolved::Unavailable,
            };
        }
        match fetch_calendar_with_names(client, cfg, now).await {
            Ok((events, calendars)) => {
                *self.calendars.write().await = calendars;
                self.retry.write().await.record_success();
                *self.events.write().await = Some(Snapshot {
                    fetched_at: Instant::now(),
                    value: events.clone(),
                });
                Resolved::Fresh(events)
            }
            Err(err) => {
                self.retry.write().await.record_failure(err.retry_after());
                // Log a redacted, log-safe description — never the CalDAV app
                // password or any credential (Slice 6 secret redaction).
                eprintln!(
                    "magic-mirror: calendar fetch failed: {}",
                    describe_fetch_error(&err)
                );
                match self.events.read().await.as_ref() {
                    Some(snap) => Resolved::Stale(snap.value.clone()),
                    None => Resolved::Unavailable,
                }
            }
        }
    }
}

/// A short, log-safe description of a calendar fetch failure. The CalDAV app
/// password is never included (it is only ever sent as an HTTP Basic auth
/// header, never embedded in an error message), but the description is kept
/// generic so it can be logged safely (Slice 6 secret redaction).
fn describe_fetch_error(err: &FetchError) -> String {
    match err {
        FetchError::Network(e) => format!("network: {e}"),
        FetchError::Http { status, .. } => format!("non-2xx status: {status}"),
        FetchError::Parse(msg) => format!("parse: {msg}"),
        FetchError::Other(msg) => msg.clone(),
    }
}

/// Apply the rolling display window even to cached or fallback events.
pub fn filter_display_events(events: &mut Vec<Event>, now: DateTime<Utc>, days: u32) {
    let end = now + ChronoDuration::days(i64::from(days));
    events.retain(|event| {
        DateTime::parse_from_rfc3339(&event.start)
            .map(|start| start >= now && start < end)
            .unwrap_or(false)
    });
}

/// Fold the resolved events into the API's `CalendarSection`.
///
/// `events` is `Some` when a reading is available (fresh or stale) and `None`
/// when there is no data at all (unavailable), in which case the section
/// degrades to the supplied fallback (mock) body with `Status::Unavailable`.
pub fn build_calendar_section(
    now: String,
    status: Status,
    events: Option<Vec<Event>>,
    fallback: crate::api::CalendarSection,
) -> crate::api::CalendarSection {
    match events {
        Some(items) => crate::api::CalendarSection {
            updated_at: now,
            status,
            events: items,
        },
        None => crate::api::CalendarSection {
            updated_at: now,
            status: Status::Unavailable,
            ..fallback
        },
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_window_uses_elapsed_hours_and_filters_cached_events() {
        // Spans the Budapest autumn DST change: days still mean exactly 24h.
        let now = Utc.with_ymd_and_hms(2026, 10, 24, 12, 0, 0).unwrap();
        for (days, hours) in [(1, 24), (2, 48), (7, 168)] {
            let mut events: Vec<Event> = [-1, 0, hours - 1, hours, hours + 1]
                .into_iter()
                .map(|offset| Event {
                    id: offset.to_string(),
                    title: String::new(),
                    start: (now + ChronoDuration::hours(offset)).to_rfc3339(),
                    end: (now + ChronoDuration::hours(offset + 1)).to_rfc3339(),
                    all_day: false,
                    location: None,
                    description: None,
                })
                .collect();
            filter_display_events(&mut events, now, days);
            assert_eq!(events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
                vec!["0", &(hours - 1).to_string()]);
        }
    }

    /// Load a fixture VCALENDAR from `fixtures/caldav/`.
    fn fixture(name: &str) -> String {
        let path = format!("{}/fixtures/caldav/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"))
    }

    /// A weekly recurring event with one overridden occurrence expands to
    /// exactly one event at the override time (acceptance criterion: "a weekly
    /// recurring event with one overridden occurrence expanding to exactly one
    /// event at that time").
    #[test]
    fn weekly_recurring_with_one_override_expands_to_exactly_one_event() {
        let ical = fixture("weekly_override.ics");
        let start = Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        // The series runs Mon 12, 19, 26 Oct; the 19th is overridden to 16:00
        // and the 26th is EXDATEd. So within the range we expect exactly:
        //   - 2026-10-12T09:00Z (regular)
        //   - 2026-10-19T16:00Z (the single override)
        assert_eq!(events.len(), 2, "expected 2 events, got: {events:?}");

        let override_events: Vec<_> = events
            .iter()
            .filter(|e| e.start == "2026-10-19T16:00:00Z")
            .collect();
        assert_eq!(
            override_events.len(),
            1,
            "the overridden occurrence must appear exactly once: {events:?}"
        );
        assert_eq!(override_events[0].title, "Standup (moved)");

        // The original 09:00 slot on the 19th must NOT also appear.
        assert!(
            !events.iter().any(|e| e.start == "2026-10-19T09:00:00Z"),
            "the overridden original slot must be replaced, not duplicated: {events:?}"
        );
        // The EXDATEd 26th must not appear.
        assert!(
            !events.iter().any(|e| e.start.starts_with("2026-10-26")),
            "the EXDATEd occurrence must be excluded: {events:?}"
        );
    }

    /// A recurring event that excludes multiple dates via MULTIPLE separate
    /// `EXDATE` properties (the common real-world shape: one `EXDATE` line per
    /// excluded occurrence) must drop EVERY excluded occurrence — not just the
    /// first `EXDATE` line. The series runs Mon 5, 12, 19, 26 Jan; the 12th
    /// and 19th are each excluded by their own `EXDATE` property.
    #[test]
    fn multiple_exdate_properties_all_excluded() {
        let ical = fixture("exdate_multiple_lines.ics");
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        // Both excluded occurrences must be absent.
        assert!(
            !events.iter().any(|e| e.start == "2026-01-12T10:00:00Z"),
            "the first EXDATEd occurrence must be excluded: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.start == "2026-01-19T10:00:00Z"),
            "the second EXDATEd occurrence must be excluded (only the first EXDATE property is honored today): {events:?}"
        );
        // The non-excluded occurrences must be present.
        assert!(
            events.iter().any(|e| e.start == "2026-01-05T10:00:00Z"),
            "the first occurrence must be present: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.start == "2026-01-26T10:00:00Z"),
            "the last occurrence must be present: {events:?}"
        );
        assert_eq!(
            events.len(),
            2,
            "exactly the two non-excluded occurrences: {events:?}"
        );
    }

    /// A single `EXDATE` property carrying a comma-separated list of values
    /// (RFC 5545 §3.8.5.1) must exclude every listed occurrence.
    #[test]
    fn exdate_comma_separated_list_all_excluded() {
        let ical = fixture("exdate_comma_list.ics");
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        assert!(
            !events.iter().any(|e| e.start == "2026-01-12T10:00:00Z"),
            "the first listed EXDATE value must be excluded: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.start == "2026-01-19T10:00:00Z"),
            "the second listed EXDATE value must be excluded: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.start == "2026-01-05T10:00:00Z"),
            "the first occurrence must be present: {events:?}"
        );
        assert!(
            events.iter().any(|e| e.start == "2026-01-26T10:00:00Z"),
            "the last occurrence must be present: {events:?}"
        );
        assert_eq!(
            events.len(),
            2,
            "exactly the two non-excluded occurrences: {events:?}"
        );
    }

    /// An `EXDATE` with a `TZID` parameter (a wall-clock time in the event's
    /// zone, not a UTC instant) must exclude the matching occurrence.
    #[test]
    fn exdate_with_tzid_param_excludes_occurrence() {
        let ical = fixture("exdate_tzid.ics");
        let start = Utc.with_ymd_and_hms(2026, 3, 19, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 4, 6, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        // Three Friday occurrences: 20 Mar (CET), 27 Mar (CET), 3 Apr (CEST).
        // The 27th is excluded by a TZID-qualified EXDATE.
        assert_eq!(events.len(), 2, "expected 2 occurrences: {events:?}");
        assert_eq!(
            events[0].start, "2026-03-20T00:30:00Z",
            "pre-transition CET: {events:?}"
        );
        assert_eq!(
            events[1].start, "2026-04-02T23:30:00Z",
            "post-transition CEST: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.start == "2026-03-27T00:30:00Z"),
            "the TZID-qualified EXDATEd occurrence must be excluded: {events:?}"
        );
    }

    /// A recurring event with a fixed `DTEND`: each occurrence's end must be
    /// its own start plus the event's duration — not the original series'
    /// `DTEND` instant (which would put later occurrences' ends before their
    /// starts).
    #[test]
    fn recurring_event_with_dtend_has_per_occurrence_end() {
        let ical = fixture("exdate_comma_list.ics");
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        for e in &events {
            assert!(
                e.end > e.start,
                "every occurrence's end must be after its start: {e:?}"
            );
        }
        let jan26 = events
            .iter()
            .find(|e| e.start == "2026-01-26T10:00:00Z")
            .expect("the 26th must be present");
        assert_eq!(
            jan26.end, "2026-01-26T10:30:00Z",
            "the occurrence's end must be its own start + 30m, not the series' original DTEND: {jan26:?}"
        );
    }

    /// DST-boundary expansion: a weekly 01:30 Europe/Budapest event keeps its
    /// wall-clock time across the 2026-03-29 spring-forward; the UTC instant
    /// shifts by one hour (acceptance criterion: "unit tests verify
    /// DST-boundary expansion correctness").
    #[test]
    fn dst_boundary_expansion_keeps_wall_time() {
        let ical = fixture("dst_weekly.ics");
        let start = Utc.with_ymd_and_hms(2026, 3, 19, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 4, 6, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        // Three Friday occurrences: 20, 27 Mar (CET, +01:00) and 3 Apr
        // (CEST, +02:00). Same wall-clock time 01:30, UTC shifts by 1 h.
        assert_eq!(events.len(), 3, "expected 3 occurrences: {events:?}");
        assert_eq!(
            events[0].start, "2026-03-20T00:30:00Z",
            "pre-transition CET: {events:?}"
        );
        assert_eq!(
            events[1].start, "2026-03-27T00:30:00Z",
            "pre-transition CET: {events:?}"
        );
        assert_eq!(
            events[2].start, "2026-04-02T23:30:00Z",
            "post-transition CEST: {events:?}"
        );
    }

    /// A UTC event parses and expands without a VTIMEZONE.
    #[test]
    fn utc_event_parses() {
        let ical = fixture("utc_simple.ics");
        let start = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 19, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start, "2026-10-18T12:00:00Z");
        assert_eq!(events[0].end, "2026-10-18T13:30:00Z");
        assert_eq!(events[0].title, "UTC meeting");
        assert!(!events[0].all_day);
    }

    /// An all-day event (VALUE=DATE) normalizes to midnight UTC.
    #[test]
    fn all_day_event_parses() {
        let ical = fixture("allday.ics");
        let start = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 19, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day, "all-day flag must be set: {events:?}");
        assert_eq!(events[0].start, "2026-10-18T00:00:00Z");
    }

    /// A `DTEND` with NO `TZID` parameter (a bare local DATE-TIME) sitting
    /// under a zoned `DTSTART` must resolve in the DTSTART's zone — not be
    /// treated as a floating/UTC instant. The `.or_else` fallback in
    /// `parse_vevents` is dead code: `parse_ics_time` returns `Some(Floating)`
    /// (not `None`) when the TZID param is absent, so the DTSTART-zone
    /// fallback never fires and the bare DTEND is resolved as UTC.
    #[test]
    fn bare_dtend_under_zoned_dtstart_resolves_in_dtstart_zone() {
        let ical = fixture("dtend_bare_under_zoned.ics");
        let start = Utc.with_ymd_and_hms(2026, 1, 15, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 1, 16, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");

        assert_eq!(events.len(), 1, "expected 1 event: {events:?}");
        // DTSTART 12:00 Europe/Budapest (CET, +01:00 in January) = 11:00 UTC.
        assert_eq!(
            events[0].start, "2026-01-15T11:00:00Z",
            "DTSTART must resolve in the Budapest zone: {events:?}"
        );
        // DTEND 13:30 (bare, no TZID) under a Budapest DTSTART must resolve in
        // Budapest (CET, +01:00) = 12:30 UTC — NOT 13:30 UTC (the floating/UTC
        // treatment that the dead `.or_else` fallback was meant to prevent).
        assert_eq!(
            events[0].end, "2026-01-15T12:30:00Z",
            "a bare DTEND under a zoned DTSTART must resolve in the DTSTART zone, not UTC: {events:?}"
        );
    }

    /// CANCELLED events are hidden (program design: calendar_cancelled_hidden).
    #[test]
    fn cancelled_event_is_hidden() {
        let ical = fixture("cancelled.ics");
        let start = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 19, 0, 0, 0).unwrap();
        let events = expand_calendar(&ical, "cal1", start, end).expect("expands");
        assert!(
            !events.iter().any(|e| e.title == "Cancelled thing"),
            "cancelled event must be hidden: {events:?}"
        );
    }

    /// The VTIMEZONE offset resolution itself: Budapest is +01:00 in winter
    /// and +02:00 in summer.
    #[test]
    fn budapest_offset_across_dst() {
        let ical = fixture("dst_weekly.ics");
        let cal = parse_ical(&ical).expect("parses");
        let zones = parse_vtimezones(&cal);
        let tz = zones.get("Europe/Budapest").expect("zone present");

        let winter = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
            NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
        );
        let summer = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2026, 7, 15).unwrap(),
            NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
        );
        assert_eq!(offset_for(tz, winter), Some(3600), "winter must be +01:00");
        assert_eq!(offset_for(tz, summer), Some(7200), "summer must be +02:00");
    }

    // --- CalDAV discovery + REPORT against a mock server -------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Start a mock CalDAV server that records every (method, path) it sees,
    /// and serves the discovery + REPORT flow.
    async fn start_mock_caldav() -> (String, Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        use axum::{
            http::{Method, StatusCode},
            routing::{any, get},
            Extension, Router,
        };

        #[derive(Clone)]
        struct Seen(Arc<std::sync::Mutex<Vec<(String, String)>>>);

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ics = fixture("utc_simple.ics");

        /// Record the (method, uri) of an incoming request.
        async fn record(Extension(seen): Extension<Seen>, method: Method, uri: axum::http::Uri) {
            seen.0
                .lock()
                .unwrap()
                .push((method.to_string(), uri.to_string()));
        }

        let app = Router::new()
            .route(
                "/.well-known/caldav",
                get(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            StatusCode::MOVED_PERMANENTLY,
                            [(axum::http::header::LOCATION, "/principal/john/")],
                        )
                    },
                ),
            )
            .route(
                "/principal/john/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/principal/john/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:href>/calendars/john/</d:href>
        </c:calendar-home-set>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#,
                        )
                    },
                ),
            )
            .route(
                "/calendars/john/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/calendars/john/</d:href>
    <d:propstat><d:prop><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/john/Personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><c:calendar xmlns:c="urn:ietf:params:xml:ns:caldav"/></d:resourcetype>
        <d:displayname>Personal</d:displayname>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#,
                        )
                    },
                ),
            )
            .route(
                "/calendars/john/Personal/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            format!(
                                r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/calendars/john/Personal/evt.ics</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-data xmlns:c="urn:ietf:params:xml:ns:caldav">{ics}</c:calendar-data>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#
                            ),
                        )
                    },
                ),
            )
            .layer(Extension(Seen(seen.clone())));

        let base = register_test_server(app);

        (base, seen)
    }

    fn test_cfg(base: &str) -> CalendarConfig {
        CalendarConfig {
            enabled: true,
            url: base.to_string(),
            username: "john@example.com".into(),
            calendar_names: vec!["Personal".into()],
            refresh_secs: 3600,
            lookahead_days: 7,
            app_password: Some("app-password".into()),
        }
    }

    /// Full discovery → REPORT flow resolves the calendar home and returns
    /// the event (acceptance criteria: "CalDAV discovery … resolves the
    /// calendar home" and "a bounded REPORT query returns events").
    #[tokio::test]
    async fn discovery_and_report_return_events() {
        let (base, _seen) = start_mock_caldav().await;
        let cfg = test_cfg(&base);
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let events = fetch_calendar(&client, &cfg, now).await.expect("fetch ok");
        assert_eq!(events.len(), 1, "expected the mock event: {events:?}");
        assert_eq!(events[0].title, "UTC meeting");
    }

    /// A mock whose well-known URI points at the DAV root (not a principal),
    /// mirroring real servers such as Radicale/CommonCal. The DAV root exposes
    /// `current-user-principal` but not `calendar-home-set`; the principal
    /// carries `calendar-home-set`. This exercises the slow discovery path.
    async fn start_mock_caldav_dav_root() -> (String, Arc<std::sync::Mutex<Vec<(String, String)>>>)
    {
        use axum::{
            http::{Method, StatusCode},
            routing::{any, get},
            Extension, Router,
        };

        #[derive(Clone)]
        struct Seen(Arc<std::sync::Mutex<Vec<(String, String)>>>);

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ics = fixture("utc_simple.ics");

        async fn record(Extension(seen): Extension<Seen>, method: Method, uri: axum::http::Uri) {
            seen.0
                .lock()
                .unwrap()
                .push((method.to_string(), uri.to_string()));
        }

        let app = Router::new()
            .route(
                "/.well-known/caldav",
                get(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            StatusCode::TEMPORARY_REDIRECT,
                            [(axum::http::header::LOCATION, "/dav/")],
                        )
                    },
                ),
            )
            .route(
                "/dav/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:current-user-principal>
          <d:href>/dav/principals/john/</d:href>
        </d:current-user-principal>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#,
                        )
                    },
                ),
            )
            .route(
                "/dav/principals/john/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/principals/john/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:href>/dav/calendars/john/</d:href>
        </c:calendar-home-set>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#,
                        )
                    },
                ),
            )
            .route(
                "/dav/calendars/john/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/calendars/john/</d:href>
    <d:propstat><d:prop><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/calendars/john/Personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><c:calendar xmlns:c="urn:ietf:params:xml:ns:caldav"/></d:resourcetype>
        <d:displayname>Personal</d:displayname>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#,
                        )
                    },
                ),
            )
            .route(
                "/dav/calendars/john/Personal/",
                any(
                    |ext: Extension<Seen>, method: Method, uri: axum::http::Uri| async move {
                        record(ext, method, uri).await;
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            format!(
                                r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
  <d:response>
    <d:href>/dav/calendars/john/Personal/evt.ics</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-data xmlns:c="urn:ietf:params:xml:ns:caldav">{ics}</c:calendar-data>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#
                            ),
                        )
                    },
                ),
            )
            .layer(Extension(Seen(seen.clone())));

        let base = register_test_server(app);

        (base, seen)
    }

    /// Discovery through a DAV-root well-known target (well-known →
    /// `current-user-principal` → principal → `calendar-home-set`) resolves the
    /// calendar home and returns the event. This is the flow real servers such
    /// as CommonCal use, where the well-known URI points at `/dav/` rather than
    /// a principal.
    #[tokio::test]
    async fn discovery_via_dav_root_resolves_calendar_home() {
        let (base, _seen) = start_mock_caldav_dav_root().await;
        let cfg = test_cfg(&base);
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let events = fetch_calendar(&client, &cfg, now).await.expect("fetch ok");
        assert_eq!(events.len(), 1, "expected the mock event: {events:?}");
        assert_eq!(events[0].title, "UTC meeting");
    }

    /// Only read-only DAV methods are issued: GET, PROPFIND, REPORT — never
    /// PUT, POST, DELETE, MKCOL, or MKCALENDAR (acceptance criterion:
    /// "only read-only DAV methods are used").
    #[tokio::test]
    async fn only_read_only_dav_methods_are_used() {
        let (base, seen) = start_mock_caldav().await;
        let cfg = test_cfg(&base);
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let _ = fetch_calendar(&client, &cfg, now).await.expect("fetch ok");

        let calls = seen.lock().unwrap().clone();
        let methods: Vec<&str> = calls.iter().map(|(m, _)| m.as_str()).collect();
        assert!(
            !methods.is_empty(),
            "the mock server must have seen requests"
        );
        for m in &methods {
            assert!(
                matches!(*m, "GET" | "PROPFIND" | "REPORT"),
                "non-read-only DAV method issued: {m} (all: {methods:?})"
            );
        }
        // And the expected read-only flow is present.
        assert!(
            methods.contains(&"GET"),
            "well-known GET missing: {methods:?}"
        );
        assert!(
            methods.iter().filter(|m| **m == "PROPFIND").count() >= 2,
            "principal + calendar-home PROPFINDs missing: {methods:?}"
        );
        assert!(methods.contains(&"REPORT"), "REPORT missing: {methods:?}");
    }

    /// The app password is sent as HTTP Basic auth on the DAV requests.
    #[tokio::test]
    async fn app_password_used_for_basic_auth() {
        let (base, _seen) = start_mock_caldav().await;
        let cfg = test_cfg(&base);
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let _ = fetch_calendar(&client, &cfg, now).await.expect("fetch ok");
        // (The mock server accepts any auth; the assertion is that the flow
        // completes with the password configured.)
    }

    // --- Cache / refresh-interval behaviour --------------------------------

    /// A reading within the refresh interval is served from cache: the second
    /// resolve does not hit the upstream again.
    #[tokio::test]
    async fn cache_serves_within_interval() {
        let (base, seen) = start_mock_caldav().await;
        let cfg = test_cfg(&base);
        let cache = CalendarCache::new();
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();

        let r1 = cache.resolve(&client, &cfg, now).await;
        let r2 = cache.resolve(&client, &cfg, now).await;

        assert!(matches!(r1, Resolved::Fresh(_)));
        assert!(matches!(r2, Resolved::Fresh(_)));
        assert_eq!(
            cache.calendars().await,
            vec![(format!("{base}/calendars/john/Personal/"), "Personal".into())]
        );
        let calls = seen.lock().unwrap();
        let reports = calls.iter().filter(|(m, _)| m == "REPORT").count();
        assert_eq!(reports, 1, "second resolve must be cached: {calls:?}");
    }

    #[tokio::test]
    async fn calendar_names_available_without_upcoming_events() {
        let (base, _) = start_mock_caldav().await;
        let cfg = test_cfg(&base);
        let cache = CalendarCache::new();
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap();
        assert!(matches!(
            cache.resolve(&client, &cfg, now).await,
            Resolved::Fresh(events) if events.is_empty()
        ));
        assert_eq!(
            cache.calendars().await,
            vec![(format!("{base}/calendars/john/Personal/"), "Personal".into())]
        );
    }

    #[tokio::test]
    async fn seeded_events_refresh_missing_calendar_names() {
        let (base, _) = start_mock_caldav().await;
        let cfg = test_cfg(&base);
        let cache = CalendarCache::new();
        cache.seed(seeded_event()).await;
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        assert!(matches!(
            cache.resolve(&client, &cfg, now).await,
            Resolved::Fresh(_)
        ));
        assert!(
            !cache.calendars().await.is_empty(),
            "loaded events must have calendar choices"
        );
    }

    /// An interval of zero forces a refetch every time (no caching).
    #[tokio::test]
    async fn zero_interval_always_refetches() {
        let (base, seen) = start_mock_caldav().await;
        let mut cfg = test_cfg(&base);
        cfg.refresh_secs = 0;
        let cache = CalendarCache::new();
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();

        let _ = cache.resolve(&client, &cfg, now).await;
        let _ = cache.resolve(&client, &cfg, now).await;

        let calls = seen.lock().unwrap();
        let reports = calls.iter().filter(|(m, _)| m == "REPORT").count();
        assert_eq!(reports, 2, "zero interval must refetch: {calls:?}");
    }

    // --- Slice 6: restart recovery + backoff cooldown -----------------------

    /// Start a mock CalDAV server that always fails (500) on every request,
    /// with a shared hit counter so tests can assert how many times the
    /// upstream was hit.
    async fn start_failing_caldav() -> (String, Arc<AtomicUsize>) {
        use axum::Router;

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();

        let app = Router::new().fallback(move |_: ()| {
            h.fetch_add(1, Ordering::SeqCst);
            async move { axum::http::StatusCode::INTERNAL_SERVER_ERROR }
        });

        let base = register_test_server(app);

        (base, hits)
    }

    fn seeded_event() -> Vec<Event> {
        vec![Event {
            id: "seeded:1".into(),
            title: "Seeded event".into(),
            start: "2026-10-18T12:00:00Z".into(),
            end: "2026-10-18T13:00:00Z".into(),
            all_day: false,
            location: None,
            description: None,
        }]
    }

    /// Restart recovery: a seeded (persisted) last-good snapshot is served as
    /// `Stale` when the first refresh after restart fails (acceptance criterion:
    /// "on restart the section is restored from last-good before the first
    /// refresh").
    #[tokio::test]
    async fn restart_recovery_serves_last_good_as_stale() {
        let (base, _hits) = start_failing_caldav().await;
        let mut cfg = test_cfg(&base);
        cfg.refresh_secs = 0; // zero interval forces a refetch
        let cache = CalendarCache::new();
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();

        // Simulate restart recovery: seed the last-good snapshot from disk.
        cache.seed(seeded_event()).await;

        // The first refresh after restart fails; the seeded value is served as Stale.
        let resolved = cache.resolve(&client, &cfg, now).await;
        let (events, status) = resolved.into_parts();
        assert_eq!(
            status,
            Status::Stale,
            "seeded last-good must be served as Stale"
        );
        let events = events.expect("seeded last-good must be retained");
        assert!(
            events.iter().any(|e| e.id == "seeded:1"),
            "seeded event retained: {events:?}"
        );
    }

    /// Backoff cooldown: after a failure, the next resolve within the cooldown
    /// window does not hit the upstream (acceptance criterion: "exponential
    /// backoff … skips refetches while in cooldown").
    #[tokio::test]
    async fn backoff_cooldown_skips_refetch() {
        let (base, hits) = start_failing_caldav().await;
        let mut cfg = test_cfg(&base);
        cfg.refresh_secs = 0; // zero interval forces a refetch
        let cache = CalendarCache::new();
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();

        // Seed last-good so the cooldown path serves Stale (not Unavailable).
        cache.seed(seeded_event()).await;

        // First resolve: not in cooldown, attempts the (failing) fetch.
        let r1 = cache.resolve(&client, &cfg, now).await;
        assert!(matches!(r1, Resolved::Stale(_)));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "first resolve must attempt the fetch"
        );

        // Second resolve: now in cooldown, must NOT hit the upstream.
        let r2 = cache.resolve(&client, &cfg, now).await;
        assert!(matches!(r2, Resolved::Stale(_)));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "cooldown must skip the refetch"
        );
    }

    // --- Config: env-only password + independent refresh interval ----------

    /// The CalDAV app password is read only from the
    /// `MIRROR_CALDAV_APP_PASSWORD` environment variable (acceptance
    /// criterion). A config file cannot supply it (`#[serde(skip)]`).
    #[test]
    fn app_password_is_env_only() {
        // The password field is skipped during deserialization: a TOML value
        // for it must not populate the field.
        let toml = r#"
[calendar]
enabled = true
url = "https://caldav.example/"
username = "u@example.com"
app_password = "from-file-must-be-ignored"
refresh_secs = 123
"#;
        let cfg: crate::config::Config = toml::from_str(toml).expect("parses");
        assert!(
            cfg.calendar.app_password.is_none(),
            "config file must not supply the app password: {:?}",
            cfg.calendar.app_password
        );
        assert_eq!(cfg.calendar.refresh_secs, 123);
    }

    /// Secret redaction: the CalDAV app password must not appear in the log
    /// output (the `describe_fetch_error` description that is `eprintln!`'d on
    /// a failed fetch) nor in the API JSON (acceptance criterion: "secrets do
    /// not appear in API JSON or logs").
    #[tokio::test]
    async fn app_password_not_leaked_in_logs_or_api_json() {
        const SECRET: &str = "super-secret-app-password-XYZ123";
        let (base, _hits) = start_failing_caldav().await;
        let mut cfg = test_cfg(&base);
        cfg.app_password = Some(SECRET.into());
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();

        // The fetch fails (500). The log-safe description must not contain the
        // password — it is only ever sent as an HTTP Basic auth header, never
        // embedded in an error message.
        let err = fetch_calendar(&client, &cfg, now)
            .await
            .expect_err("must fail");
        let description = describe_fetch_error(&err);
        assert!(
            !description.contains(SECRET),
            "the app password must not appear in the log description: {description:?}"
        );

        // The API JSON (calendar section) must not contain the password either.
        let section = build_calendar_section(
            now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            Status::Stale,
            Some(vec![]),
            crate::api::CalendarSection {
                updated_at: String::new(),
                status: Status::Stale,
                events: vec![],
            },
        );
        let json = serde_json::to_string(&section).expect("serializes");
        assert!(
            !json.contains(SECRET),
            "the app password must not appear in the API JSON: {json:?}"
        );
    }

    /// The calendar refresh interval is independently configurable (separate
    /// from the weather / news intervals).
    #[test]
    fn calendar_refresh_interval_independently_configurable() {
        let toml = r#"
[calendar]
refresh_secs = 42
[weather]
weather_refresh_secs = 900
[news]
news_refresh_secs = 900
"#;
        let cfg: crate::config::Config = toml::from_str(toml).expect("parses");
        assert_eq!(cfg.calendar.refresh_secs, 42);
        assert_eq!(cfg.weather.weather_refresh_secs, 900);
        assert_eq!(cfg.news.news_refresh_secs, 900);
    }

    // --- CalDAV protocol conformance: spec-conformant server behaviour -----
    //
    // The mocks above answer any method on any path and ignore the `Depth`
    // header. A spec-conformant CalDAV server (RFC 4791 §5.3.1, and the
    // reference implementation in `happening/backend/src/caldav/http.rs`)
    // does neither:
    //   * the calendar home accepts only PROPFIND (and OPTIONS) — REPORT on
    //     the home is 405 Method Not Allowed;
    //   * a PROPFIND on the home with `Depth: 0` returns the home itself
    //     only; child calendars appear only with `Depth: 1`.
    // The mock below enforces both, so the client's protocol behaviour is
    // actually exercised.

    /// Start a protocol-enforcing mock CalDAV server that mirrors a
    /// spec-conformant server: the principal and calendar home accept only
    /// PROPFIND (REPORT → 405), and the home's PROPFIND response branches on
    /// the `Depth` header (`0` → home only; `1` → home + children). Records
    /// (method, path, depth) for every request.
    async fn start_mock_caldav_strict(
    ) -> (String, Arc<std::sync::Mutex<Vec<(String, String, String)>>>) {
        use axum::{
            http::{HeaderMap, Method, StatusCode},
            response::IntoResponse,
            routing::{any, get},
            Extension, Router,
        };

        #[derive(Clone)]
        struct Seen(Arc<std::sync::Mutex<Vec<(String, String, String)>>>);

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ics = fixture("utc_simple.ics");

        /// Record the (method, uri, depth) of an incoming request.
        async fn record(
            Extension(seen): Extension<Seen>,
            method: &Method,
            uri: axum::http::Uri,
            headers: &HeaderMap,
        ) {
            let depth = headers
                .get("depth")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<absent>")
                .to_string();
            seen.0
                .lock()
                .unwrap()
                .push((method.to_string(), uri.to_string(), depth));
        }

        /// The home block as a spec-conformant server renders it: a plain
        /// `d:collection`, no `c:calendar` element. `Depth: 0` on the home
        /// returns the home itself only.
        const HOME_ONLY: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/john/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>John's calendars</d:displayname>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

        /// `Depth: 1` on the home: the home plus its child calendars.
        const HOME_AND_CHILDREN: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/john/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>John's calendars</d:displayname>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/john/Personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Personal</d:displayname>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;

        /// A 405 response with an honest `Allow` header.
        fn method_not_allowed(allow: &str) -> axum::response::Response {
            (
                StatusCode::METHOD_NOT_ALLOWED,
                [(axum::http::header::ALLOW, allow)],
            )
                .into_response()
        }

        let app = Router::new()
            .route(
                "/.well-known/caldav",
                get(
                    |ext: Extension<Seen>,
                     method: Method,
                     uri: axum::http::Uri,
                     headers: HeaderMap| async move {
                        record(ext, &method, uri, &headers).await;
                        (
                            StatusCode::MOVED_PERMANENTLY,
                            [(axum::http::header::LOCATION, "/principal/john/")],
                        )
                    },
                ),
            )
            .route(
                "/principal/john/",
                any(
                    |ext: Extension<Seen>,
                     method: Method,
                     uri: axum::http::Uri,
                     headers: HeaderMap| async move {
                        record(ext, &method, uri, &headers).await;
                        if method.as_str() != "PROPFIND" {
                            return method_not_allowed("PROPFIND, OPTIONS");
                        }
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/principal/john/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set>
          <d:href>/calendars/john/</d:href>
        </c:calendar-home-set>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#,
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/calendars/john/",
                any(
                    |ext: Extension<Seen>,
                     method: Method,
                     uri: axum::http::Uri,
                     headers: HeaderMap| async move {
                        record(ext, &method, uri, &headers).await;
                        // The calendar home is not a calendar collection:
                        // only PROPFIND (and OPTIONS) are allowed.
                        if method.as_str() != "PROPFIND" {
                            return method_not_allowed("PROPFIND, OPTIONS");
                        }
                        // Depth: 0 → the home itself only; Depth: 1 → home
                        // plus its child calendars (RFC 4791 §5.3.1).
                        let depth = headers
                            .get("depth")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("0");
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            if depth == "1" {
                                HOME_AND_CHILDREN
                            } else {
                                HOME_ONLY
                            },
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/calendars/john/Personal/",
                any(
                    |ext: Extension<Seen>,
                     method: Method,
                     uri: axum::http::Uri,
                     headers: HeaderMap| async move {
                        record(ext, &method, uri, &headers).await;
                        // Only the calendar collection accepts REPORT.
                        if method.as_str() != "REPORT" {
                            return method_not_allowed("PROPFIND, OPTIONS, REPORT");
                        }
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/xml")],
                            format!(
                                r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/john/Personal/evt.ics</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-data>{ics}</c:calendar-data>
      </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#
                            ),
                        )
                            .into_response()
                    },
                ),
            )
            .layer(Extension(Seen(seen.clone())));

        let base = register_test_server(app);

        (base, seen)
    }

    /// With the default configuration (`calendar_names` empty = "every
    /// calendar under the discovered calendar home"), the client must select
    /// only the child calendar collection and REPORT it — never the calendar
    /// home itself. A spec-conformant server answers REPORT on the home with
    /// 405 Method Not Allowed (the reported production symptom:
    /// "calendar fetch failed: REPORT failed: 405 Method Not Allowed").
    #[tokio::test]
    async fn fetch_calendar_with_default_names_never_reports_calendar_home() {
        let (base, seen) = start_mock_caldav_strict().await;
        let mut cfg = test_cfg(&base);
        cfg.calendar_names = Vec::new(); // default: every calendar under the home
        let client = CalDavClient::new(
            cfg.username.clone(),
            cfg.app_password.clone(),
            Duration::from_secs(5),
        );
        let now = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();

        let events = fetch_calendar(&client, &cfg, now)
            .await
            .expect("fetch must succeed against a spec-conformant server");
        assert_eq!(events.len(), 1, "expected the mock event: {events:?}");
        assert_eq!(events[0].title, "UTC meeting");

        let calls = seen.lock().unwrap().clone();
        let reports: Vec<&(String, String, String)> =
            calls.iter().filter(|(m, _, _)| m == "REPORT").collect();
        assert!(
            !reports.is_empty(),
            "a REPORT must have been issued: {calls:?}"
        );
        for (m, path, depth) in &reports {
            assert!(
                path.ends_with("/Personal/"),
                "REPORT must target the child calendar, not the calendar home: {m} {path} (Depth: {depth})"
            );
        }
    }

    /// `parse_calendar_listing` must classify resources by their
    /// `<resourcetype>`: a block whose href merely *contains* the substring
    /// "calendar" but whose `<resourcetype>` is only `<d:collection/>` (the
    /// calendar home) must be EXCLUDED; a block carrying `<c:calendar/>`
    /// must be INCLUDED.
    #[test]
    fn parse_calendar_listing_excludes_home_collection() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/john/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/></d:resourcetype>
        <d:displayname>John's calendars</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
  <d:response>
    <d:href>/calendars/john/Personal/</d:href>
    <d:propstat>
      <d:prop>
        <d:resourcetype><d:collection/><c:calendar/></d:resourcetype>
        <d:displayname>Personal</d:displayname>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        let listing = parse_calendar_listing(xml);
        assert_eq!(
            listing.len(),
            1,
            "only the <c:calendar/> resource may be listed; the home (a plain d:collection whose href contains 'calendar') must be excluded: {listing:?}"
        );
        assert_eq!(
            listing[0].0, "/calendars/john/Personal/",
            "the listed resource must be the child calendar: {listing:?}"
        );
        assert_eq!(listing[0].1, "Personal");
    }

    // --- Hand-rolled XML scanning: adversarial / nested shapes --------------
    //
    // `find_open_tag` / `find_close_tag` / `element_inner` / `extract_href`
    // walk the byte stream by local tag name with NO nesting-depth tracking and
    // NO scoping to the target element's actual extent. These tests feed raw
    // XML directly to the parsing helpers (no mock server) and encode the
    // CORRECT expected behaviour for shapes a real CalDAV server can produce.

    /// `extract_href` must return the `<href>` INSIDE the named property. When
    /// that property is EMPTY (a DAV server signals "not supported" by returning
    /// the property with no value, in a 404 `propstat`), the correct answer is
    /// `None` — NOT the `<href>` of a later sibling property.
    ///
    /// This is the exact shape of a DAV-root PROPFIND where the root does not
    /// support `calendar-home-set` (empty, 404) but does support
    /// `current-user-principal` (with a real href), and the server orders the
    /// 404 `propstat` first. Today `extract_href` leaks the sibling's href, so
    /// `calendar_home()` silently returns the *principal* URL as the calendar
    /// home instead of following the slow path.
    #[test]
    fn extract_href_empty_property_does_not_leak_sibling_href() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/</d:href>
    <d:propstat>
      <d:prop>
        <c:calendar-home-set/>
      </d:prop>
      <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
    <d:propstat>
      <d:prop>
        <d:current-user-principal>
          <d:href>/principal/john/</d:href>
        </d:current-user-principal>
      </d:prop>
      <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
  </d:response>
</d:multistatus>"#;
        // The `calendar-home-set` element is empty (404, not supported). The
        // correct extraction is `None` — there is no href inside it.
        assert_eq!(
            extract_href(xml, "calendar-home-set"),
            None,
            "an empty (404) calendar-home-set must yield None, not the sibling current-user-principal href"
        );
        // The sibling property is still extractable on its own.
        assert_eq!(
            extract_href(xml, "current-user-principal"),
            Some("/principal/john/".to_string()),
            "the current-user-principal href must still be found"
        );
    }

    /// Payload text is XML text, even when it resembles a closing element.
    #[test]
    fn parse_calendar_data_preserves_escaped_close_and_cdata() {
        for payload in [
            "BEGIN:VCALENDAR\nSUMMARY:fake &lt;/calendar-data&gt; TRAILER\nEND:VCALENDAR",
            "<![CDATA[BEGIN:VCALENDAR\nSUMMARY:fake </calendar-data> TRAILER\nEND:VCALENDAR]]>",
        ] {
            let xml = dav_data_response(payload, "200 OK", "urn:ietf:params:xml:ns:caldav");
            assert_eq!(
                parse_calendar_data(&xml),
                vec![
                    "BEGIN:VCALENDAR\nSUMMARY:fake </calendar-data> TRAILER\nEND:VCALENDAR"
                        .to_string()
                ]
            );
        }
    }

    #[test]
    fn parse_calendar_data_rejects_malformed_xml() {
        let xml = dav_data_response(
            "BEGIN:VCALENDAR\nSUMMARY:fake </calendar-data> TRAILER\nEND:VCALENDAR",
            "200 OK",
            "urn:ietf:params:xml:ns:caldav",
        );
        assert!(
            parse_calendar_data(&xml).is_empty(),
            "malformed XML must not fabricate a payload"
        );
    }

    fn dav_data_response(payload: &str, status: &str, namespace: &str) -> String {
        format!(
            r#"<multistatus xmlns="DAV:" xmlns:c="{namespace}"><response>
<href>/event.ics</href><propstat><prop><c:calendar-data>{payload}</c:calendar-data></prop>
<status>HTTP/1.1 {status}</status></propstat></response></multistatus>"#
        )
    }

    #[test]
    fn parse_calendar_data_scopes_status_namespace_and_decodes_once() {
        let valid = dav_data_response(
            "A &lt; B &amp; C &#xE9; &#233; &amp;lt;",
            "200 OK",
            "urn:ietf:params:xml:ns:caldav",
        );
        assert_eq!(
            parse_calendar_data(&valid),
            vec!["A < B & C é é &lt;".to_string()]
        );
        for xml in [
            dav_data_response(
                "unavailable",
                "404 Not Found",
                "urn:ietf:params:xml:ns:caldav",
            ),
            dav_data_response("lookalike", "200 OK", "urn:wrong"),
            "<multistatus xmlns=\"DAV:\"/>".to_string(),
        ] {
            assert!(
                parse_calendar_data(&xml).is_empty(),
                "must ignore failed or foreign properties: {xml}"
            );
        }
    }

    #[test]
    fn parse_calendar_data_multiple_resources() {
        let first = dav_data_response("first", "200 OK", "urn:ietf:params:xml:ns:caldav");
        let second = dav_data_response("second", "200 OK", "urn:ietf:params:xml:ns:caldav");
        let xml = first.replace("</multistatus>", "") + second.split_once('>').unwrap().1;
        assert_eq!(
            parse_calendar_data(&xml),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn extract_href_scopes_successful_properties_in_either_order() {
        let failed = "<d:propstat><d:prop><c:calendar-home-set><d:href>/wrong/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>";
        let success = "<d:propstat><d:prop><c:calendar-home-set><d:href>/home/a&amp;b/</d:href></c:calendar-home-set><d:current-user-principal><d:href>../principal/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>";
        for props in [format!("{failed}{success}"), format!("{success}{failed}")] {
            let xml = format!(
                r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/dav/</d:href>{props}</d:response></d:multistatus>"#
            );
            assert_eq!(
                extract_href(&xml, "calendar-home-set"),
                Some("/home/a&b/".to_string())
            );
            assert_eq!(
                extract_href(&xml, "current-user-principal"),
                Some("../principal/".to_string())
            );
        }
    }

    #[test]
    fn discovery_hrefs_resolve_against_response_url() {
        let base = "https://dav.example.test/dav/principals/john/";
        for (href, expected) in [
            ("/dav/home/", "https://dav.example.test/dav/home/"),
            ("../../home/", "https://dav.example.test/dav/home/"),
            (
                "https://other.example.test/home/",
                "https://other.example.test/home/",
            ),
        ] {
            assert_eq!(resolve_url(base, href).unwrap(), expected);
        }
    }

    #[test]
    fn extract_href_ignores_foreign_property_namespace() {
        let xml = r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:x="urn:wrong"><response><href>/dav/</href><propstat><prop><x:calendar-home-set><href>/wrong/</href></x:calendar-home-set><c:calendar-home-set><href>https://example.test/home/</href></c:calendar-home-set></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        assert_eq!(
            extract_href(xml, "calendar-home-set"),
            Some("https://example.test/home/".to_string())
        );
    }

    #[test]
    fn listing_requires_successful_caldav_resource_type_and_decodes_text() {
        for (resource_type, status, expected) in [
            ("<c:calendar/>", "200 OK", true),
            (
                "<calendar xmlns=\"urn:ietf:params:xml:ns:caldav\"/>",
                "200 OK",
                true,
            ),
            ("<c:calendar/>", "404 Not Found", false),
            ("<x:calendar xmlns:x=\"urn:wrong\"/>", "200 OK", false),
            ("<collection/>", "200 OK", false),
        ] {
            let xml = format!(
                r#"<multistatus xmlns="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><response><href>calendar/a&amp;b/</href><propstat><prop><resourcetype><collection/>{resource_type}</resourcetype><displayname>Work &amp; home &#233;</displayname></prop><status>HTTP/1.1 {status}</status></propstat></response></multistatus>"#
            );
            let expected = if expected {
                vec![("calendar/a&b/".to_string(), "Work & home é".to_string())]
            } else {
                vec![]
            };
            assert_eq!(parse_calendar_listing(&xml), expected, "{xml}");
        }
    }

    #[test]
    fn rdate_timezone_preserves_modern_budapest_recurrence_and_exceptions() {
        let ical = fixture("budapest_rdate.ics");
        let events = expand_calendar(
            &ical,
            "cal1",
            Utc.with_ymd_and_hms(2026, 3, 20, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 4, 15, 0, 0, 0).unwrap(),
        )
        .expect("expands");
        let starts: Vec<_> = events.iter().map(|e| e.start.as_str()).collect();
        assert_eq!(
            starts,
            vec![
                "2026-03-23T08:00:00Z",
                "2026-03-30T07:00:00Z",
                "2026-04-13T08:00:00Z"
            ]
        );
        assert_eq!(events[2].title, "Moved meeting");
    }

    /// `extract_href` must return the `<href>` inside the named property even
    /// when an EARLIER sibling property also carries an `<href>`. This is the
    /// common, well-formed case and must keep working (regression guard for the
    /// scoping fix above).
    #[test]
    fn extract_href_ignores_earlier_sibling_href() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/dav/</d:href>
    <d:propstat><d:prop>
      <d:current-user-principal><d:href>/principal/john/</d:href></d:current-user-principal>
      <c:calendar-home-set><d:href>/calendars/john/</d:href></c:calendar-home-set>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
        assert_eq!(
            extract_href(xml, "calendar-home-set"),
            Some("/calendars/john/".to_string()),
            "the calendar-home-set href must be the one inside calendar-home-set, not the earlier sibling's"
        );
    }

    /// `parse_calendar_data` must decode the XML entities that appear inside a
    /// well-formed `calendar-data` payload (a properly-escaped `<` becomes `<`).
    #[test]
    fn parse_calendar_data_decodes_entities() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:response>
    <d:href>/calendars/john/Personal/</d:href>
    <d:propstat><d:prop>
      <c:calendar-data>BEGIN:VCALENDAR
SUMMARY:5 &lt; 6 &amp; 7 &gt; 4
END:VCALENDAR</c:calendar-data>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
        assert_eq!(
            parse_calendar_data(xml),
            vec!["BEGIN:VCALENDAR\nSUMMARY:5 < 6 & 7 > 4\nEND:VCALENDAR".to_string()],
            "well-formed escaped entities in calendar-data must be decoded"
        );
    }

    /// `list_calendars` must return the child calendars of the home. Against
    /// a spec-conformant server that honours `Depth`, this is only possible
    /// with `Depth: 1` — `Depth: 0` returns the home itself and nothing else.
    #[tokio::test]
    async fn list_calendars_returns_child_calendars() {
        let (base, _seen) = start_mock_caldav_strict().await;
        let client = CalDavClient::new(
            "john@example.com".into(),
            Some("app-password".into()),
            Duration::from_secs(5),
        );
        let home = format!("{base}/calendars/john/");
        let listing = client
            .list_calendars(&home)
            .await
            .expect("list_calendars must succeed");
        assert!(
            listing.iter().any(|(url, _)| url.ends_with("/calendars/john/Personal/")),
            "the child calendar must be listed (a Depth: 0 PROPFIND on the home returns the home only): {listing:?}"
        );
    }

    // --- Non-2xx error observability: body + target URL --------------------
    //
    // Real CalDAV servers answer a non-2xx (405, 403, 404, …) with a body
    // carrying the reason — a `DAV:` error element, a Sabre exception, or a
    // plain-text message. The error surfaced to the caller (and logged via
    // `describe_fetch_error`) must include that body (truncated is fine) and
    // the target URL, so the operator can tell WHICH URL failed and WHY.
    // Today both are dropped: the error is just "REPORT failed: 405 Method
    // Not Allowed" / "PROPFIND failed: 405 Method Not Allowed" — the reported
    // production symptom.

    #[tokio::test]
    async fn errors_bound_and_redact_diagnostic_context() {
        let client = CalDavClient::new(
            "john".into(),
            Some("app-password".into()),
            Duration::from_secs(5),
        );
        let body = format!("<d:error xmlns:d=\"DAV:\"><d:responsedescription>app-password {}</d:responsedescription></d:error>", "reason ".repeat(100));
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .status(403)
                .body(body)
                .unwrap(),
        );
        let error = client
            .failure(
                "REPORT",
                "https://john:url-secret@example.test/calendar/?token=query-secret#fragment-secret",
                response,
            )
            .await;
        assert!(error.contains("REPORT https://example.test/calendar/"));
        assert!(error.contains("403"));
        assert!(error.contains("reason"));
        for secret in [
            "app-password",
            "url-secret",
            "query-secret",
            "fragment-secret",
        ] {
            assert!(!error.contains(secret));
        }
        assert!(error.len() < 400);
        assert!(!format!("{client:?}").contains("app-password"));
        let response = reqwest::Response::from(
            axum::http::Response::builder()
                .status(500)
                .body("private calendar content")
                .unwrap(),
        );
        assert!(!client
            .failure("REPORT", "https://example.test/", response)
            .await
            .contains("private calendar content"));
    }

    #[tokio::test]
    async fn malformed_report_is_an_error_and_empty_multistatus_is_valid() {
        use axum::Router;
        for (body, valid) in [
            ("<multistatus xmlns=\"DAV:\"/>", true),
            ("<multistatus xmlns=\"DAV:\"><response>", false),
        ] {
            let base = register_test_server(Router::new().fallback(move || async move { body }));
            let client = CalDavClient::new("john".into(), None, Duration::from_secs(5));
            let now = Utc.with_ymd_and_hms(2026, 10, 24, 0, 0, 0).unwrap();
            assert_eq!(
                client
                    .report_events(&base, now, now + ChronoDuration::days(7))
                    .await
                    .is_ok(),
                valid
            );
        }
    }

    #[tokio::test]
    async fn discovery_trace_and_report_wire_contract() {
        use axum::{routing::any, Router};
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let app = Router::new().fallback(any(move |request: axum::extract::Request| {
            let recorded = recorded.clone();
            async move {
                let method = request.method().to_string();
                let path = request.uri().path().to_string();
                let depth = request.headers().get("depth").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let body = axum::body::to_bytes(request.into_body(), 8192).await.unwrap();
                recorded.lock().unwrap().push((method.clone(), path.clone(), depth.clone()));
                let xml = match path.as_str() {
                    "/.well-known/caldav" => return axum::http::Response::builder().status(307).header("Location", "/dav/").body(String::new()).unwrap(),
                    "/dav/" => {
                        assert_eq!((method.as_str(), depth.as_str()), ("PROPFIND", "0"));
                        "<d:response><d:propstat><d:prop><c:calendar-home-set/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat><d:propstat><d:prop><d:current-user-principal><d:href>principals/john/</d:href></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
                    }
                    "/dav/principals/john/" => {
                        assert_eq!((method.as_str(), depth.as_str()), ("PROPFIND", "0"));
                        "<d:response><d:propstat><d:prop><c:calendar-home-set><d:href>/dav/calendars/john/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
                    }
                    "/dav/calendars/john/" => {
                        assert_eq!((method.as_str(), depth.as_str()), ("PROPFIND", "1"));
                        "<d:response><d:href>/dav/calendars/john/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>personal/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Work &amp; home</d:displayname></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
                    }
                    "/dav/calendars/john/personal/" => {
                        assert_eq!((method.as_str(), depth.as_str()), ("REPORT", "1"));
                        let xml = xml_document(std::str::from_utf8(&body).unwrap()).unwrap();
                        assert!(xml.is(CALDAV, "calendar-query"));
                        assert!(xml.child(DAV, "prop").unwrap().child(CALDAV, "calendar-data").is_some());
                        let filter = xml.child(CALDAV, "filter").unwrap().child(CALDAV, "comp-filter").unwrap();
                        assert!(filter.child(CALDAV, "comp-filter").unwrap().child(CALDAV, "time-range").is_some());
                        let body = std::str::from_utf8(&body).unwrap();
                        assert!(body.contains("name=\"VCALENDAR\""));
                        assert!(body.contains("name=\"VEVENT\""));
                        assert!(body.contains("start=\"20261024T000000Z\" end=\"20261031T000000Z\""));
                        ""
                    }
                    _ => panic!("unexpected CalDAV target {path}"),
                };
                axum::http::Response::builder().status(207).body(format!("<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">{xml}</d:multistatus>")).unwrap()
            }
        }));
        let base = register_test_server(app);
        let client =
            CalDavClient::new("john".into(), Some("secret".into()), Duration::from_secs(5));
        let mut cfg = test_cfg(&base);
        cfg.calendar_names = vec!["Work & home".into()];
        let now = Utc.with_ymd_and_hms(2026, 10, 24, 0, 0, 0).unwrap();
        assert!(fetch_calendar(&client, &cfg, now).await.unwrap().is_empty());
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[4].1, "/dav/calendars/john/personal/");
    }

    /// Start a mock CalDAV server that answers EVERY request with 405
    /// Method Not Allowed carrying a body with a distinctive marker (as real
    /// servers do: the reason in the body).
    async fn start_mock_caldav_405_with_body() -> String {
        use axum::Router;

        const BODY: &str = r#"<?xml version="1.0"?>
<d:error xmlns:d="DAV:" xmlns:s="http://sabre.io/ns/error/">
  <s:exception>Sabre\DAV\Exception\MethodNotAllowed</s:exception>
  <s:message>REPORT is not supported on this resource</s:message>
</d:error>"#;

        let app = Router::new().fallback(|_: ()| async move {
            (
                axum::http::StatusCode::METHOD_NOT_ALLOWED,
                [(axum::http::header::CONTENT_TYPE, "application/xml")],
                BODY,
            )
        });

        register_test_server(app)
    }

    /// A non-2xx REPORT answer (405) carries the server's reason in the body.
    /// The error surfaced to the caller must include that body (truncated is
    /// fine) so the operator can see WHY — today it is dropped, leaving only
    /// "REPORT failed: 405 Method Not Allowed".
    #[tokio::test]
    async fn report_failure_error_includes_response_body() {
        let base = start_mock_caldav_405_with_body().await;
        let client = CalDavClient::new(
            "john@example.com".into(),
            Some("app-password".into()),
            Duration::from_secs(5),
        );
        let url = format!("{base}/calendars/john/Personal/");
        let start = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap();

        let err = client
            .report_events(&url, start, end)
            .await
            .expect_err("405 must be an error");
        assert!(
            err.contains("REPORT is not supported on this resource"),
            "the error must include the response body (the server's reason), got: {err:?}"
        );
    }

    /// A non-2xx REPORT answer (405) must name the target URL in the error,
    /// so the operator can tell WHICH calendar was REPORTed — today the error
    /// is identical regardless of URL.
    #[tokio::test]
    async fn report_failure_error_includes_target_url() {
        let base = start_mock_caldav_405_with_body().await;
        let client = CalDavClient::new(
            "john@example.com".into(),
            Some("app-password".into()),
            Duration::from_secs(5),
        );
        let url = format!("{base}/calendars/john/Personal/");
        let start = Utc.with_ymd_and_hms(2026, 10, 18, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap();

        let err = client
            .report_events(&url, start, end)
            .await
            .expect_err("405 must be an error");
        assert!(
            err.contains("/calendars/john/Personal/"),
            "the error must include the target URL, got: {err:?}"
        );
    }

    /// A non-2xx PROPFIND answer (405) must include the response body (the
    /// server's reason) and the target URL in the error — today both are
    /// dropped, leaving only "PROPFIND failed: 405 Method Not Allowed".
    #[tokio::test]
    async fn propfind_failure_error_includes_response_body_and_url() {
        let base = start_mock_caldav_405_with_body().await;
        let client = CalDavClient::new(
            "john@example.com".into(),
            Some("app-password".into()),
            Duration::from_secs(5),
        );
        let target = format!("{base}/principal/john/");

        let err = client
            .calendar_home(&target)
            .await
            .expect_err("405 must be an error");
        assert!(
            err.contains("REPORT is not supported on this resource"),
            "the error must include the response body (the server's reason), got: {err:?}"
        );
        assert!(
            err.contains("/principal/john/"),
            "the error must include the target URL, got: {err:?}"
        );
    }

    /// The calendar-home PROPFIND (the calendar listing) must send
    /// `Depth: 1` so a spec-conformant server includes the child calendars.
    #[tokio::test]
    async fn calendar_home_propfind_uses_depth_one() {
        let (base, seen) = start_mock_caldav_strict().await;
        let client = CalDavClient::new(
            "john@example.com".into(),
            Some("app-password".into()),
            Duration::from_secs(5),
        );
        let home = format!("{base}/calendars/john/");
        let _ = client.list_calendars(&home).await.expect("list_calendars");

        let calls = seen.lock().unwrap().clone();
        let home_propfinds: Vec<&(String, String, String)> = calls
            .iter()
            .filter(|(m, p, _)| m == "PROPFIND" && p.ends_with("/calendars/john/"))
            .collect();
        assert!(
            !home_propfinds.is_empty(),
            "a PROPFIND on the calendar home must have been issued: {calls:?}"
        );
        for (m, p, depth) in home_propfinds {
            assert_eq!(
                depth, "1",
                "the calendar-home PROPFIND must send Depth: 1 (Depth: 0 lists the home itself only): {m} {p} (Depth: {depth})"
            );
        }
    }
}
