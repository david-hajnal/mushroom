---
name: Cathode Telemetry
colors:
  surface: '#121319'
  surface-dim: '#121319'
  surface-bright: '#383940'
  surface-container-lowest: '#0d0e14'
  surface-container-low: '#1a1b21'
  surface-container: '#1e1f26'
  surface-container-high: '#292a30'
  surface-container-highest: '#33343b'
  on-surface: '#e3e1ea'
  on-surface-variant: '#c8c7be'
  inverse-surface: '#e3e1ea'
  inverse-on-surface: '#2f3037'
  outline: '#929189'
  outline-variant: '#474741'
  surface-tint: '#c9c6c1'
  primary: '#ffffff'
  on-primary: '#31312d'
  primary-container: '#e5e2dc'
  on-primary-container: '#656460'
  inverse-primary: '#5f5e5a'
  secondary: '#ffba3d'
  on-secondary: '#432c00'
  secondary-container: '#d59401'
  on-secondary-container: '#4c3200'
  tertiary: '#ffffff'
  on-tertiary: '#68000a'
  tertiary-container: '#ffdad7'
  on-tertiary-container: '#bf282b'
  error: '#ffb4ab'
  on-error: '#690005'
  error-container: '#93000a'
  on-error-container: '#ffdad6'
  primary-fixed: '#e5e2dc'
  primary-fixed-dim: '#c9c6c1'
  on-primary-fixed: '#1c1c18'
  on-primary-fixed-variant: '#474743'
  secondary-fixed: '#ffdeae'
  secondary-fixed-dim: '#ffba3d'
  on-secondary-fixed: '#281900'
  on-secondary-fixed-variant: '#604100'
  tertiary-fixed: '#ffdad7'
  tertiary-fixed-dim: '#ffb3ad'
  on-tertiary-fixed: '#410004'
  on-tertiary-fixed-variant: '#930012'
  background: '#121319'
  on-background: '#e3e1ea'
  surface-variant: '#33343b'
typography:
  headline-xl:
    fontFamily: Chivo
    fontSize: 48px
    fontWeight: '900'
    lineHeight: 52px
    letterSpacing: -0.04em
  headline-xl-mobile:
    fontFamily: Chivo
    fontSize: 32px
    fontWeight: '900'
    lineHeight: 36px
    letterSpacing: -0.03em
  headline-lg:
    fontFamily: Chivo
    fontSize: 32px
    fontWeight: '800'
    lineHeight: 36px
    letterSpacing: -0.02em
  headline-lg-mobile:
    fontFamily: Chivo
    fontSize: 24px
    fontWeight: '800'
    lineHeight: 28px
    letterSpacing: -0.01em
  headline-md:
    fontFamily: Chivo
    fontSize: 20px
    fontWeight: '700'
    lineHeight: 24px
    letterSpacing: 0.02em
  body-lg:
    fontFamily: Space Mono
    fontSize: 16px
    fontWeight: '400'
    lineHeight: 24px
    letterSpacing: -0.01em
  body-md:
    fontFamily: Space Mono
    fontSize: 14px
    fontWeight: '400'
    lineHeight: 20px
    letterSpacing: 0em
  body-sm:
    fontFamily: Space Mono
    fontSize: 12px
    fontWeight: '400'
    lineHeight: 16px
    letterSpacing: 0.02em
  label-lg:
    fontFamily: Space Mono
    fontSize: 13px
    fontWeight: '700'
    lineHeight: 16px
    letterSpacing: 0.08em
  label-md:
    fontFamily: Space Mono
    fontSize: 11px
    fontWeight: '700'
    lineHeight: 14px
    letterSpacing: 0.12em
  label-sm:
    fontFamily: Space Mono
    fontSize: 9px
    fontWeight: '700'
    lineHeight: 12px
    letterSpacing: 0.16em
spacing:
  gutter: 1rem
  gutter-desktop: 1.5rem
  margin: 1rem
  margin-tablet: 2rem
  margin-desktop: 3rem
  space-xs: 0.25rem
  space-sm: 0.5rem
  space-md: 1rem
  space-lg: 1.5rem
  space-xl: 2.5rem
---

## Brand & Style

This design system channels an uncompromising, mechanical convergence of David Lynch’s industrial dread and Trent Reznor’s raw audio-visual engineering. It establishes a high-performance HUD telemetry language designed for low-light environments, technical diagnostics, and critical monitoring displays. The emotional signature is austere, vigilant, and uncompromisingly utilitarian—evoking the sensory friction of cold iron, vacuum-tube filaments, and high-voltage precision apparatuses.

The aesthetic fuses surgical minimalism with technical brutalism:
- **Atmospheric Tension:** Abyss-grade background fields pierced by pinpoint, high-voltage illumination.
- **Instrument Telemetry:** Surfaces mimic analogue measurement equipment, testing consoles, and diagnostic terminal glass.
- **Zero Ornamentation:** No ornamental softness, artificial skeuomorphic plastic, or decorative radius. Form is solely derived from structural enclosure, hard division boundaries, and terminal data density.

## Colors

The palette operates on high-contrast phosphor radiance piercing through tiered void states. 

- **Primary (`#f2efe9` - Cold Cathode):** Pure, unyielding tungsten-white emission. Used for active indicators, primary metrics, razor-edge headers, and highest-priority readouts.
- **Secondary (`#ffb834` - Phosphor Amber):** Vintage tube burn-in accent. Reserved for active warning alerts, state modifications, engagement toggles, and live telemetry tracking.
- **Tertiary (`#d13636` - Hazard Crimson):** High-criticality industrial interlock alert. Reserved strictly for system errors, threshold breeches, destructive triggers, and shutdown states.
- **Neutral (`#82828a` - Muted Steel):** Cold, passive mechanical alloy tone for structural outlines, inactive metrics, peripheral framing, and secondary metadata.

