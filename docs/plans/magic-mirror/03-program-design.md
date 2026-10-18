# Program Design: Magic Mirror

## Files
```
Cargo.toml                     deps: tokio, axum, reqwest(rustls), serde, serde_json, toml,
                               chrono, chrono-tz, quick-xml, ics (evaluated), feed-rs,
                               tracing, tracing-subscriber, thiserror
src/main.rs                    startup: load config → init snapshot → serve → spawn refresh tasks; graceful shutdown
src/config.rs                  Config::load(path) → validated Config; per-source refresh_seconds; secret refs
src/domain/mod.rs              re-exports
src/domain/calendar.rs         CalendarEvent, EventStatus
src/domain/weather.rs          Weather, AirQuality
src/domain/transit.rs          Departure
src/domain/news.rs             Headline
src/domain/snapshot.rs         DashboardSnapshot, SectionStatus, freshness helpers
src/application/ports.rs       CalendarPort, WeatherPort, TransitPort, NewsPort, SnapshotStore (traits)
src/application/refresh.rs     RefreshTask, Backoff, per-source scheduler
src/application/dashboard.rs   assemble DashboardSnapshot from section states; sort/prioritize
src/adapters/inbound/http.rs   Axum Router, routes, JSON serializers, static serving, security headers
src/adapters/outbound/caldav.rs    CalDAV discovery + REPORT + iCalendar parse → CalendarEvent
src/adapters/outbound/open_meteo.rs forecast + air-quality → Weather/AirQuality
src/adapters/outbound/bkk_futar.rs arrivals-and-departures → Departure
src/adapters/outbound/rss.rs       feed fetch + parse → Headline
src/adapters/outbound/file_snapshot.rs  load/save last-good JSON (atomic)
src/infrastructure/http_client.rs    shared reqwest (timeouts, pool, rustls)
src/infrastructure/time.rs     Clock trait, Europe/Budapest zone, now()
src/infrastructure/diagnostics.rs    tracing setup, secret redaction
web/index.html                 semantic shell, data-driven containers
web/css/styles.css             authored CSS (no Tailwind), tokens from DESIGN.md
web/js/main.js                 bootstrap, poll orchestration, visibility, error states
web/js/api.js                  fetch + timeout + shape validation + last-success
web/js/clock.js                wall-clock time, drift-safe
web/js/format.js               Intl.DateTimeFormat/RelativeTimeFormat, weather text
web/js/timeline.js             instant→% mapping, markers, clipping
web/js/render.js               DOM via createElement/textContent (no innerHTML for untrusted)
web/fonts/                     self-hosted Chivo + Space Mono (OFL)
config/mirror.example.toml     full example config
fixtures/caldav/ weather/ bkk/ rss/   test fixtures
tests/integration_dashboard.rs, integration_providers.rs
```

## Types & signatures
```rust
// domain/calendar.rs
pub struct CalendarEvent {
    pub id: String,            // "cal:uid:occurrence"
    pub title: String,
    pub start: DateTime<FixedOffset>,
    pub end: Option<DateTime<FixedOffset>>,
    pub all_day: bool,
    pub location: Option<String>,
    pub description: Option<String>,
}
pub enum EventStatus { Ongoing, Upcoming, Expired }

// domain/weather.rs
pub struct Weather { pub temperature_c: f32, pub condition_code: u16, pub pressure_hpa: u16 }
pub struct AirQuality { pub european_aqi: u16 }

// domain/transit.rs
pub struct Departure {
    pub stop_id: String,
    pub route: String,
    pub destination: String,
    pub scheduled_time: DateTime<FixedOffset>,
    pub predicted_time: Option<DateTime<FixedOffset>>,
    pub realtime: bool,
    pub delay_seconds: Option<i32>,
    pub platform: Option<String>,
}

// domain/news.rs
pub struct Headline { pub id: String, pub source: String, pub title: String, pub published_at: Option<DateTime<FixedOffset>> }

// domain/snapshot.rs
pub enum SectionStatus { Fresh, Stale, Unavailable }
pub struct Section<T> { pub updated_at: Option<DateTime<Utc>>, pub status: SectionStatus, pub data: T }
pub struct DashboardSnapshot {
    pub generated_at: DateTime<Utc>,
    pub timezone: String,
    pub calendar: Section<Vec<CalendarEvent>>,
    pub weather: Section<Weather>,
    pub air_quality: Section<AirQuality>,
    pub transit: Section<Vec<Departure>>,
    pub news: Section<Vec<Headline>>,
}

// application/ports.rs
#[async_trait] pub trait CalendarPort: Send + Sync { async fn fetch(&self, range: TimeRange) -> Result<Vec<CalendarEvent>, ProviderError>; }
#[async_trait] pub trait WeatherPort: Send + Sync { async fn fetch(&self) -> Result<(Weather, Option<AirQuality>), ProviderError>; }
#[async_trait] pub trait TransitPort: Send + Sync { async fn fetch(&self, stops: &[StopConfig]) -> Result<Vec<Departure>, ProviderError>; }
#[async_trait] pub trait NewsPort: Send + Sync { async fn fetch(&self, feeds: &[FeedConfig]) -> Result<Vec<Headline>, ProviderError>; }
pub trait SnapshotStore: Send + Sync {
    fn load(&self) -> Option<DashboardSnapshot>;
    fn save(&self, snap: &DashboardSnapshot) -> io::Result<()>;
    fn current(&self) -> DashboardSnapshot;
    fn replace_section(&self, /* section id */, /* updated_at */, /* status */, /* data */);
}

// application/refresh.rs
pub struct Backoff { /* base, max, jitter */ }
impl Backoff { pub fn next_delay(&mut self) -> Duration; pub fn reset(&mut self); }
pub struct RefreshTask<P> { /* port, interval, backoff, store */ }
impl<P> RefreshTask<P> { pub async fn run(self, cancel: CancellationToken); }

// application/dashboard.rs
pub fn prioritize_events(events: &[CalendarEvent], now: DateTime<FixedOffset>, urgent_min: u32) -> Vec<CalendarEvent>;
pub fn prioritize_departures(deps: &[Departure], now: DateTime<FixedOffset>, walking_min: u32) -> Vec<Departure>;

// adapters/inbound/http.rs
pub fn build_router(state: AppState) -> Router;
async fn get_dashboard(State(state): State<AppState>) -> Json<DashboardSnapshot>;
async fn get_health(State(state): State<AppState>) -> Json<HealthReport>;

// infrastructure/time.rs
pub trait Clock: Send + Sync { fn now(&self) -> DateTime<Utc>; }
```

