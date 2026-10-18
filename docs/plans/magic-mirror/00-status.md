# Status: Magic Mirror

- Gate 1 — Product: APPROVED 2026-10-18
- Gate 2 — Architecture: APPROVED 2026-10-18
- Gate 3 — Program Design: APPROVED 2026-10-18
- Gate 4 — Slice plan: pending

## Slices
- [ ] Slice 1 — tracer bullet: shell + hardcoded `/api/v1/dashboard` contract, rendered in browser
- [ ] Slice 2 — real weather + AQI (Open-Meteo)
- [ ] Slice 3 — real news (RSS/Atom)
- [ ] Slice 4 — real transit (BKK FUTÁR)
- [ ] Slice 5 — real calendar (CalDAV, recurring/DST)
- [ ] Slice 6 — reliability (stale/offline/backoff/persistence/security)
- [ ] Slice 7 — deployment + polish (systemd/kiosk/budgets/real device)

## Notes for a fresh session
- Existing technical spec: `docs/techplan/magic-mirror-technical-spec.md` (v1.0) — covers product, architecture, providers, API, config, security, testing. Treat as input material; the gate docs in this folder are the source of truth for decisions.
- Design reference: `docs/techplan/stitch_dark_industrial_magic_mirror/` (DESIGN.md, code.html, screen.png) — approved visual direction: pure black, Chivo + Space Mono, amber #ffba3d accent, 0px radius, scanlines.
- Repo is otherwise empty: no code yet.
- Apple CalDAV reference: `/Users/kaszperek/repos/happening` has a working Rust CalDAV client in `backend/src/caldav/` (discovery `http.rs`, iCal parse `ical.rs`, REPORT `query.rs`) — use to discover/validate the Apple CalDAV flow before writing our adapter.
- Open product decisions (spec §13): display hardware, BKK stop IDs/routes, calendar names, RSS feed URLs, language (hu-HU vs en-GB), disk persistence of personal data.
