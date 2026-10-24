// Magic Mirror — frontend (Slice 1).
//
// Native ES module. No framework, no build step. Fetches the combined
// dashboard payload, renders it with DOM text nodes (untrusted provider
// strings are never injected as HTML), and keeps the clock / relative times
// live. Dynamic positioning uses element.style (allowed by the CSP, which
// forbids inline <style> attributes and external scripts only).
//
// Spec references: §5 (API shape), §6 (frontend rules), §9 (security).

const $ = (id) => document.getElementById(id);

const DAY_START_HOUR = 7;
const DAY_END_HOUR = 22;
const IMMINENT_MINUTES = 45;
const POLL_MS = 30_000;
const CLOCK_MS = 1_000;
const RELATIVE_MS = 15_000;

const DEFAULT_TZ = "Europe/Budapest";
const DEFAULT_LOCALE = "hu-HU";

let state = {
  tz: DEFAULT_TZ,
  locale: DEFAULT_LOCALE,
  dashboard: null,
  lastSuccess: null,
  error: null,
};

let pollController = null;

const CALENDAR_STORAGE_KEY = "mirror.hiddenCalendars";
let hiddenCalendars;
try {
  const saved = JSON.parse(localStorage.getItem(CALENDAR_STORAGE_KEY) || "[]");
  hiddenCalendars = new Set(Array.isArray(saved) ? saved.filter((id) => typeof id === "string") : []);
} catch {
  hiddenCalendars = new Set();
}
let calendars = [];

function visibleCalendarSection(section) {
  if (!section) return section;
  return {
    ...section,
    events: section.events.filter((event) =>
      ![...hiddenCalendars].some((id) => event.id.startsWith(`${id}:`))),
  };
}

function calendarChoices() {
  const choices = new Map(calendars);
  // Older snapshots have events but no discovery metadata. Keep their
  // calendars selectable while discovery is offline or still loading.
  for (const event of state.dashboard?.calendar?.events ?? []) {
    const match = event.id.match(/^(https?:\/\/.*?\/):/);
    if (!match || choices.has(match[1])) continue;
    const id = match[1];
    const segment = new URL(id).pathname.split("/").filter(Boolean).pop() || "Calendar";
    let name = segment;
    try { name = decodeURIComponent(segment); } catch { /* Keep the original path segment. */ }
    choices.set(id, name);
  }
  return [...choices];
}

function renderCalendarOptions() {
  const choices = calendarChoices();
  const host = $("calendar-options");
  // Do not replace a focused checkbox during periodic dashboard refreshes.
  const signature = JSON.stringify(choices);
  if (host.dataset.calendars === signature) return;
  host.dataset.calendars = signature;
  clear(host);
  if (!choices.length) {
    host.textContent = "No calendars available yet.";
    return;
  }
  for (const [id, name] of choices) {
    const label = el("label", "calendar-option");
    const input = el("input");
    input.type = "checkbox";
    input.checked = !hiddenCalendars.has(id);
    input.addEventListener("change", () => {
      if (input.checked) hiddenCalendars.delete(id);
      else hiddenCalendars.add(id);
      try {
        localStorage.setItem(CALENDAR_STORAGE_KEY, JSON.stringify([...hiddenCalendars]));
        $("calendar-save-status").textContent = "Saved in this browser.";
      } catch {
        $("calendar-save-status").textContent = "Applies for this session; browser storage is unavailable.";
      }
      renderCalendar();
    });
    label.append(input, el("span", null, name));
    host.appendChild(label);
  }
}

function initCalendarMenu() {
  const toggle = $("menu-toggle");
  const panel = $("calendar-panel");
  function close() {
    panel.hidden = true;
    toggle.setAttribute("aria-expanded", "false");
  }
  toggle.addEventListener("click", () => {
    panel.hidden = !panel.hidden;
    toggle.setAttribute("aria-expanded", String(!panel.hidden));
  });
  document.addEventListener("click", (event) => {
    if (!$("calendar-menu").contains(event.target)) close();
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !panel.hidden) {
      close();
      toggle.focus();
    }
  });
}

function renderCalendar() {
  const section = visibleCalendarSection(state.dashboard?.calendar);
  renderRuler(dayWindow(state.tz, new Date()), section?.events ?? []);
  renderEvents(section);
  if (section && !section.events.length) {
    $("events").appendChild(el("p", "calendar-empty", "No events in visible calendars."));
  }
}

