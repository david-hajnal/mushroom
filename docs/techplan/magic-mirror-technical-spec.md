# Magic Mirror — Technical Specification

**Version:** 1.0 (implementation proposal)  
**Target:** lightweight, always-on portrait mirror dashboard  
**Backend:** Rust, hexagonal architecture (ports and adapters)  
**Frontend:** native browser JavaScript, HTML, CSS  
**Default locale:** `hu-HU`; configurable to `en-GB`  
**Timezone:** `Europe/Budapest`  
**Reference assets:** `screen.png` and `code.html` provided with the request

## 1. Product definition

Build a read-only, automatically updating dashboard for a wall-mounted display behind a partially reflective mirror. The software retrieves personal calendar events from Apple iCloud CalDAV, current weather and air-quality information, real-time departures from Budapest's BKK transport system, and headlines from user-configured RSS/Atom feeds.

The user-facing mirror should require no interaction in daily use. A single Rust process serves the frontend and JSON API, polls upstream services, maintains a small last-known-good cache, and exposes health state. The browser renders data and updates relative times locally.

### 1.1 Goals

- Minimal runtime footprint: one Rust executable, static assets, no frontend framework, no Node.js/Electron, no external database for MVP.
- Close visual fidelity to the supplied reference.
- Robust operation when the internet or a single provider is down.
- Provider logic independently replaceable without changing domain models or UI contracts.
- Configuration through an editable local TOML file and environment variables for secrets.
- Safe read-only calendar access and backend-only provider credentials.
- Deterministic logic and good automated test coverage.

### 1.2 Non-goals (MVP)

- Touch interaction, voice assistant, camera, microphone, face recognition.
- Creating, modifying, or deleting calendar events.
- Route planning or interactive transit maps.
- Full RSS article display, browser navigation, news summaries by LLM.
- Multiuser accounts, cloud sync, remote administration, home automation.
- Full-featured management UI; edit configuration files instead.

## 2. Inspection of supplied design

The reference is a portrait, vertically flowing dashboard (`max-width: 560px`) on pure black (`#000000`). It uses Chivo for large bold display typography, Space Mono for supporting text, an amber accent (`#ffba3d`), near-white text, dark gray borders, and subtle 4px scanlines.

Visual hierarchy from top to bottom:

1. **Clock + conditions:** large 24-hour time, smaller seconds, date and status code on the left; temperature, condition, pressure, and AQI on the right.
2. **Timeline:** 07:00–22:00 day ruler with event spans and a live current-time marker.
3. **Calendar:** five event cards; the imminent event receives an amber border, amber start time, and `T-MINUS` badge. Later events are progressively subdued.
4. **Transit:** one high-priority departure card plus two compact secondary departures.
5. **News:** two timestamped, clipped terminal-like headlines.
6. **Device footer:** minimal device/system status text.

**Source HTML adjustments:** Replace Tailwind CDN and Tailwind utilities with authored static CSS; self-host fonts with appropriate licenses; eliminate the external Material Symbols dependency (not used in visible reference); generate all event/timeline/transit/news elements from data instead of hard-coding the sample values. Replace fictitious footer metrics (`MIRROR PASS`, `DIODE`) with actual diagnostics or optionally hide them. The prototype's visual timetable markers are fixed percentages and must instead be derived from event start/end timestamps. Avoid synthetic labels such as `CST` in the Budapest deployment; use local time without abbreviation by default.

### 2.1 UI sizing and layout

- Primary target: portrait browser viewport, nominally 1080 × 1920, with graceful scaling to narrower widths.
- Main content column: `width: min(100% - 32px, 560px)` at the reference CSS scale; optionally support a configurable `ui_scale` for high-density panels.
- Page background must remain truly black outside the content column.
- The layout should fill the viewport, but **must not hide critical data** if text expands. Reduce the number of visible secondary events before scaling fonts too small.
- Respect safe margins and avoid scrollbars in mirror mode; automatic fit prioritizes clock, next event, and next transit departure.
- Display should support browser zoom / device pixel ratio; test actual attached panel.
- Preferred colors: `--bg:#000`, `--panel:#09090b`, `--border:#27272a`, `--text:#fff`, `--muted:#a1a1aa`, `--accent:#ffba3d`.
- Optional CRT effect via a fixed, pointer-events-none CSS scanline overlay, adjustable opacity and disabled by default on OLED to reduce retention risk and visual interference.

