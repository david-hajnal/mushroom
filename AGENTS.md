# AGENTS.md

## What this is

Magic Mirror is an always-on, read-only dashboard for a portrait display. It shows calendar events, weather and air quality, transit departures, and news headlines. One Rust binary (axum + tokio) serves the static frontend and a small JSON API.

## Layout

- `src/` — the server. One module per concern: `main.rs` (router, security headers), `api.rs` (endpoints, response types), `config.rs` (config loading and validation), `weather.rs`, `news.rs`, `calendar.rs` (providers), `backoff.rs` (retry delays), `persistence.rs` (last-good snapshot).
- `static/` — the frontend: `index.html`, `assets/app.js`, `assets/style.css`, self-hosted fonts. No CDN, no build step.
- `config/` — `mirror.example.toml` (template) and `mirror.toml` (local, gitignored).
- `deploy/` — systemd units, kiosk files, env template, budget script.
- `fixtures/caldav/` — iCalendar test fixtures.
- `data/` — runtime snapshot and budget report.
- `docs/plans/magic-mirror/` — approved product, architecture, and slice plans. The source of truth for decisions.
- `.agent/` — orchestrator tasks and run logs.

## Commands

- Build: `cargo build`
- Run: `cargo run` (binds `127.0.0.1:3000` by default)
- Test: `cargo test` (in-process, no network, no ports)
- Release: `cargo build --release`

## Conventions

- Provider pattern: each provider has a `*Cache` with an `RwLock<Option<Snapshot>>`, a `Resolved<T>` enum (`Fresh` / `Stale` / `Unavailable`), and a `build_*_section()` function. Follow this when adding a provider.
- Never return 500 for a provider failure. A failed section degrades to its last-good data with a `stale` or `unavailable` status. The section status is the worst of its providers.
- Config precedence: env vars > `config.toml` > built-in defaults. A missing config file is fine; an invalid one fails fast with an actionable error.
- Secrets are env-only (for example `MIRROR_CALDAV_APP_PASSWORD`). Never in the config file, API JSON, or logs.
- CalDAV is strictly read-only: `GET`, `PROPFIND`, and `REPORT` only.
- Snapshot writes are atomic: temp file, fsync, and rename.
- No new dependencies without a strong reason.
- Keep the `GET /api/v1/dashboard` response shape stable. The frontend depends on it.

## Current state

- Slices 1, 2, 3, 5, 6, and 7 are implemented.
- Slice 4 (transit) is not implemented in code yet: the transit section still returns mock data.
- `GET /api/v1/health` hardcodes all providers as `fresh`. It does not reflect real cache state.
- `docs/plans/magic-mirror/00-status.md` is stale: it says "no code yet" and leaves all slices unchecked.
