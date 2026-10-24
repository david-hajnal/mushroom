//! Transit integration: BKK FUTÁR arrivals-and-departures (Slice 4).
//!
//! The transit section is populated from the BKK FUTÁR
//! `arrivals-and-departures-for-stop` endpoint for the configured stops. The
//! adapter is strictly **read-only**: it issues only `GET` requests. The BKK
//! API key is read only from the `MIRROR_BKK_API_KEY` environment variable
//! (see `config.rs`); it is never read from the config file and never appears
//! in API JSON or logs.
//!
//! Each departure is mapped to the API's `Departure` shape (the frontend
//! contract stays stable). The `realtime` flag distinguishes a live departure
//! (a `predictedDepartureTime` was supplied → "ON TIME" / delayed / early) from
//! a schedule-only departure (no predicted time → "SCHEDULED", `delay_seconds`
//! 0). Departures are filtered by the configured routes and by a walking-time
//! horizon (only departures within `walking_minutes` of now are shown).
//!
//! This mirrors the provider pattern established in `weather.rs` (Slice 2),
//! `news.rs` (Slice 3), and `calendar.rs` (Slice 5): a `*Cache` struct with an
//! `RwLock<Option<Snapshot<T>>>`, a `Resolved<T>` enum (`Fresh`/`Stale`/
//! `Unavailable`) with `into_parts()`, and a `build_*_section()` function.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::api::{Departure, Status, TransitSection};
use crate::backoff::{retry_after_from_response, FetchError, RetryState};
use crate::config::TransitConfig;

// ===========================================================================
// BKK FUTÁR wire types (a minimal subset of the OTP `where` response)
// ===========================================================================

/// The top-level FUTÁR response: everything is wrapped in `data`.
#[derive(Debug, Deserialize)]
struct FutarResponse {
    data: FutarData,
}

#[derive(Debug, Deserialize)]
struct FutarData {
    entry: FutarEntry,
    references: FutarReferences,
}

#[derive(Debug, Deserialize)]
struct FutarEntry {
    #[serde(rename = "stopTimes")]
    stop_times: Vec<FutarStopTime>,
}

/// A single stop time (one upcoming departure at the stop). Times are epoch
/// seconds. `predictedDepartureTime` is present only when live (realtime) data
/// is available.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FutarStopTime {
    stop_id: String,
    stop_headsign: String,
    departure_time: i64,
    #[serde(default)]
    predicted_departure_time: Option<i64>,
    trip_id: String,
}