### Background and Enclosure Tiering
- **Floor Void (`#070707`):** Base canvas background, creating deep negative space and darkroom immersion.
- **Sub-chassis (`#0d0d0d`):** Primary panel containers, module backgrounds, and unlit CRT wells.
- **Machined Iron (`#1a1a1c`):** Surface layers, cell headers, and elevated terminal decks.
- **Brushed Plate (`#262629`):** Interacting card backgrounds, input beds, and hover selections.

## Typography

Typography establishes an absolute contrast between massive, aggressive industrial structural markers (`Chivo`) and rhythmic, clinical terminal readouts (`Space Mono`).

- **Chivo (Headlines):** High-density geometric gothic with razor terminal strokes. Rendered in all-caps or heavy weights for station labeling, deck identification, and primary warnings.
- **Space Mono (Body, Telemetry, Labels):** Rigorous, fixed-width monospaced gauge output. All status codes, telemetry arrays, metadata labels, and timestamps enforce character alignment across multi-row tables.
- **Case Discipline:** Micro-labels (`label-sm`, `label-md`) must always be formatted in uppercase with extended tracking (`0.08em` to `0.16em`) to reflect precision-etched instrument panels.

## Layout & Spacing

The layout model simulates an electronic console interface: a strict 12-column structural grid system built from abutting machine cells and technical borders rather than float-away cards.

### Layout Philosophy
- **Modular Bays:** Content is structured into enclosed compartments resembling flight/laboratory diagnostic racks.
- **Grid Discipline:** Elements strictly bind to column boundaries. Modules touch edge-to-edge separated by 1px mechanical hairline seams rather than open margins.
- **Screen Adaptation:**
  - **Desktop (1200px+):** 12-column layout with high data density. Sidebars act as fixed vertical control racks. 
  - **Tablet (768px - 1199px):** 6-column reflow; persistent hardware headers remain anchored.
  - **Mobile (< 768px):** Single-column stacked telemetry modules. Viewports utilize minimal margin padding (`1rem`) to maximize signal surface on constrained displays.

## Elevation & Depth

This design system rejects drop shadows, blurred ambient lighting, and floating z-index tiers. Depth is rendered via hardware enclosures, illumination, and hairline optical boundaries.

- **Hairline Division:** Containers are delimited by sharp 1px borders using `#262629` (idle) or `#82828a` (active/focused).
- **Cathode Backlight Glow:** Crucial active readouts or hazard thresholds cast a tight, sharp 1px to 2px inner or perimeter glow using `#f2efe9`, `#ffb834`, or `#d13636` with zero ambient blur diffusion.
- **Recessed Wells:** Input fields and data streams utilize inset shading through darker surface tiers (`#070707` sunken into `#0d0d0d`), providing an impression of recessed gauge housings.
- **Scanline & Vignette Textures:** Complex dashboards utilize an atmospheric CSS grid overlay and subtle peripheral vignette to simulate low-light CRT phosphor shielding.

## Shapes

The geometric rule across every component, container, button, and indicator is absolute: **0px border radius.**

- **Corner Orthogonality:** All intersections terminate at 90-degree sharp right angles. Curvature is strictly prohibited.
- **Chamfer Overlays (Optional Functional Accents):** For technical accents, 45-degree angled corner cuts (4px or 8px via CSS clip-path) may be applied exclusively to corner callouts, batch action triggers, or module boundary tags.
- **Structural Integrity:** Elements reinforce structural solidity; components appear milled from structural steel or cut from mineral glass.

## Components

### Buttons & Switches
- **Primary Action:** Solid `#f2efe9` fill with `#070707` Chivo bold uppercase text. Hover state switches instantly to `#ffb834` without transitional easing. Active state depresses 1px down-right.
- **Secondary / Technical Action:** Transparent background with a 1px `#82828a` border and `#f2efe9` Space Mono text. On hover, background shifts to `#1a1a1c` and border illuminates to `#f2efe9`.
- **Hazard Action:** Hairline `#d13636` border, text `#d13636`. Hover fills the button with `#d13636` with `#070707` text.

### Telemetry Cards & Panels
- **Container Structure:** Background `#0d0d0d`, surrounded by a 1px solid `#1a1a1c` boundary.
- **Header Bay:** Top panel strip in `#1a1a1c` with uppercase `label-md` tracking, accompanied by technical index numbers (e.g., `SEC_04 // ADDR: 0x82`).
- **Telemetry Readouts:** Metric readouts set in `headline-lg` (`Chivo`) alongside a monospaced delta tag (`Space Mono`).

### Input Fields & Controls
- **Text Inputs:** Recessed box with `#070707` fill and 1px `#262629` frame. Text is monospaced `#f2efe9`. When focused, the frame transitions to a crisp 1px `#f2efe9` outline with no outer shadow bloom.
- **Checkboxes & Radios:** Sharp square boxes (0px radius) with `#1a1a1c` fill and `#82828a` borders. Checked state displays a solid `#ffb834` square core with an inner 2px offset.

### Status Indicators & Badges (Chips)
- **Status Badges:** Compact, unrounded blocks featuring an inline bracket notation: `[ ONLINE ]`, `[ STANDBY ]`, `[ CRITICAL ]`.
- **Status Signal:** 6px square solid diode indicator. Blinking pulse animation for warning (`#ffb834`) and fault (`#d13636`) states.

### Data Lists & Grids
- **Tables:** Monospaced tabular data with alternating zebra strips of `#0d0d0d` and `#121214`.
- **Separators:** 1px horizontal rule using `#1a1a1c`. Hovering over a row highlights the entire horizontal slice with a `#262629` fill and a 2px left border strip in `#ffb834`.