### 2.2 Information state

- Fresh: ordinary content.
- Stale: retain content, show a subtle `STALE` marker and age where useful.
- Never loaded: display `--` or concise `UNAVAILABLE`, not fictitious data.
- Next imminent event: highlight if start is within a configurable threshold (default 45 minutes); if an event is ongoing, show `NOW` / `ENDS IN …`.
- Upcoming departures: highlight the soonest *configured relevant* departure that is still catchable, subject to optional walking-time offset.

## 3. Architecture

### 3.1 High-level components

```text
                         ┌────────────────────────────┐
                         │ Browser / kiosk Chromium   │
                         │ HTML + CSS + ES modules    │
                         │ clock + rendering + timers │
                         └─────────────┬──────────────┘
                                       │ HTTP / JSON
                         ┌─────────────▼──────────────┐
                         │ Rust HTTP adapter (Axum)   │
                         │ static assets + /api/v1    │
                         └─────────────┬──────────────┘
                                       │ use-case calls
                     ┌─────────────────▼───────────────────┐
                     │ APPLICATION                        │
                     │ query dashboard / sync providers   │
                     │ prioritization / cache policy      │
                     └───────────────┬─────────────────────┘
                                     │ ports (traits)
          ┌───────────────┬──────────┼─────────────┬───────────────┐
          ▼               ▼          ▼             ▼               ▼
   CalendarPort      WeatherPort  TransitPort    NewsPort      SnapshotPort
          │               │          │             │               │
   iCloud CalDAV       Open-Meteo  BKK FUTÁR    RSS/Atom      in-memory +
   adapter            adapter     adapter      adapter       disk snapshot
```

Dependencies point inward: domain models know nothing about HTTP, CalDAV, BKK, RSS, Axum, files, or `serde`. Application use cases rely on port traits. Adapters implement these traits and map provider-specific data into normalized domain objects.

Hexagonal architecture should remain **pragmatic**: one Rust crate with modules for MVP (not several microservices or an excessive cargo workspace). Extract crates only when genuinely useful.

### 3.2 Modules and responsibilities

```text
src/
  main.rs                         # startup, shutdown
  config.rs                       # TOML/env loading, validation
  domain/
    mod.rs
    calendar.rs                   # Event, EventStatus
    weather.rs                    # Weather, AirQuality
    transit.rs                    # Departure, TransitAlert
    news.rs                       # Headline
    snapshot.rs                   # DashboardSnapshot, freshness
  application/
    mod.rs
    ports.rs                      # provider/snapshot traits
    refresh.rs                    # polling, backoff, cache
    dashboard.rs                  # display normalization, sorting
  adapters/
    inbound/http.rs               # Axum JSON & static routes
    outbound/caldav.rs            # CalDAV + iCalendar parsing
    outbound/open_meteo.rs
    outbound/bkk_futar.rs
    outbound/rss.rs
    outbound/file_snapshot.rs
  infrastructure/
    http_client.rs                # shared reqwest configuration
    time.rs                       # time source, timezone
    diagnostics.rs                # logging, metrics
web/
  index.html
  css/styles.css
  js/main.js
  js/api.js
  js/clock.js
  js/render.js
  js/timeline.js
  js/format.js
  fonts/...
config/
  mirror.example.toml
fixtures/
  caldav/  weather/  bkk/  rss/
tests/
  integration_*.rs
```

### 3.3 Suggested Rust dependencies

| Purpose | Candidate | Notes |
|---|---|---|
| Async runtime | `tokio` | one runtime and recurring jobs |
| HTTP server | `axum` | JSON and static routes |
| HTTP client | `reqwest` with `rustls` | TLS, timeouts, bounded connection pool |
| Serialization | `serde`, `serde_json`, `toml` | configuration and API |
| Time | `chrono`, `chrono-tz` | timezone + DST; or consistent `time` alternative |
| XML | `quick-xml` | CalDAV XML reports |
| iCalendar | evaluated RFC 5545 parser crate | confirm recurring-event and timezone support; add custom normalization |
| RSS / Atom | `feed-rs` | normalize mixed feeds |
| Logging | `tracing`, `tracing-subscriber` | structured log diagnostics |
| Error types | `thiserror` | domain/application boundaries |