#[derive(Debug, Deserialize)]
struct FutarReferences {
    #[serde(default)]
    routes: HashMap<String, FutarRoute>,
    #[serde(default)]
    trips: HashMap<String, FutarTrip>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FutarRoute {
    short_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FutarTrip {
    route_id: String,
}

// ===========================================================================
// Parsing + mapping to the API's `Departure` shape
// ===========================================================================

/// Format an epoch-seconds instant as an RFC 3339 `...Z` timestamp (second
/// precision), matching the spec §5 examples.
fn format_rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// Parse an RFC 3339 timestamp back to epoch seconds.
fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

/// Map a single FUTÁR stop time to the API's `Departure` shape, resolving the
/// route short-name through the `references` block.
///
/// Returns `None` when the trip or its route cannot be resolved (a defensive
/// skip so a malformed reference never panics or yields a blank route).
fn map_stop_time(
    st: &FutarStopTime,
    references: &FutarReferences,
) -> Option<Departure> {
    let route_id = references.trips.get(&st.trip_id)?.route_id.clone();
    let route = references.routes.get(&route_id)?.short_name.clone();

    let scheduled = st.departure_time;
    let predicted = st.predicted_departure_time;
    let realtime = predicted.is_some();
    let delay_seconds = predicted.unwrap_or(scheduled) - scheduled;

    Some(Departure {
        stop_id: st.stop_id.clone(),
        route,
        destination: st.stop_headsign.clone(),
        scheduled_time: format_rfc3339(scheduled),
        predicted_time: predicted.map(format_rfc3339),
        realtime,
        delay_seconds,
        platform: None,
    })
}

/// Parse a FUTÁR `arrivals-and-departures-for-stop` response body into the API's
/// `Departure` list.
///
/// Malformed JSON is a `FetchError::Parse`. Individual stop times whose trip or
/// route cannot be resolved are skipped rather than failing the whole stop.
pub fn parse_futar(body: &str) -> Result<Vec<Departure>, FetchError> {
    let resp: FutarResponse =
        serde_json::from_str(body).map_err(|e| FetchError::Parse(e.to_string()))?;
    let references = &resp.data.references;
    let mut out: Vec<Departure> = Vec::with_capacity(resp.data.entry.stop_times.len());
    for st in &resp.data.entry.stop_times {
        if let Some(dep) = map_stop_time(st, references) {
            out.push(dep);
        }
    }
    Ok(out)
}

/// Apply the route and walking-time filters to a list of departures.
///
/// - **Route filter**: when `routes` is non-empty, keep only departures whose
///   `route` is in the list. An empty list keeps every route.
/// - **Walking-time filter**: keep only departures whose effective time
///   (predicted, else scheduled) falls within `[now_secs, now_secs +
///   walking_minutes * 60]`. Departures already in the past are dropped.
///
/// The input order (FUTÁR returns departures time-ordered) is preserved.
pub fn filter_departures(
    deps: Vec<Departure>,
    routes: &[String],
    walking_minutes: u32,
    now_secs: i64,
) -> Vec<Departure> {
    let horizon = now_secs + (walking_minutes as i64) * 60;
    deps.into_iter()
        .filter(|d| {
            if !routes.is_empty() && !routes.iter().any(|r| r == &d.route) {
                return false;
            }
            let effective = parse_rfc3339(d.predicted_time.as_deref().unwrap_or(&d.scheduled_time));
            match effective {
                Some(t) => t >= now_secs && t <= horizon,
                None => false,
            }
        })
        .collect()
}

/// Fetch departures for all configured stops from BKK FUTÁR, filtered by the
/// configured routes and walking-time horizon, sorted by effective time and
/// deduplicated.
///
/// Any failure (missing API key, network, non-2xx, parse) is a `FetchError`;
/// the caller degrades to the last-good snapshot or `Unavailable`.
pub async fn fetch_transit(
    client: &reqwest::Client,
    cfg: &TransitConfig,
) -> Result<Vec<Departure>, FetchError> {
    let api_key = cfg.api_key.as_deref().ok_or_else(|| {
        FetchError::Other("BKK API key not set (MIRROR_BKK_API_KEY)".to_string())
    })?;

    let now_secs = chrono::Utc::now().timestamp();
    let base = cfg.base_url.trim_end_matches('/');
    let mut all: Vec<Departure> = Vec::new();
    for stop in &cfg.stops {
        let url = format!(
            "{base}/arrivals-and-departures-for-stop.json?key={}&stopId={}&onlyDepartures=true&minutesBefore=0&minutesAfter={}&limit=60",
            urlenc(api_key),
            urlenc(stop),
            cfg.walking_minutes,
        );
        let resp = client.get(&url).send().await.map_err(FetchError::Network)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::Http {
                status,
                retry_after: retry_after_from_response(&resp),
            });
        }
        let body = resp
            .text()
            .await
            .map_err(|e| FetchError::Parse(e.to_string()))?;
        all.extend(parse_futar(&body)?);
    }

    let filtered = filter_departures(all, &cfg.routes, cfg.walking_minutes, now_secs);
    let mut out = filtered;
    out.sort_by(|a, b| {
        let ta = parse_rfc3339(a.predicted_time.as_deref().unwrap_or(&a.scheduled_time))
            .unwrap_or(i64::MAX);
        let tb = parse_rfc3339(b.predicted_time.as_deref().unwrap_or(&b.scheduled_time))
            .unwrap_or(i64::MAX);
        ta.cmp(&tb)
    });
    out.dedup_by(|a, b| {
        a.stop_id == b.stop_id && a.route == b.route && a.scheduled_time == b.scheduled_time
    });
    Ok(out)
}

/// Minimal percent-encoding for the two query values we interpolate (the API
/// key and the stop id). FUTÁR stop ids and keys are ASCII, but we encode the
/// characters that would otherwise break the query string.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ===========================================================================
// Cache + section building (weather.rs / news.rs / calendar.rs pattern)
// ===========================================================================

