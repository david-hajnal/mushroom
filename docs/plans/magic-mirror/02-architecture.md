# Architecture: Magic Mirror

## Fit
Greenfield — no existing services. One Rust binary (single crate, hexagonal layout) owns everything: it serves the static frontend, exposes the JSON API, polls upstream providers, and keeps a last-known-good snapshot. The browser is a dumb client: it renders and updates relative times locally. No database, no framework, no build step.

Layout (dependencies point inward — domain knows nothing of HTTP/CalDAV/BKK/RSS/serde):

```
domain/        Event, Weather, AirQuality, Departure, Headline, DashboardSnapshot
application/   ports (traits), refresh (polling/backoff/cache), dashboard (normalization/sort)
adapters/
  inbound/http        Axum: static + /api/v1
  outbound/caldav     CalDAV discovery + iCalendar parse
  outbound/open_meteo forecast + air quality
  outbound/bkk_futar  arrivals-and-departures
  outbound/rss        RSS 2.0 + Atom
  outbound/file_snapshot  last-good JSON persistence
infrastructure/ http_client (reqwest/rustls), time (chrono-tz), diagnostics (tracing)
web/           index.html, css/, js/ (native ES modules), fonts/
config/        mirror.example.toml
```

## Endpoints
| Method | Path | Purpose |
|---|---|---|
| GET | `/` | static dashboard (index.html) |
| GET | `/assets/*` | bundled CSS/JS/fonts (long cache, hashed names) |
| GET | `/api/v1/dashboard` | combined current view — the single MVP data endpoint |
| GET | `/api/v1/health` | liveness + per-provider status (minimal exposure) |

No write endpoints. CORS disabled by default. `Cache-Control: no-store` on dashboard JSON.

## Data
No SQL database. State is in-memory provider snapshots, atomically replaced on each successful refresh, plus an optional last-good JSON file (write-temp + fsync + rename) for restart recovery.

- **Calendar:** normalized `CalendarEvent` (id, title, start, end, all_day, location, description). Dedup key `(calendar_id, uid, recurrence_id|occurrence_start)`. Bounded query: today → +N days (default 7).
- **Weather:** `Weather` (temperature_c, condition_code, pressure_hpa) + `AirQuality` (european_aqi). Modeled/forecast, not ground truth.
- **Transit:** `Departure` (stop_id, route, destination, scheduled_time, predicted_time?, realtime, delay_seconds?, platform?).
- **News:** `Headline` (id, source, title, published_at?).
- **Snapshot:** `DashboardSnapshot` = the four sections above, each with `updated_at` + `status` (`fresh`|`stale`|`unavailable`).

Queries that hit the snapshot: one read per `/api/v1/dashboard` — assemble the four sections, stamp `generated_at`, return. No joins, no writes from the API path.

## Flow
Main path (per provider, independent Tokio tasks):

```
startup: load+validate config → init last-good snapshot → start HTTP server (promptly)
        → spawn one refresh task per enabled provider
refresh task: fetch upstream → parse → normalize to domain → atomic swap into snapshot
              on error: keep last-good, mark stale, exponential backoff + jitter (honor Retry-After)
API: GET /api/v1/dashboard → read current snapshot → serialize → respond
browser: fetch /api/v1/dashboard every 30s (AbortController, no overlap)
         clock/relative times/timeline recompute locally every 1s / 15s / 1min
```

Per-source refresh cadence (configurable, independent): calendar 5m, weather 15m, AQI 60m, transit 30s, news 15m.

## External
- **Apple iCloud CalDAV** — read-only (OPTIONS/PROPFIND/REPORT/GET only). Secret: `MIRROR_CALDAV_APP_PASSWORD` (env). Requires 2FA + app-specific password.
- **Open-Meteo** — forecast + air-quality APIs. No key. Noncommercial use; CC BY 4.0 attribution.
- **BKK FUTÁR** — arrivals-and-departures. Secret: `MIRROR_BKK_API_KEY` (env, from opendata.bkk.hu). Attribution `Data source: BKK Zrt., CC BY 4.0`.
- **RSS/Atom** — user-supplied feed URLs. No key. SSRF safeguards (reject internal IPs, revalidate redirects).
- Env var NAMES only; values never in code, logs, static assets, or API JSON.