Use a small, audited dependency set; avoid an ORM and SQL DB in MVP. Use Axum's static file handling or compile static files into the executable (`rust-embed` or `include_dir`) for a portable single-binary distribution. Use `async_trait` only if selected Rust/trait-object design requires it.

## 4. Data sources and functional requirements

### 4.1 Calendar — Apple iCloud CalDAV

**Configuration:** CalDAV discovery URL, Apple Account identifier, secret app-specific password, list of calendar IDs/names, event display rules. The discovery entry point can start at `https://caldav.icloud.com/`, but the adapter must follow server-provided principal/calendar-home URLs rather than construct brittle paths.

**Authentication:** account identifier + app-specific password over HTTPS, provided in secret environment variable (`MIRROR_CALDAV_APP_PASSWORD`) or a protected secret file. The Apple Account must have 2FA for app-specific passwords. Do not put any password in static assets, API JSON, log messages, or browser storage.

**Read-only protocol restriction:** allow only `OPTIONS`, `PROPFIND`, `REPORT`, `GET` (as needed for discovery and retrieval). Never issue `PUT`, `POST`, `DELETE`, `MKCOL`, or calendar-altering requests. Note that this is an application restriction; app-specific credentials may not themselves be read-only.

**Sync:**

1. Discover calendar-home collections with `PROPFIND` and resolve selected calendars.
2. Retrieve a bounded current range (e.g., today through next 7 days), preferring `calendar-query REPORT` with requested components.
3. Parse `VEVENT`s, including `UID`, `RECURRENCE-ID`, `RRULE`, `RDATE`, `EXDATE`, `DTSTART`, `DTEND`, `SUMMARY`, `LOCATION`, `DESCRIPTION`, and `STATUS`.
4. Expand recurring series within the requested bounded interval, including exceptions and cancellations. Correctly parse `TZID`, UTC, floating local time, all-day dates, and daylight-saving transitions.
5. Normalize to `CalendarEvent`; deduplicate by `(calendar_id, uid, recurrence_id or occurrence_start)`.
6. Sort upcoming and ongoing instances; hide cancelled events; support configurable all-day treatment and calendar filters.
7. Cache last valid response with last successful sync time. Use CalDAV ETag/sync tokens if supported; otherwise bounded full query.

**Polling target:** every 5 minutes; next event and `T-MINUS`/ongoing states update locally every minute without upstream requests.

**Display:** 5 upcoming events by default, starting with current or next event. Show start time, title, optional location, optional clipped description, and relative countdown. Event description may contain sensitive/private content: configurable field hiding is required. Events beyond today can be shown with day/date prefix if fewer same-day events exist (configurable). Timeline uses **today's** events only.

**Potential calendar issue:** macOS Calendar can aggregate other providers. Only calendars actually stored in the configured iCloud CalDAV account are available through this adapter. Calendars originating in another account require their own adapter or calendar source.

### 4.2 Weather — Open-Meteo

**Provider:** Open-Meteo Forecast API plus Open-Meteo Air Quality API.

**Input:** fixed latitude/longitude, `Europe/Budapest` timezone, metric units. Geolocation permission is unnecessary on a fixed mirror.

**Forecast values:** `temperature_2m`, `relative_humidity_2m` (optional), `weather_code`, `surface_pressure`, `is_day` (optional), optionally `apparent_temperature` and precipitation probability for future iterations. Air quality: `european_aqi`; label explicitly `EU AQI` if shown to avoid confusing it with the US AQI scale.

**Endpoint patterns:**

```text
GET https://api.open-meteo.com/v1/forecast
  ?latitude=...&longitude=...
  &current=temperature_2m,weather_code,surface_pressure
  &timezone=Europe%2FBudapest

GET https://air-quality-api.open-meteo.com/v1/air-quality
  ?latitude=...&longitude=...
  &current=european_aqi
  &timezone=Europe%2FBudapest
```