/// A last-known-good reading with the instant it was fetched.
#[derive(Debug, Clone)]
struct Snapshot<T> {
    fetched_at: Instant,
    value: T,
}

/// The outcome of resolving the transit reading against its refresh interval.
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

/// In-memory transit snapshot, atomically replaced on each successful refresh
/// (architecture doc §Data). The independently-configured transit refresh
/// interval is honored across polls (acceptance criterion: "the transit
/// refresh interval is independently configurable").
#[derive(Debug, Default)]
pub struct TransitCache {
    departures: RwLock<Option<Snapshot<Vec<Departure>>>>,
    /// Retry state (Slice 6): after a fetch failure we skip refetches until
    /// the backoff cooldown elapses, serving the last-good snapshot as `Stale`
    /// in the meantime — so a failing FUTÁR endpoint is not re-hit on every
    /// poll.
    retry: RwLock<RetryState>,
}

impl TransitCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the departures snapshot from a persisted last-good value (Slice 6
    /// restart recovery). The snapshot is stamped with the current instant so
    /// it is immediately eligible to be served (as `Stale` if a refetch is
    /// attempted and fails, or `Fresh` if the refresh interval has not yet
    /// elapsed).
    pub async fn seed(&self, value: Vec<Departure>) {
        *self.departures.write().await =
            Some(Snapshot { fetched_at: Instant::now(), value });
    }

    /// The last-good departures, if any (for persistence).
    pub async fn last_good(&self) -> Option<Vec<Departure>> {
        self.departures.read().await.as_ref().map(|s| s.value.clone())
    }

    /// Resolve the current departures, honoring the transit refresh interval.
    ///
    /// A snapshot younger than the interval is served as-is (`Fresh`).
    /// Otherwise a refetch is attempted: on success the snapshot is replaced
    /// and the value is `Fresh`; on failure the last-good snapshot (if any) is
    /// served as `Stale`, else `Unavailable`.
    ///
    /// While the provider is in its backoff cooldown (a recent failure) the
    /// refetch is skipped entirely and the last-good snapshot is served as
    /// `Stale` (Slice 6) — so a failing FUTÁR endpoint is not re-hit on every
    /// poll.
    pub async fn resolve(
        &self,
        client: &reqwest::Client,
        cfg: &TransitConfig,
    ) -> Resolved<Vec<Departure>> {
        let interval = Duration::from_secs(cfg.refresh_secs);
        if let Some(snap) = self.departures.read().await.as_ref() {
            if snap.fetched_at.elapsed() < interval {
                return Resolved::Fresh(snap.value.clone());
            }
        }
        // In backoff cooldown: skip the refetch and serve the last-good value.
        if self.retry.read().await.in_cooldown() {
            return match self.departures.read().await.as_ref() {
                Some(snap) => Resolved::Stale(snap.value.clone()),
                None => Resolved::Unavailable,
            };
        }
        match fetch_transit(client, cfg).await {
            Ok(departures) => {
                self.retry.write().await.record_success();
                *self.departures.write().await = Some(Snapshot {
                    fetched_at: Instant::now(),
                    value: departures.clone(),
                });
                Resolved::Fresh(departures)
            }
            Err(err) => {
                self.retry.write().await.record_failure(err.retry_after());
                // Log a redacted, log-safe description — never the BKK API key
                // (it is only ever sent as a query parameter, never embedded in
                // an error message) (Slice 6 secret redaction).
                eprintln!("magic-mirror: transit fetch failed: {}", describe_fetch_error(&err));
                match self.departures.read().await.as_ref() {
                    Some(snap) => Resolved::Stale(snap.value.clone()),
                    None => Resolved::Unavailable,
                }
            }
        }
    }
}

/// A short, log-safe description of a transit fetch failure. The BKK API key
/// is never included (it is only ever sent as a `key` query parameter, never
/// embedded in an error message), but the description is kept generic so it
/// can be logged safely (Slice 6 secret redaction).
fn describe_fetch_error(err: &FetchError) -> String {
    match err {
        FetchError::Network(e) => format!("network: {e}"),
        FetchError::Http { status, .. } => format!("non-2xx status: {status}"),
        FetchError::Parse(msg) => format!("parse: {msg}"),
        FetchError::Other(msg) => msg.clone(),
    }
}

