# Product: Magic Mirror

## Problem
I want to glance at the mirror on my wall and immediately know four things — what's next on my calendar, what the weather is, which tram or bus I can still catch from my stop, and what's new in the feeds I follow — without touching a phone or a computer. Today that means pulling up apps, and the information is scattered across four of them.

## Success metric
Within 5 minutes of the real world changing (a calendar edit, a new departure, a new headline), the mirror shows it — and it does so at least 99% of waking hours with zero user interaction. Measured by comparing the mirror's displayed next-event and next-departure against the source data at random times over a two-week trial.

## Requirements
- **Configurable refresh per feature:** each data source — calendar, weather, air quality, transit, news — has its own independently configurable refresh interval. Changing one does not affect the others. Sensible defaults ship out of the box (transit most frequent, air quality least), and the user can tune each to taste or to a provider's rate limits.

## Announcement — the blog post before the feature
Your mirror now knows your day. It shows your next five calendar events with a live countdown, today's weather and air quality, the next departure you can actually catch from your configured stop, and the latest headlines from your feeds. It updates itself on a schedule you control per source, survives internet outages by keeping the last good data and marking it stale, and never asks you to touch anything. It's built for a portrait display behind a semi-reflective mirror, in the dark industrial telemetry style we already approved.

## Screens
- `mockups/mirror-home.html` — the single mirror dashboard: clock + weather, day timeline, five event cards, transit departures, news lines, device footer. Copied from the approved reference design.
- `mockups/reference-screen.png` — screenshot of the approved reference design.

## Open product decisions (from spec §13)
1. **Display hardware:** what screen/resolution/diagonal, and does the server share the machine with the browser?
2. **Transit:** which stop(s), which routes/directions, walking time, and does the single primary line win or the fastest relevant departure?
3. **Calendar:** which calendar names, how to treat work/all-day events, and whether event descriptions are shown.
4. **News:** which feed URLs, and do headlines rotate or stay static?
5. **Language:** Hungarian or English labels (24-hour time either way).
6. **Privacy:** may recent calendar/news data be written to disk on the display device?