Map WMO weather codes to a short status text (`FOG`, `RAIN`, etc.); keep localization in the frontend or language-specific presentation mapper. Weather and AQI are **modeled/forecast-source information**, not necessarily locally measured ground truth.

**Polling target:** weather 15 minutes; AQI 60 minutes. Do not update seconds by requesting upstream weather data. Display source attribution in the footer/about view or visible but unobtrusive disclosure.

**Licensing:** Open-Meteo's free hosted API is intended for noncommercial use within provider limits, and the underlying data requires CC BY 4.0 attribution. Review terms before commercial deployment.

### 4.3 Transport — Budapest BKK FUTÁR

**Recommendation:** official **BKK FUTÁR REST/JSON API**, rather than GTFS-Realtime for MVP. It is well suited to querying a handful of configured stops/directions and receiving directly displayable upcoming departures. Obtain an API key from `https://opendata.bkk.hu/`, via Key Management. Never expose the API key to the browser.

**Provider API documentation:** `https://opendata.bkk.hu/data-sources` and linked FUTÁR OpenAPI document. The API key is supplied as `key` in the upstream query. Confirm the active host and current endpoint contract from the official OpenAPI at implementation time; avoid hard-coding obsolete third-party example hosts.

**Typical operation:** `arrivals-and-departures-for-stop` with `stopId`, upcoming-minute window, and optional route filter, subject to actual OpenAPI schema. Stop and route IDs are resolved during configuration (e.g. `BKK_F...`), not guessed from display names.

**Configuration per monitored service:**

- `stop_id` (required), `route_ids` (optional list), direction / `headsign` filter (optional).
- `label` (optional friendly stop name).
- `walking_minutes` offset (default 0), so a departure too close to catch can be excluded or de-emphasized.
- `primary` ranking and `max_results`.
- Optional display of platform/stop position, real-time delay, and service alerts where provider fields support them.

**Normalization:** each departure has `stop_id`, `route_short_name`, `destination`, `scheduled_time`, `predicted_time?`, `delay_seconds?`, `realtime` boolean, `platform?`, `trip_id?`, and any relevant alerts. Countdown is derived client-side from `predicted_time ?? scheduled_time` and local current time.

**Display:** largest primary row highlights the earliest relevant departure; secondary rows show other configured services. Do not invent platform numbers or assert `ON TIME` if the provider only returned timetable data. Use `SCHEDULED` for non-realtime predictions. If cancelled, mark the record and exclude it from the primary actionable departure.

**Polling target:** 20–30 seconds for configured stops; local countdown recalculates every 15 seconds. Respect BKK key limits/backoff; official feed documentation says realtime data refreshes approximately every 10 seconds or more, so polling more frequently than every 5 seconds is not advised. An API key and required attribution `Data source: BKK Zrt., CC BY 4.0` apply.

**Future alternate:** GTFS + GTFS-Realtime adapters if offline timetable fallback, broader network operations, or complex multimodal comparisons become a requirement. Avoid downloading the full feeds in MVP.

### 4.4 News — RSS and Atom

**Input:** explicit list of user-supplied HTTP(S) feed URLs; no hard-coded news source selection. Up to 10 configured feeds in MVP, configurable priority/order and per-feed item limit.

**Processing:** download feeds with conditional requests (`ETag`, `Last-Modified`) where supported; parse RSS 2.0 and Atom; normalize title, publication time, source, URL, GUID; sanitize/remap HTML entities and strip markup; deduplicate by stable GUID or canonical URL and fallback normalized title; sort chronologically; optional configurable keyword exclusions. Treat all feed text as untrusted text (`textContent`, never `innerHTML`).

**Display:** two latest headline rows by default; short local timestamp and one-line ellipsis, with source name optionally shown. If publication time missing, indicate source without a fabricated timestamp. Auto-rotation is deferred to a later iteration; preserve the screen's low-distraction intent.

**Polling target:** 15 minutes, respecting conditional cache headers and individual feed errors. Failed feed must not block other feeds. Do not require image download or article scraping.

## 5. API design