/// Fold the resolved departures into the API's `TransitSection`.
///
/// `departures` is `Some` when a reading is available (fresh or stale) and
/// `None` when there is no data at all (unavailable), in which case the
/// section degrades to the supplied fallback (mock) body with
/// `Status::Unavailable`.
pub fn build_transit_section(
    now: String,
    status: Status,
    departures: Option<Vec<Departure>>,
    fallback: TransitSection,
) -> TransitSection {
    match departures {
        Some(items) => TransitSection {
            updated_at: now,
            status,
            departures: items,
        },
        None => TransitSection {
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

    /// A FUTÁR-shaped response body with one realtime (on-time) departure and
    /// one schedule-only departure. The base epoch is the current instant so
    /// the departures fall in the near future (within the walking-time horizon)
    /// and the full fetch → filter flow actually populates the section.
    fn sample_body() -> String {
        let base = chrono::Utc::now().timestamp();
        serde_json::json!({
            "data": {
                "entry": {
                    "stopTimes": [
                        {
                            "stopId": "BKK:9210001:1",
                            "stopHeadsign": "Kálvin tér",
                            "departureTime": base + 120,
                            "predictedDepartureTime": base + 120,
                            "tripId": "trip:4"
                        },
                        {
                            "stopId": "BKK:9210001:1",
                            "stopHeadsign": "Keleti pályaudvar",
                            "departureTime": base + 300,
                            "tripId": "trip:8"
                        }
                    ]
                },
                "references": {
                    "routes": {
                        "route:4": { "shortName": "4", "longName": "4", "type": "TRAM" },
                        "route:8": { "shortName": "8", "longName": "8", "type": "TRAM" }
                    },
                    "trips": {
                        "trip:4": { "routeId": "route:4" },
                        "trip:8": { "routeId": "route:8" }
                    }
                }
            }
        })
        .to_string()
    }

    /// A realtime departure (a predicted time equal to the scheduled time) maps
    /// to `realtime: true`, `delay_seconds: 0`, and a `predicted_time` (the
    /// "ON TIME" case).
    #[test]
    fn parse_futar_realtime_on_time() {
        let deps = parse_futar(&sample_body()).expect("parses");
        let dep = deps.iter().find(|d| d.route == "4").expect("route 4 present");
        assert!(dep.realtime, "a predicted time means realtime");
        assert_eq!(dep.delay_seconds, 0, "predicted == scheduled is on time");
        assert!(dep.predicted_time.is_some(), "predicted_time must be set");
        assert_eq!(dep.destination, "Kálvin tér");
        assert_eq!(dep.platform, None, "this endpoint has no platform");
    }

    /// A schedule-only departure (no predicted time) maps to `realtime: false`,
    /// `delay_seconds: 0`, and no `predicted_time` (the "SCHEDULED" case).
    #[test]
    fn parse_futar_schedule_only() {
        let deps = parse_futar(&sample_body()).expect("parses");
        let dep = deps.iter().find(|d| d.route == "8").expect("route 8 present");
        assert!(!dep.realtime, "no predicted time means schedule-only");
        assert_eq!(dep.delay_seconds, 0, "schedule-only has no delay");
        assert!(dep.predicted_time.is_none(), "predicted_time must be absent");
        assert_eq!(dep.destination, "Keleti pályaudvar");
    }

    /// A realtime departure whose predicted time is later than the scheduled
    /// time maps to a positive `delay_seconds` (the "DELAYED" case).
    #[test]
    fn parse_futar_realtime_delayed() {
        let body = serde_json::json!({
            "data": {
                "entry": {
                    "stopTimes": [
                        {
                            "stopId": "BKK:1",
                            "stopHeadsign": "D",
                            "departureTime": 1_000_000,
                            "predictedDepartureTime": 1_000_300,
                            "tripId": "t"
                        }
                    ]
                },
                "references": {
                    "routes": { "r": { "shortName": "6" } },
                    "trips": { "t": { "routeId": "r" } }
                }
            }
        })
        .to_string();
        let deps = parse_futar(&body).expect("parses");
        let dep = &deps[0];
        assert!(dep.realtime);
        assert_eq!(dep.delay_seconds, 300, "predicted 300s after scheduled");
    }

    /// A stop time whose trip or route cannot be resolved is skipped (defensive
    /// — never a panic, never a blank route).
    #[test]
    fn parse_futar_unresolvable_trip_is_skipped() {
        let body = serde_json::json!({
            "data": {
                "entry": {
                    "stopTimes": [
                        {
                            "stopId": "BKK:1",
                            "stopHeadsign": "D",
                            "departureTime": 1_000_000,
                            "tripId": "missing-trip"
                        }
                    ]
                },
                "references": { "routes": {}, "trips": {} }
            }
        })
        .to_string();
        let deps = parse_futar(&body).expect("parses");
        assert!(deps.is_empty(), "unresolvable stop time must be skipped: {deps:?}");
    }

    /// Malformed JSON is a parse error (not a panic).
    #[test]
    fn parse_futar_bad_json_is_parse_error() {
        let err = parse_futar("this is not json {").expect_err("must fail");
        assert!(matches!(err, FetchError::Parse(_)), "expected Parse, got {err:?}");
    }

    // --- Route + walking-time filtering ------------------------------------

    fn dep(route: &str, scheduled: i64, predicted: Option<i64>) -> Departure {
        Departure {
            stop_id: "BKK:1".into(),
            route: route.into(),
            destination: "D".into(),
            scheduled_time: format_rfc3339(scheduled),
            predicted_time: predicted.map(format_rfc3339),
            realtime: predicted.is_some(),
            delay_seconds: predicted.unwrap_or(scheduled) - scheduled,
            platform: None,
        }
    }

    /// The route filter keeps only configured routes; an empty list keeps all.
    #[test]
    fn filter_departures_by_route() {
        let now = 1_000_000i64;
        let deps = vec![
            dep("4", now + 60, None),
            dep("6", now + 120, None),
            dep("8", now + 180, None),
        ];
        let kept = filter_departures(deps.clone(), &["4".into(), "8".into()], 60, now);
        let routes: Vec<&str> = kept.iter().map(|d| d.route.as_str()).collect();
        assert_eq!(routes, vec!["4", "8"], "only configured routes kept: {routes:?}");

        let all = filter_departures(deps, &[], 60, now);
        assert_eq!(all.len(), 3, "empty routes keeps all: {all:?}");
    }

    /// The walking-time filter keeps only departures within the horizon and
    /// drops past departures.
    #[test]
    fn filter_departures_by_walking_time() {
        let now = 1_000_000i64;
        let deps = vec![
            dep("4", now + 60, None),   // within horizon → kept
            dep("6", now + 1200, None), // within 30 min → kept
            dep("8", now + 3600, None), // beyond 30 min → dropped
            dep("2", now - 60, None),   // in the past → dropped
        ];
        let kept = filter_departures(deps, &[], 30, now);
        let routes: Vec<&str> = kept.iter().map(|d| d.route.as_str()).collect();
        assert_eq!(routes, vec!["4", "6"], "horizon + past filter: {routes:?}");
    }

    /// Both filters apply together.
    #[test]
    fn filter_departures_combined() {
        let now = 1_000_000i64;
        let deps = vec![
            dep("4", now + 60, None),   // route ok, in horizon → kept
            dep("6", now + 60, None),   // route not configured → dropped
            dep("4", now + 3600, None), // route ok, beyond horizon → dropped
        ];
        let kept = filter_departures(deps, &["4".into()], 30, now);
        assert_eq!(kept.len(), 1, "only the in-horizon configured route: {kept:?}");
        assert_eq!(kept[0].route, "4");
    }

    /// The walking-time filter uses the *predicted* time (when present) for the
    /// horizon check, not the scheduled time: a vehicle whose scheduled time is
    /// beyond the horizon but whose predicted time is within it is kept.
    #[test]
    fn filter_uses_predicted_time_for_horizon() {
        let now = 1_000_000i64;
        let deps = vec![
            // Scheduled beyond the 30-min horizon, but predicted within it → kept.
            dep("4", now + 3600, Some(now + 60)),
            // Scheduled within the horizon, but predicted beyond it → dropped.
            dep("6", now + 60, Some(now + 3600)),
        ];
        let kept = filter_departures(deps, &[], 30, now);
        let routes: Vec<&str> = kept.iter().map(|d| d.route.as_str()).collect();
        assert_eq!(routes, vec!["4"], "predicted time drives the horizon: {routes:?}");
    }

    // --- Section building ---------------------------------------------------

    fn fallback_section() -> TransitSection {
        TransitSection {
            updated_at: String::new(),
            status: Status::Fresh,
            departures: vec![dep("4", 100, None)],
        }
    }

    /// A resolved reading folds into the section with the given status.
    #[test]
    fn build_transit_section_some_is_status() {
        let section = build_transit_section(
            "now".into(),
            Status::Stale,
            Some(vec![dep("4", 100, None)]),
            fallback_section(),
        );
        assert_eq!(section.status, Status::Stale);
        assert_eq!(section.departures.len(), 1);
        assert_eq!(section.updated_at, "now");
    }

    /// No reading degrades to the fallback body with `Status::Unavailable`.
    #[test]
    fn build_transit_section_none_is_unavailable() {
        let section = build_transit_section("now".into(), Status::Unavailable, None, fallback_section());
        assert_eq!(section.status, Status::Unavailable);
        assert_eq!(section.departures.len(), 1, "fallback departures retained");
        assert_eq!(section.updated_at, "now");
    }

    // --- Cache / refresh-interval behaviour (mock server) -------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Start a minimal mock FUTÁR server on a random localhost port, returning
    /// a fixed FUTÁR-shaped body, with a hit counter.
    async fn start_mock_futar() -> (String, Arc<AtomicUsize>) {
        use axum::{routing::get, Json, Router};

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let body = sample_body();

        let app = Router::new().route(
            "/arrivals-and-departures-for-stop.json",
            get(move |_: ()| {
                h.fetch_add(1, Ordering::SeqCst);
                async move { Json(serde_json::from_str::<serde_json::Value>(&body).unwrap()) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock futar");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock futar ran");
        });

        (format!("http://{addr}"), hits)
    }

    fn test_cfg(base: &str) -> TransitConfig {
        TransitConfig {
            enabled: true,
            stops: vec!["BKK:9210001:1".into()],
            routes: vec![],
            walking_minutes: 60,
            refresh_secs: 3600,
            base_url: base.to_string(),
            api_key: Some("test-key".into()),
        }
    }

    /// A reading within the refresh interval is served from cache: the second
    /// resolve does not hit the upstream again. The resolved value is the
    /// parsed + filtered departures from the FUTÁR body (acceptance criterion:
    /// "the transit section is populated from the BKK FUTÁR API for configured
    /// stops").
    #[tokio::test]
    async fn cache_serves_within_interval() {
        let (base, hits) = start_mock_futar().await;
        let cfg = test_cfg(&base);
        let cache = TransitCache::new();
        let client = reqwest::Client::new();

        let r1 = cache.resolve(&client, &cfg).await;
        let r2 = cache.resolve(&client, &cfg).await;

        let (deps, status) = r1.into_parts();
        assert_eq!(status, Status::Fresh);
        let deps = deps.expect("fresh reading must carry departures");
        // Both sample departures (routes 4 and 8) are in the near future and
        // within the 60-minute horizon, so both survive the filter.
        let routes: Vec<&str> = deps.iter().map(|d| d.route.as_str()).collect();
        assert_eq!(routes, vec!["4", "8"], "both FUTÁR departures populated: {routes:?}");

        assert!(matches!(r2, Resolved::Fresh(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "second call must be cached");
    }

    /// An interval of zero forces a refetch every time (no caching).
    #[tokio::test]
    async fn zero_interval_always_refetches() {
        let (base, hits) = start_mock_futar().await;
        let mut cfg = test_cfg(&base);
        cfg.refresh_secs = 0;
        let cache = TransitCache::new();
        let client = reqwest::Client::new();

        let _ = cache.resolve(&client, &cfg).await;
        let _ = cache.resolve(&client, &cfg).await;

        assert_eq!(hits.load(Ordering::SeqCst), 2, "zero interval must refetch");
    }

    // --- Slice 6: restart recovery + backoff cooldown -----------------------

    /// Start a mock FUTÁR server that always fails (500), with a hit counter.
    async fn start_failing_futar() -> (String, Arc<AtomicUsize>) {
        use axum::Router;

        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();

        let app = Router::new().fallback(move |_: ()| {
            h.fetch_add(1, Ordering::SeqCst);
            async move { axum::http::StatusCode::INTERNAL_SERVER_ERROR }
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind failing futar");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("failing futar ran");
        });

        (format!("http://{addr}"), hits)
    }

    fn seeded_departures() -> Vec<Departure> {
        vec![dep("4", 1_791_561_720, None)]
    }

    /// Restart recovery: a seeded (persisted) last-good snapshot is served as
    /// `Stale` when the first refresh after restart fails.
    #[tokio::test]
    async fn restart_recovery_serves_last_good_as_stale() {
        let (base, _hits) = start_failing_futar().await;
        let mut cfg = test_cfg(&base);
        cfg.refresh_secs = 0; // zero interval forces a refetch
        let cache = TransitCache::new();
        let client = reqwest::Client::new();

        cache.seed(seeded_departures()).await;

        let resolved = cache.resolve(&client, &cfg).await;
        let (deps, status) = resolved.into_parts();
        assert_eq!(status, Status::Stale, "seeded last-good must be served as Stale");
        let deps = deps.expect("seeded last-good must be retained");
        assert!(deps.iter().any(|d| d.route == "4"), "seeded departure retained: {deps:?}");
    }

    /// Backoff cooldown: after a failure, the next resolve within the cooldown
    /// window does not hit the upstream.
    #[tokio::test]
    async fn backoff_cooldown_skips_refetch() {
        let (base, hits) = start_failing_futar().await;
        let mut cfg = test_cfg(&base);
        cfg.refresh_secs = 0; // zero interval forces a refetch
        let cache = TransitCache::new();
        let client = reqwest::Client::new();

        cache.seed(seeded_departures()).await;

        let r1 = cache.resolve(&client, &cfg).await;
        assert!(matches!(r1, Resolved::Stale(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "first resolve must attempt the fetch");

        let r2 = cache.resolve(&client, &cfg).await;
        assert!(matches!(r2, Resolved::Stale(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "cooldown must skip the refetch");
    }

    // --- Secret redaction ---------------------------------------------------

    /// The BKK API key must not appear in the log-safe error description nor in
    /// the API JSON (acceptance criterion: "the BKK API key … never appears in
    /// API JSON or config files").
    #[tokio::test]
    async fn api_key_not_leaked_in_logs_or_api_json() {
        const SECRET: &str = "super-secret-bkk-key-XYZ123";
        let (base, _hits) = start_failing_futar().await;
        let mut cfg = test_cfg(&base);
        cfg.api_key = Some(SECRET.into());
        let client = reqwest::Client::new();

        // The fetch fails (500). The log-safe description must not contain the
        // key — it is only ever sent as a query parameter, never embedded in an
        // error message.
        let err = fetch_transit(&client, &cfg).await.expect_err("must fail");
        let description = describe_fetch_error(&err);
        assert!(
            !description.contains(SECRET),
            "the BKK API key must not appear in the log description: {description:?}"
        );

        // The API JSON (transit section) must not contain the key either.
        let section = build_transit_section(
            "now".into(),
            Status::Stale,
            Some(vec![dep("4", 100, None)]),
            fallback_section(),
        );
        let json = serde_json::to_string(&section).expect("serializes");
        assert!(
            !json.contains(SECRET),
            "the BKK API key must not appear in the API JSON: {json:?}"
        );
    }

    /// A missing API key is a clean `Other` failure (not a panic).
    #[tokio::test]
    async fn fetch_transit_without_api_key_fails() {
        let (base, _hits) = start_mock_futar().await;
        let mut cfg = test_cfg(&base);
        cfg.api_key = None;
        let client = reqwest::Client::new();

        let err = fetch_transit(&client, &cfg).await.expect_err("must fail");
        assert!(
            matches!(err, FetchError::Other(_)),
            "expected Other, got {err:?}"
        );
    }
}