/* ------------------------------------------------------------------ *
 * Timezone helpers (Intl-based, DST-safe)
 * ------------------------------------------------------------------ */

function zonedDateParts(tz, date) {
  const dtf = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
  });
  const map = {};
  for (const p of dtf.formatToParts(date)) map[p.type] = p.value;
  return { year: +map.year, month: +map.month, day: +map.day };
}

function zoneOffsetMs(tz, date) {
  const dtf = new Intl.DateTimeFormat("en-US", {
    timeZone: tz,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  const map = {};
  for (const p of dtf.formatToParts(date)) map[p.type] = p.value;
  const asUTC = Date.UTC(
    map.year,
    map.month - 1,
    map.day,
    map.hour % 24,
    map.minute,
    map.second,
  );
  return asUTC - date.getTime();
}

// Convert a desired zone wall-clock (expressed as a UTC-like ms number) to the
// real instant, refining against the zone offset (handles DST).
function zonedToInstant(tz, wall) {
  let instant = wall - zoneOffsetMs(tz, new Date(wall));
  instant = wall - zoneOffsetMs(tz, new Date(instant));
  return instant;
}

function dayWindow(tz, date) {
  const { year, month, day } = zonedDateParts(tz, date);
  const startWall = Date.UTC(year, month - 1, day, DAY_START_HOUR, 0, 0);
  const endWall = Date.UTC(year, month - 1, day, DAY_END_HOUR, 0, 0);
  return {
    start: zonedToInstant(tz, startWall),
    end: zonedToInstant(tz, endWall),
  };
}

function clamp01(x) {
  return Math.min(1, Math.max(0, x));
}

function pct(instant, window) {
  const span = window.end - window.start;
  if (span <= 0) return 0;
  return clamp01((instant - window.start) / span) * 100;
}

/* ------------------------------------------------------------------ *
 * Formatting
 * ------------------------------------------------------------------ */

function fmtTime(tz, date) {
  return new Intl.DateTimeFormat(state.locale, {
    timeZone: tz,
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

function fmtSeconds(tz, date) {
  return new Intl.DateTimeFormat(state.locale, {
    timeZone: tz,
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  })
    .format(date)
    .split(":")
    .pop();
}

function fmtDate(tz, date) {
  const s = new Intl.DateTimeFormat(state.locale, {
    timeZone: tz,
    weekday: "long",
    day: "2-digit",
    month: "short",
  }).format(date);
  return s.toUpperCase();
}

function fmtClock(tz, date) {
  return new Intl.DateTimeFormat(state.locale, {
    timeZone: tz,
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  }).format(date);
}

function fmtDay(tz, date) {
  return new Intl.DateTimeFormat(state.locale, {
    timeZone: tz,
    day: "2-digit",
    month: "short",
  })
    .format(date)
    .toUpperCase();
}

function fmtMinutes(ms) {
  const mins = Math.max(0, Math.ceil(ms / 60_000));
  return `${mins}M`;
}

function fmtDuration(ms) {
  const total = Math.max(0, Math.round(ms / 60_000));
  const h = Math.floor(total / 60);
  const m = total % 60;
  if (h > 0) return `${h}H ${m}M`;
  return `${m}M`;
}

function parseInstant(value) {
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? null : d.getTime();
}

/* ------------------------------------------------------------------ *
 * DOM helpers (text nodes only — no innerHTML with untrusted data)
 * ------------------------------------------------------------------ */

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function clear(node) {
  node.replaceChildren();
}

/* ------------------------------------------------------------------ *
 * Clock (renders immediately, drift-safe)
 * ------------------------------------------------------------------ */

function renderClock() {
  const now = new Date();
  const tz = state.tz;
  const time = fmtTime(tz, now);
  const [hh, mm] = time.split(":");
  const seconds = fmtSeconds(tz, now);
  $("clock-time").textContent = `${hh}:${mm}`;
  $("clock-seconds").textContent = `:${seconds}`;
  $("clock-date").textContent = fmtDate(tz, now);
}

function scheduleClock() {
  // Align the next tick to the wall clock so ticks never drift.
  const now = Date.now();
  const delay = 1000 - (now % 1000);
  setTimeout(() => {
    renderClock();
    scheduleClock();
  }, delay);
}

/* ------------------------------------------------------------------ *
 * Weather glance
 * ------------------------------------------------------------------ */

const CONDITION_TEXT = {
  45: "COLD FOG",
  48: "RIME FOG",
  51: "LIGHT DRIZZLE",
  53: "DRIZZLE",
  55: "HEAVY DRIZZLE",
  61: "LIGHT RAIN",
  63: "RAIN",
  65: "HEAVY RAIN",
  66: "FREEZING RAIN",
  67: "FREEZING RAIN",
  71: "LIGHT SNOW",
  73: "SNOW",
  75: "HEAVY SNOW",
  77: "SNOW GRAINS",
  80: "RAIN SHOWERS",
  81: "RAIN SHOWERS",
  82: "HEAVY SHOWERS",
  85: "SNOW SHOWERS",
  86: "SNOW SHOWERS",
  95: "THUNDERSTORM",
  96: "STORM + HAIL",
  99: "STORM + HAIL",
  0: "CLEAR",
  1: "MAINLY CLEAR",
  2: "PARTLY CLOUDY",
  3: "OVERCAST",
};

function renderWeather(section) {
  if (!section) return;
  const temp = section.temperature_c;
  $("wx-temp").textContent =
    Number.isFinite(temp) ? temp.toFixed(1) : "--";
  $("wx-condition").textContent =
    CONDITION_TEXT[section.condition_code] ?? "—";
  $("wx-pressure").textContent =
    Number.isFinite(section.pressure_hpa) ? `${section.pressure_hpa} hPa` : "-- hPa";
  $("wx-aqi").textContent =
    Number.isFinite(section.european_aqi) ? `AQI ${section.european_aqi}` : "AQI --";
}

/* ------------------------------------------------------------------ *
 * Timeline ruler
 * ------------------------------------------------------------------ */

function renderRuler(window, events) {
  const labels = $("ruler-labels");
  clear(labels);
  const marks = [
    { t: "07:00", cls: "label" },
    { t: "10:00", cls: "label" },
    { t: "13:00", cls: "label" },
    { t: "16:00", cls: "label" },
    { t: "19:00", cls: "label" },
    { t: "22:00", cls: "label" },
  ];
  for (const m of marks) labels.appendChild(el("span", m.cls, m.t));

  const track = $("ruler-track");
  // Rebuild blocks + ticks, keep the now marker element.
  const nowEl = $("ruler-now");
  clear(track);

  // Ticks.
  const ticks = el("div", "ruler-ticks");
  const tickHeights = [8, 6, 8, 6, 10, 6, 8, 6, 8];
  for (const h of tickHeights) {
    const t = el("div", "tick");
    t.style.height = `${h}px`;
    ticks.appendChild(t);
  }
  track.appendChild(ticks);

  // Event blocks (clamped to the visible day span).
  for (const ev of events) {
    const s = parseInstant(ev.start);
    const e = parseInstant(ev.end);
    if (s == null || e == null) continue;
    const left = pct(s, window);
    const right = pct(e, window);
    if (right <= left) continue;
    const block = el("div", "ruler-block");
    block.style.left = `${left}%`;
    block.style.width = `${right - left}%`;
    track.appendChild(block);
  }

  // Now marker.
  const nowPct = pct(Date.now(), window);
  nowEl.style.left = `calc(${nowPct}% - 2px)`;
  nowEl.hidden = false;
  track.appendChild(nowEl);
}

/* ------------------------------------------------------------------ *
 * Event cards
 * ------------------------------------------------------------------ */

function urgency(ev, now) {
  const s = parseInstant(ev.start);
  let e = parseInstant(ev.end);
  if (s == null) return null;
  // An all-day event with no distinct end (no DTEND) collapses to a
  // zero-length span at midnight UTC and would read as instantly "past".
  // Treat it as lasting the whole day.
  if (ev.all_day && (e == null || e <= s)) e = s + 86_400_000;
  if (e != null && now >= e) return { kind: "past" };
  if (now >= s) return { kind: "ongoing", end: e };
  const until = s - now;
  // All-day events have no meaningful "imminent" window — they're either
  // happening or they're later.
  if (!ev.all_day && until <= IMMINENT_MINUTES * 60_000)
    return { kind: "imminent", until };
  return { kind: "later", until };
}

function renderEvents(section) {
  const host = $("events");
  clear(host);
  if (!section) return;

  const now = Date.now();
  const events = [...section.events].sort(
    (a, b) => (parseInstant(a.start) ?? 0) - (parseInstant(b.start) ?? 0),
  );

  // The single most urgent upcoming/ongoing event gets the hero treatment.
  let heroId = null;
  for (const ev of events) {
    const u = urgency(ev, now);
    if (u && (u.kind === "ongoing" || u.kind === "imminent")) {
      heroId = ev.id;
      break;
    }
  }

  for (const ev of events) {
    const u = urgency(ev, now);
    const isHero = ev.id === heroId;
    const classes = ["event"];
    if (isHero) classes.push("imminent");
    if (ev.all_day) classes.push("allday");
    const card = el("div", classes.join(" "));

    const top = el("div", "event-top");
    const timeWrap = el("div", "event-time");
    const start = parseInstant(ev.start);
    const chipText =
      start == null
        ? "--:--"
        : ev.all_day
          ? fmtDay(state.tz, new Date(start))
          : fmtClock(state.tz, new Date(start));
    timeWrap.appendChild(el("span", "chip", chipText));
    if (u) {
      let badgeText;
      if (u.kind === "ongoing") badgeText = "NOW";
      else if (u.kind === "imminent") badgeText = `T-MINUS ${fmtMinutes(u.until)}`;
      else if (u.kind === "later") badgeText = `IN ${fmtDuration(u.until)}`;
      else badgeText = "DONE";
      timeWrap.appendChild(el("span", "badge", badgeText));
    }
    top.appendChild(timeWrap);
    if (ev.location) top.appendChild(el("span", "event-location", ev.location));
    card.appendChild(top);

    // Event descriptions are intentionally not rendered: they take up too
    // much space and most events carry no useful body text. The API still
    // exposes `description` (response shape stays stable).
    const body = el("div", "event-body");
    body.appendChild(el("span", "title", ev.title));
    card.appendChild(body);

    host.appendChild(card);
  }
}

/* ------------------------------------------------------------------ *
 * Transit
 * ------------------------------------------------------------------ */

function renderTransit(section) {
  const heroHost = $("transit-hero");
  const secHost = $("transit-secondary");
  clear(heroHost);
  clear(secHost);
  if (!section) return;

  const now = Date.now();
  const deps = section.departures
    .map((d) => ({ d, t: parseInstant(d.predicted_time ?? d.scheduled_time) }))
    .filter((x) => x.t != null)
    .sort((a, b) => a.t - b.t);

  if (deps.length === 0) return;

  const [hero, ...rest] = deps;
  const hd = hero.d;
  const heroLeft = el("div", "left");
  heroLeft.appendChild(el("span", "route-badge", hd.route));
  const info = el("div");
  info.appendChild(el("span", "dest", hd.destination));
  const meta = el("div", "meta");
  const sched = parseInstant(hd.scheduled_time);
  meta.appendChild(
    el("span", "time", sched != null ? fmtClock(state.tz, new Date(sched)) : "--:--"),
  );
  meta.appendChild(el("span", "dot", "•"));
  if (hd.platform) meta.appendChild(el("span", null, `PLATFORM ${hd.platform}`));
  meta.appendChild(el("span", "dot", "•"));
  const delay = hd.delay_seconds;
  const status =
    delay === 0
      ? "ON TIME"
      : delay > 0
        ? `+${Math.round(delay / 60)}M DELAY`
        : `${Math.round(delay / 60)}M EARLY`;
  meta.appendChild(el("span", "status", status));
  info.appendChild(meta);
  heroLeft.appendChild(info);
  heroHost.appendChild(heroLeft);

  const heroRight = el("div", "right");
  const mins = Math.max(0, Math.ceil((hero.t - now) / 60_000));
  heroRight.appendChild(el("div", "minutes", String(mins).padStart(2, "0")));
  heroRight.appendChild(el("div", "minutes-label", "MINUTES"));
  heroHost.appendChild(heroRight);

  for (const r of rest.slice(0, 2)) {
    const d = r.d;
    const item = el("div", "transit-item");
    const left = el("div");
    const row = el("div");
    row.appendChild(el("span", "route", d.route));
    row.appendChild(el("span", "dest", d.destination));
    left.appendChild(row);
    if (d.platform) left.appendChild(el("span", "platform", `GLEIS ${d.platform}`));
    item.appendChild(left);
    const right = el("div", "right");
    const sched = parseInstant(d.scheduled_time);
    right.appendChild(
      el("div", "time", sched != null ? fmtClock(state.tz, new Date(sched)) : "--:--"),
    );
    const mins2 = Math.max(0, Math.ceil((r.t - now) / 60_000));
    right.appendChild(el("div", "mins", `${mins2}M`));
    item.appendChild(right);
    secHost.appendChild(item);
  }
}

/* ------------------------------------------------------------------ *
 * News
 * ------------------------------------------------------------------ */

let newsSignature = null;

function renderNews(section) {
  const host = $("news-track");
  if (!section || section.items.length === 0) {
    newsSignature = null;
    clear(host);
    return;
  }

  // Skip re-render when the feed is unchanged: rebuilding the DOM would
  // restart the ticker animation on every 15s relative-time refresh.
  const signature = section.items
    .map((i) => `${i.id}|${i.title}|${i.published_at}`)
    .join("\u0000");
  if (signature === newsSignature) return;
  newsSignature = signature;

  clear(host);
  const buildItem = (item) => {
    const row = el("div", "news-item");
    const pub = parseInstant(item.published_at);
    row.appendChild(
      el("span", "time", pub != null ? fmtClock(state.tz, new Date(pub)) : "--:--"),
    );
    row.appendChild(el("span", "sep", "//"));
    row.appendChild(el("p", "text", item.title));
    return row;
  };

  // Render every item once, then duplicate the set so the track is exactly
  // 2x the content width — the -50% loop point is mathematically exact.
  const items = section.items.map(buildItem);
  for (const row of items) host.appendChild(row);
  for (const row of items) {
    const copy = row.cloneNode(true);
    copy.setAttribute("aria-hidden", "true");
    host.appendChild(copy);
  }

  // Constant scroll speed (~50px/s) regardless of content length.
  const half = host.scrollWidth / 2;
  host.style.animationDuration = `${Math.max(half / 50, 10)}s`;
}

/* ------------------------------------------------------------------ *
 * Orchestration
 * ------------------------------------------------------------------ */

function renderAll() {
  const d = state.dashboard;
  if (!d) return;
  renderWeather(d.weather);
  renderCalendar();
  renderCalendarOptions();
  renderTransit(d.transit);
  renderNews(d.news);
}

async function fetchDashboard() {
  pollController?.abort();
  pollController = new AbortController();
  const timeout = setTimeout(() => pollController.abort(), 8_000);
  try {
    const res = await fetch("/api/v1/dashboard", {
      signal: pollController.signal,
      headers: { Accept: "application/json" },
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    if (!data || typeof data !== "object") throw new Error("bad payload");
    state.dashboard = data;
    state.tz = data.timezone || state.tz;
    state.lastSuccess = Date.now();
    state.error = null;
    renderAll();
    renderClock();
    // Fetch after the dashboard has refreshed the calendar discovery cache.
    try {
      const response = await fetch("/api/v1/calendars", {
        signal: pollController.signal,
        headers: { Accept: "application/json" },
      });
      if (response.ok) {
        calendars = await response.json();
        renderCalendarOptions();
      }
    } catch {
      // Keep the last successful menu when temporarily offline.
      if (!calendarChoices().length) $("calendar-options").textContent = "Calendars unavailable. Retrying…";
    }
  } catch (err) {
    if (err.name === "AbortError") return;
    state.error = String(err?.message ?? err);
  } finally {
    clearTimeout(timeout);
  }
}

function poll() {
  fetchDashboard().finally(() => {
    setTimeout(poll, POLL_MS);
  });
}

function init() {
  initCalendarMenu();
  renderClock();
  scheduleClock();
  poll();
  // Refresh relative times / urgency on a slower cadence.
  setInterval(() => {
    if (state.dashboard) renderAll();
  }, RELATIVE_MS);
}

init();