Single-origin UI and REST API. Default bind `127.0.0.1:3000` for same-device kiosk; configurable trusted LAN bind for remote browser display.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/` | static dashboard |
| `GET` | `/assets/*` | bundled CSS/JS/fonts |
| `GET` | `/api/v1/dashboard` | combined current view, recommended MVP endpoint |
| `GET` | `/api/v1/health` | liveness + provider status (minimal exposure) |

No public write endpoints in MVP. CORS remains disabled unless explicitly configured for a specific trusted origin. If a management API is ever introduced, it must be separately authenticated.

**Example `GET /api/v1/dashboard` response:**

```json
{
  "generated_at": "2026-10-18T13:10:00Z",
  "timezone": "Europe/Budapest",
  "calendar": {
    "updated_at": "2026-10-18T13:05:00Z",
    "status": "fresh",
    "events": [
      {
        "id": "cal1:uid123:2026-10-18T14:00:00+02:00",
        "title": "Studio session",
        "start": "2026-10-18T14:00:00+02:00",
        "end": "2026-10-18T15:30:00+02:00",
        "all_day": false,
        "location": "Studio A",
        "description": "Recording"
      }
    ]
  },
  "weather": {
    "updated_at": "2026-10-18T13:00:00Z",
    "status": "fresh",
    "temperature_c": 11.2,
    "condition_code": 45,
    "pressure_hpa": 1014,
    "european_aqi": 22
  },
  "transit": {
    "updated_at": "2026-10-18T13:09:45Z",
    "status": "fresh",
    "departures": [
      {
        "stop_id": "BKK_EXAMPLE",
        "route": "4",
        "destination": "Example destination",
        "scheduled_time": "2026-10-18T13:21:00+02:00",
        "predicted_time": "2026-10-18T13:20:00+02:00",
        "realtime": true,
        "delay_seconds": -60,
        "platform": null
      }
    ]
  },
  "news": {
    "updated_at": "2026-10-18T13:00:00Z",
    "status": "fresh",
    "items": [
      {
        "id": "feed1:item1",
        "source": "Example feed",
        "title": "Example headline",
        "published_at": "2026-10-18T12:42:00Z"
      }
    ]
  }
}
```

All sample values are illustrative and not live source data. Status is one of `fresh`, `stale`, `unavailable`. `updated_at` means successful provider ingestion; the frontend uses this to display age. Errors are reported as non-sensitive enum codes if required, not secrets or upstream URL query strings.

**HTTP caching:** `Cache-Control: no-store` for private dashboard JSON to avoid unwanted client/proxy persistence; static assets use hashed names and long cache lifetimes. Client fetches full dashboard every 30 seconds; clock/relative times refresh independently. `AbortController` prevents overlap. Optionally return `ETag`/`304` later; not needed for the small MVP JSON payload.

## 6. Frontend implementation

**Technology:** semantic HTML, normal CSS, native ES modules. No build step needed for day-to-day development. No React/Vue, jQuery, Tailwind runtime, icon kits, chart libraries, or client data-store framework.

### 6.1 Modules

- `api.js`: fetch JSON with timeout, validate basic shape, last-success handling.
- `clock.js`: display local time and seconds, safely correct for timer drift and clock changes.
- `format.js`: `Intl.DateTimeFormat`, `Intl.RelativeTimeFormat`, locale-specific weather text, safe string formatting.
- `timeline.js`: map local event instants to percentages of configured visible day span and generate accessible markers.
- `render.js`: DOM updates via `createElement`, `replaceChildren`, and `textContent`; keyed reuse optional if tests show benefit.
- `main.js`: bootstrap, polling orchestration, visibility handling, UI error states.

### 6.2 Clock, timeline, and event rules

- Render clock immediately before first API response.
- Use `Date` and `Intl` with the configured IANA timezone; never hardcode UTC offset or rely on `CST`.
- Update seconds every second; schedule using current wall clock rather than accumulating `setInterval` ticks to avoid drift.
- Every minute update current-time ruler position, event urgency labels, and ongoing status.
- For day ruler with configurable `07:00`–`22:00`: `x = clamp((instant - dayStart) / (dayEnd - dayStart), 0, 1) * 100%`. Event block width is computed from clamped start/end; events spanning midnight are clipped appropriately.
- Honor daylight-saving changes using zoned day boundaries rather than assuming a local day is always 24 hours.
- If all-day events are shown, render them as separate tags, not misleading timed bars.
- Avoid overlap clutter: event bars may overlap on the same horizontal track, with combined opacity or stacked narrow strips.
- Active event highlight: ongoing gets priority over upcoming only when configured; otherwise next upcoming imminent event wins.

### 6.3 Transit countdown

- Recompute every 15 seconds from absolute time; display minute countdown rounded conservatively (`ceil` for future departures) so `0m` is not shown prematurely.
- Mark negative countdown as departed and remove at next render; retain cancelled records only if configured.
- Subtract `walking_minutes` to indicate practical departure readiness; distinguish `leaves in 8m` from `leave home in 4m` if walking threshold is enabled.
- Prioritize configured stop/route interests over purely earliest time.

### 6.4 Presentation/accessibility

- High contrast as shown; text has no essential meaning conveyed only by color.
- No flashing alerts; if the accent pulse in the reference is reproduced, provide `prefers-reduced-motion` override.
- Truncate long descriptions and headlines; never allow overflow to break the viewport.
- Custom local font loading with fallback system fonts; app must load with network off when cache exists.
- Provide CSS toggles for scanlines, density, font scale, highlight intensity, and burn-in mitigation.
- Optional tiny, periodic pixel shift (1–3 CSS pixels) for OLED, with care not to make text visibly jump; physical OLED protection still depends on hardware.

## 7. Scheduling, caching and fault handling

| Source | Provider refresh | Last-good display policy | Failure strategy |
|---|---:|---|---|
| Calendar | 5 min | retain for 24h, marked stale after 15m | exponential backoff to 30m |
| Weather | 15 min | retain for 3h, stale after 45m | backoff, independent AQI call |
| AQI | 60 min | retain for 6h, stale after 2h | independent from weather |
| BKK | 30 sec | max 2m for actionable predictions, stale after 60 sec | backoff up to 5m |
| RSS | 15 min | retain for 24h, stale after 60m | per-feed failures isolated |

These are defaults, configurable and subject to upstream provider rules. In particular, **old transit predictions must not look live**. After the actionable age threshold, show an availability warning instead of asserting a countdown is accurate; a backend-less clock may continue from cached timestamps but must label them `SCHEDULED/STALE`.

### 7.1 Startup and synchronization

- Load config, validate required fields, initialize last-known snapshot, start HTTP server promptly, then sync providers asynchronously.
- Each provider runs on an independent cancellation-aware Tokio task, with concurrency caps for multiple RSS feeds.
- Requests use explicit connect/read/overall timeouts and bounded response sizes.
- Exponential backoff with jitter and a maximum interval; reset when successful. HTTP 429 with `Retry-After` should be honored.
- Atomic state replacement: do not publish half-parsed or partly refreshed results from a single provider.
- Persist most recent normalized snapshot to a protected local JSON file via write-temp + fsync/rename (reasonable best effort); avoid disk writes for every 30-second transit refresh if storage wear matters.
- For privacy, use restrictive permissions; allow persistence of personal calendar data to be disabled entirely.
- System time synchronization should be enabled on the host. Show a diagnostic indicator if local clock becomes suspect where detectable.

## 8. Configuration

A static TOML configuration, validated at startup. Secrets are only environment variables or protected secret files.

```toml
[server]
bind = "127.0.0.1:3000"

[display]
locale = "hu-HU"
timezone = "Europe/Budapest"
ui_scale = 1.0
scanlines = true
visible_events = 5
visible_news = 2
day_start = "07:00"
day_end = "22:00"
urgent_event_minutes = 45

[calendar]
enabled = true
url = "https://caldav.icloud.com/"
username = "your-account@example.com"
calendar_names = ["Personal", "Work"]
refresh_seconds = 300
show_description = false
show_location = true
show_all_day = true
lookahead_days = 7
# Password from MIRROR_CALDAV_APP_PASSWORD

[weather]
enabled = true
provider = "open_meteo"
latitude = 47.4979     # example: Budapest city center, replace as desired
longitude = 19.0402
refresh_seconds = 900
show_european_aqi = true

[transit]
enabled = true
provider = "bkk_futar"
refresh_seconds = 30
# API key from MIRROR_BKK_API_KEY

[[transit.stops]]
id = "BKK_REPLACE_WITH_REAL_STOP_ID"
label = "Home stop"
route_ids = []
walking_minutes = 3
priority = 1

[news]
enabled = true
refresh_seconds = 900
max_headlines = 2

[[news.feeds]]
name = "Example"
url = "https://example.org/feed.xml"
```

`calendar_names` are resolved through discovery; unresolved names produce a visible warning and log diagnostic. A more robust follow-up is to support immutable calendar URLs/IDs in configuration to avoid duplicate names. Stop IDs must be verified using BKK discovery endpoints or the OpenData spec. Configuration changes initially require a service restart; live reload is optional post-MVP.

## 9. Security and privacy

- Access Apple CalDAV and BKK only server-side. The frontend never receives provider credentials.
- HTTP bind to loopback by default; for a separate kiosk and server use a trusted LAN plus firewall, optionally a local reverse proxy with TLS/basic authentication or VPN. Do not expose the dashboard publicly.
- Strict upstream HTTPS, bounded redirects and allowed provider hostnames; RSS user-controlled URLs require SSRF safeguards (reject local/internal IP ranges and revalidate redirect targets) if edited through any remotely accessible UI.
- Escape untrusted CalDAV titles, descriptions, RSS titles, and transit messages by rendering as DOM text nodes.
- Minimal security headers (`Content-Security-Policy` restricting scripts to self, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`). No third-party scripts, cookies, analytics, or trackers.
- Secret redaction in traces. Query parameters may include BKK key, so avoid logging complete upstream URLs.
- Calendar details and news interests are private; limit dashboard API access on shared networks and snapshot retention on disk.
- No silent logging of entire event bodies unless explicitly enabled for local debugging.

## 10. Deployment

**Recommended:** Raspberry Pi 4/5 or similar small Linux mini-PC, single Rust binary compiled for the host architecture, service managed by `systemd`, full-screen Chromium in kiosk mode pointing to `http://127.0.0.1:3000`.

System startup sequence:

1. Start networking/time synchronization services.
2. Start `magic-mirror.service` (Rust binary + config + env file).
3. Launch kiosk browser with a dedicated user/session and disable screen blanking where appropriate.
4. Restart crashed services via `systemd`; restore cached snapshot immediately, then refresh online.

For embedded low-resource devices, consider running Chromium on the display host and the Rust service on a separate LAN machine; the browser is likely to dominate total memory usage. Avoid claiming a guaranteed memory target until the selected board, HTTP stack, libraries, and kiosk browser are benchmarked.

**Performance budgets (targets to verify):**

- Server warm idle RAM: < 50 MB desirable, < 100 MB acceptable depending on TLS/XML/parser/runtime configuration.
- Rust binary: optimize release profile and strip symbols; report actual file size in CI rather than asserting a fixed threshold.
- Compressed static frontend (JS + CSS, excluding fonts): target < 50 KB.
- Combined dashboard response: typically < 25 KB.
- No continuous high-frequency render loop, canvases, charts, or animations.
- Kiosk view usable from local assets within 2 seconds after HTTP server is ready; provider sync is not on critical rendering path.

## 11. Testing and acceptance criteria

### 11.1 Unit tests

- Parse and normalize events (UTC, TZID, all-day, recurring exceptions, DST change, cancellation, overlapping events).
- Timeline bounds and clipping across midnight.
- Priority next-event calculation (`upcoming`, `ongoing`, `expired`).
- Departure filtering by stop, route, destination, walking offset, cancellation, realtime versus scheduled.
- Weather-code mapping, including unknown value fallback; differentiate EU AQI and US AQI.
- RSS duplicates, missing dates, malformed feeds, HTML content sanitization.
- Fresh/stale/unavailable transitions and retry schedule.

### 11.2 Integration tests

- Fixture-driven fake upstream HTTP servers for CalDAV XML/ICS, Open-Meteo JSON, BKK JSON, and RSS/Atom.
- Simulated 401/403 credentials errors, 429 rate limits, upstream 500, timeout, invalid JSON/XML, partial outages.
- Ensure all calendar adapter methods use only approved read methods.
- Snapshot JSON contract serialization and absence of secrets.
- Browser smoke test from local server, optional Playwright in development/CI only (not runtime dependency).

### 11.3 Acceptance tests

1. Mirror boots with no internet and still shows correct local time and last-known-good data where permitted, visibly marked stale.
2. An iCloud event edited on another device appears after next scheduled CalDAV sync.
3. Weekly recurring event with a single modified occurrence renders exactly once at the overridden time.
4. A `Europe/Budapest` DST transition does not distort local displayed event time.
5. Configured BKK stop shows at least one actual returned departure when available; filtered routes/directions behave correctly.
6. Realtime vs scheduled departure data are distinctly represented, and outdated predictions are not advertised as current.
7. One broken RSS feed does not remove items from other feeds.
8. On an actual portrait panel, main clock, upcoming event, primary departure, and headlines remain legible and do not overflow.
9. Browser developer tools reveal no iCloud/BKK secret and no third-party script requests.
10. Restart restores cached data promptly and resumes all refresh tasks.

## 12. Development plan

### Phase 0 — Design extraction and shell

- Port `code.html` into `index.html` and `styles.css`, removing Tailwind CDN.
- Implement static JSON fixtures, local fonts, native clock, event card rendering, ruler, transit, news.
- Achieve high-fidelity screenshot comparison at target portrait resolution.

### Phase 1 — Rust foundation

- Create Rust crate, domain entities, port traits, config validation, Axum routes, combined snapshot model.
- Implement file-backed last-good cache, provider-independent refresh scheduler, health diagnostics.
- Add fixture tests and contract validation.

### Phase 2 — Providers

- Implement Open-Meteo forecast / AQI (simplest).
- Implement RSS/Atom importer.
- Implement BKK FUTÁR for user-configured stops.
- Implement Apple CalDAV discovery, bounded query, and robust recurring event expansion (highest calendar complexity).

### Phase 3 — Reliability and deployment

- Offline/stale states, cancellation, retries, privacy/security hardening.
- Systemd service, kiosk autostart, device benchmarking.
- Real-device screenshot review and spacing adjustments.

## 13. Decisions to resolve before implementation

These are configuration choices rather than blockers to the core architecture:

1. **Display hardware:** screen pixel resolution, diagonal size, Raspberry Pi versus PC, and whether server/browser share a machine.
2. **BKK:** exact stop IDs, desired routes and directions, walking time, and whether a single primary line or fastest relevant departure wins.
3. **Apple:** calendar names or stable IDs, treatment of work/all-day events, hiding event descriptions and private events.
4. **RSS:** user-selected feed URLs and whether headlines rotate or remain static.
5. **Language:** Hungarian or English labels; 24-hour time is the default either way.
6. **Privacy:** whether recent event/news data may be written to disk on the display device.

## 14. Sources / external interface references

- Apple app-specific passwords: https://support.apple.com/en-us/102654
- Apple CalDAV product documentation: https://developer.apple.com/documentation/devicemanagement/accountcaldav
- Open-Meteo forecast API: https://open-meteo.com/en/docs
- Open-Meteo air quality API: https://open-meteo.com/en/docs/air-quality-api
- Open-Meteo licensing and pricing: https://open-meteo.com/en/pricing
- BKK official developer portal: https://opendata.bkk.hu/
- BKK source descriptions and API documentation: https://opendata.bkk.hu/data-sources
- BKK existing mirror module (implementation inspiration, not required): https://github.com/balassy/MMM-Futar
- iCalendar RFC 5545: https://www.rfc-editor.org/rfc/rfc5545
- CalDAV RFC 4791: https://www.rfc-editor.org/rfc/rfc4791

---

**Preferred MVP architecture decision:** one Rust process, in-memory provider snapshots plus optional atomic JSON persistence, single combined read-only dashboard endpoint, and a static native-JS frontend. Keep hexagonal boundaries in code without adopting unnecessary infrastructure.