## Call stack
**Startup:** `main` → `config::Config::load` → `file_snapshot::load` (seed) → `http::build_router` → `axum::serve` → for each enabled source spawn `refresh::RefreshTask::run`.

**Refresh (per source):** `RefreshTask::run` → loop { `port.fetch` → `dashboard::prioritize_*` → `store.replace_section` → `store.save` (best-effort) → sleep(interval) } ; on `Err` → `backoff.next_delay` + mark `Stale`.

**API read:** browser `GET /api/v1/dashboard` → `http::get_dashboard` → `store.current` → `Json<DashboardSnapshot>` → browser `api.js` → `render.js`/`timeline.js`/`clock.js`.

## Test plan
- `calendar_parse_utc_tzid_allday` — each time form normalizes to correct `DateTime<FixedOffset>`.
- `calendar_recurring_exception_renders_once` — weekly series + one overridden occurrence → exactly one event at the override time.
- `calendar_dst_transition_no_distortion` — event across a Budapest DST boundary keeps local wall time.
- `calendar_cancelled_hidden` — `STATUS:CANCELLED` excluded.
- `timeline_bounds_and_midnight_clip` — instant→% clamps to [0,1]; overnight event clips at day edges.
- `prioritize_events_upcoming_ongoing_expired` — correct ordering + `urgent` flag within threshold.
- `transit_filter_stop_route_walking_cancelled` — only configured, catchable, non-cancelled departures; primary = earliest relevant.
- `transit_realtime_vs_scheduled` — `realtime=false` → `SCHEDULED`, never `ON TIME`.
- `weather_code_map_unknown_fallback` — known codes map; unknown → generic label; EU AQI ≠ US AQI.
- `rss_dedup_missing_date_malformed` — dup GUIDs collapse; missing date → no fabricated time; malformed feed isolated.
- `freshness_transitions` — fresh→stale→unavailable at configured ages; backoff schedule correct.
- `integration_dashboard_contract` — full snapshot serializes; no secrets/URLs leak; shape matches spec §5.
- `integration_provider_outages` — 401/403, 429 (honor Retry-After), 500, timeout, bad JSON/XML, partial outage → last-good retained, marked stale.
- `caldav_read_only_methods_only` — adapter issues only OPTIONS/PROPFIND/REPORT/GET.

## Least confident decisions
1. **iCalendar parser crate** — spec says "evaluated RFC 5545 parser." Recurring-event expansion + DST is the highest-risk piece. Picking the crate (or hand-rolling expansion) is the single biggest unknown. Want a decision before implementation.
   - **Reference implementation available:** `/Users/kaszperek/repos/happening` has a working Rust CalDAV client in `backend/src/caldav/` — `http.rs` (discovery: `/.well-known/caldav` → calendar-home → collections via PROPFIND), `ical.rs` (iCalendar parse), `query.rs` (REPORT), `auth.rs`, `types.rs`. Use it to discover/validate the Apple CalDAV flow (discovery URLs, REPORT body, recurring-event handling) before writing our own adapter. It is a two-way sync server; we only need its read/discovery + parse patterns.
2. **`async_trait` necessity** — listed conditionally. If we avoid trait objects (use generics in `RefreshTask<P>`), we may drop it. Confirm we don't need dyn dispatch.
3. **SnapshotStore `replace_section` signature** — currently a loose "section id + data" sketch. Needs a concrete, type-safe shape (enum of sections vs. per-section methods) before coding.
4. **Transit primary rule** — "earliest relevant" vs "configured primary line wins" is still an open product decision (§13). Affects `prioritize_departures`.
5. **Disk persistence of personal calendar data** — privacy decision (§13) not yet made; `file_snapshot` save path depends on it.
6. **Single combined endpoint vs. per-source endpoints** — spec commits to one `/api/v1/dashboard`. Confirm we don't want per-source endpoints for independent caching later.
