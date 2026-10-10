---
name: Fusebox dashboard
description: A warm near-black operator panel for a local AI API proxy, read like a fusebox.
colors:
  bg: "#0b0b0a"
  surface: "#121210"
  surface-pop: "#141412"
  surface-inset: "#0e0e0c"
  row-hover: "#151513"
  row-fresh: "#1b1811"
  ctl-hover: "#1c1b18"
  icon-hover: "#1a1917"
  pal-active: "#1f1e1a"
  seg-on: "#2a2925"
  line: "#24231f"
  line-soft: "#1d1c19"
  line-strong: "#33312c"
  line-hover: "#4a4740"
  card-hover: "#3d3b35"
  rivet: "#2b2a26"
  fg: "#f2efe8"
  fg-2: "#b4afa4"
  fg-3: "#857f74"
  idle: "#55524b"
  bar: "#3a3833"
  meter: "#d9d4c8"
  brand: "#F3B52F"
  ok: "#4ade80"
  warn: "#fbbf24"
  err: "#fb7185"
  warn-line: "#4a3f22"
  err-line: "#4a2a30"
  err-field: "#7a3440"
  err-btn-line: "#3a2328"
  err-hover: "#1f1414"
typography:
  body:
    fontFamily: "IBM Plex Sans"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.45
  row:
    fontFamily: "IBM Plex Sans"
    fontSize: "13px"
    fontWeight: 400
  meta:
    fontFamily: "IBM Plex Sans"
    fontSize: "12-12.5px"
    fontWeight: 400
  figure:
    fontFamily: "IBM Plex Sans"
    fontSize: "20px (phone 17px)"
    fontWeight: 500
    lineHeight: 1.15
    letterSpacing: "-0.01em"
  drawer-title:
    fontFamily: "IBM Plex Sans"
    fontSize: "17px"
    fontWeight: 600
  page-title:
    fontFamily: "IBM Plex Sans Condensed"
    fontSize: "22px"
    fontWeight: 600
    letterSpacing: "0.08em"
    textTransform: uppercase
  section-title:
    fontFamily: "IBM Plex Sans Condensed"
    fontSize: "13px (phone 12px)"
    fontWeight: 600
    letterSpacing: "0.12em"
    textTransform: uppercase
  panel-label:
    fontFamily: "IBM Plex Sans Condensed"
    fontSize: "11px"
    fontWeight: 600
    letterSpacing: "0.12em"
    textTransform: uppercase
  table-header:
    fontFamily: "IBM Plex Sans Condensed"
    fontSize: "11px"
    fontWeight: 600
    letterSpacing: "0.10em"
    textTransform: uppercase
  tag-caps:
    fontFamily: "IBM Plex Sans Condensed"
    fontSize: "10.5px"
    fontWeight: 600
    letterSpacing: "0.08em"
  data:
    fontFamily: "DM Mono"
    fontSize: "11-14px"
    fontWeight: 400
rounded:
  segment: "1px"
  track: "2px"
  route-tag: "3px"
  kbd: "4px"
  tag: "5px"
  control: "6px"
  segmented: "7px"
  panel: "8px"
  popover: "10px"
  sheet: "18px"
spacing:
  page: "28px clamp(16px, 3vw, 40px) 56px (phone 16px 16px 24px)"
  section-gap: "32px (phone 24px)"
  panel: "18px 20px"
  row: "10px vertical, 16px column gap"
components:
  button:
    borderColor: "{colors.line-strong}"
    textColor: "{colors.fg}"
    rounded: "{rounded.control}"
    height: "32px (phone 36-44px)"
  button-primary:
    backgroundColor: "{colors.fg}"
    textColor: "{colors.bg}"
    hoverBackground: "#ffffff"
  input:
    backgroundColor: "{colors.bg}"
    borderColor: "{colors.line-strong}"
    typography: "{typography.data}"
    height: "34px"
  panel:
    backgroundColor: "{colors.surface}"
    borderColor: "{colors.line}"
    rounded: "{rounded.panel}"
  code-block:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.fg-2}"
    rounded: "{rounded.control}"
    padding: "14px 16px"
---

## Overview

Fusebox is read like the fusebox it is named after. The main line comes in at the top, every account is a circuit with its own breaker, and anything that has tripped is said plainly with the time it comes back. Surfaces are warm near-black, text is warm off-white, and hierarchy comes from luminance and from type: condensed capitals for labels, a plain sans for reading, a monospace for data.

Mode: Operate. Familiar controls, dense tables, tabular numbers, keyboard shortcuts.

## Colors

- `bg` #0b0b0a for the page, header, phone tab bar, inputs and code. `surface` #121210 for panels and the drawer; `surface-pop` for the command palette and faults menu; `surface-inset` for an expanded request.
- Text ramp: `fg` for primary text and values, `fg-2` for body copy in rows, `fg-3` (4.9:1 on `bg`) for labels, headers and metadata. Never dimmer than `fg-3` for text; `idle` is only for a disabled dot.
- State: `ok` ready and live, `warn` cooling, 75–94% used and routing detours, `err` errors, 95%+ used and failed requests. Dots, short words and meter segments only, never large fills.
- `brand` amber #F3B52F is the fuse. It appears in four places only: the active tab's underline, the current minute's load bar, link hover and the phone tab indicator (plus the selected option ring, the unsaved-section dot and the `NEXT` route tag). Never a fill, never body text.
- Providers are identified by their real logos (`ui/logos.svg`, LobeHub Icons, MIT) through `<use>`; one-colour marks take `fg`.

## Typography

Self-hosted from `ui/fonts/` (latin subset, SIL OFL 1.1), served at `/ui/fonts/*` with no external requests.

- IBM Plex Sans for everything people read: 14px/1.45 body, 13px rows and buttons, 12–12.5px metadata, 20px/500 figures.
- IBM Plex Sans Condensed, uppercase and tracked, for labels: 22px page titles (Accounts, Requests, Models, Config), 13px section titles (Subscriptions, Other circuits, Latest requests), 11px panel labels (Main line, Load · last 60 min) and table headers, 10.5px meter labels (5H, WK), `NEEDS RESTART` and `NEXT`. Labels are written in sentence case in the markup and set in capitals by CSS.
- DM Mono for data: the endpoint, keys, model ids, times, status codes, session ids, file paths, YAML and keyboard hints.
- `font-variant-numeric: tabular-nums` everywhere.

## Layout

Fluid and full width (no max-width container). Desktop: a 56px header with the wordmark (the mark below 900px), five tabs (Overview, Accounts, Requests, Models, Config), the search button (⌘K), the faults button, the privacy toggle and Live / Reconnecting. Phones (below 760px): a 52px header (the wordmark on Overview, otherwise the mark and the page title) and a 64px bottom tab bar with an amber 16×2px indicator. Both headers grow by the top safe area and the tab bar by the bottom one, so an installed iPhone app (black-translucent status bar) draws under neither.

Overview, top to bottom: Load · last 60 min across the full width, Subscriptions, Other circuits, Latest requests. Until this browser has seen Fusebox serve a request, the main line (endpoint, key, model count and how to set up a client) comes first; after that it lives in Config, Clients and the ⌘K palette. Tripped accounts are listed by the faults button in the top bar, not on the page. On phones the order is main line (first run only), subscriptions, load, other circuits, latest requests, and the faults button sits in the header on every page.

Column tiers: Requests adds First token at 1100px and In / Out / Cached at 1360px; Accounts adds Traffic at 1100px and # / Last used at 1380px; Overview's subscriptions add # and Requests at 1180px and latest requests add First token at 1240px; Models shows route order beside the ids from 1180px; Config's section nav becomes chips below 980px.

## Elevation & Depth

Flat. Panels are `surface` with a 1px `line` hairline. The only shadow is on floating layers (palette, faults menu): `0 24px 60px rgba(0,0,0,.6)`. Scrims are `rgba(0,0,0,.55)` behind the palette and `.6` behind the drawer.

## Shapes

1px meter segments and load bars, 2px meter track, 3px route tags, 4px kbd and copy buttons, 5px tags and row buttons, 6px buttons, inputs, small cards and notices, 7px segmented controls, 8px panels, 10px for the Subscriptions panel, palette and faults menu, 18px top corners on the phone bottom sheet. Hairlines are 1px. No gradients, glows or coloured side borders.

## Components

- **Segmented quota meter.** 20 segments of 5%, 9px tall (10px in the drawer, 20px on the account page), 2px gap, 180px at most. Lit segments are `meter` below 75% used, `warn` from 75% and `err` from 95%; the thresholds always measure used, even in Remaining mode. Off segments are `line`. To the right: the percentage ("<1%" and ">99%" at the ends), then a live reset countdown such as "↺ 2h 14m" or "↺ 3d 7h" in DM Mono for both 5H and WK. Use at most two units, omit zero suffixes, and show "↺ <1m" below one minute. Reserve 10ch for alignment; the full reset date, local time and timezone appear only in the hover tooltip.
- **Status.** An 8px dot and a word: Ready (`ok` dot, `fg-2` word), Cooling 1h 41m (`warn`), Error (`err`), Disabled (`idle`).
- **Rivets.** The Subscriptions, Accounts and Models panels carry four 6px `rivet` dots in their corners.
- **Breaker.** Each account row ends with refresh, an on/off switch (`role="switch"`, square knob, `meter` track when on) and remove.
- **Segmented control.** 2px padding inside a 1px `line-strong` border; the selected segment is `seg-on` with `fg` text.
- **Filter chips.** 13px with a DM Mono count; selected chips have a `line-hover` border.
- **Command palette.** ⌘K / Ctrl K or `/`: actions, accounts and models, ↑↓ to move, ↵ to run, Esc to close.
- **Faults.** Derived from account state and the request log: expired sign-ins, used-up limits, rate limits, account errors and three or more failed requests in an hour.
- **Toast.** Bottom centre, `fg` on `bg`, for 1.6s.
- **Fresh rows.** A new request row shows `row-fresh` for 1.5s, then fades (no transition with reduced motion).
- **Notifications.** Config, Notifications: "This device" (status line, Turn on / Send test / Turn off), the other subscribed devices with Remove, then a switch per event. The device controls act at once; the events save with the page.
- **App badge.** An installed app shows the fault count on its icon.
- **Privacy.** Emails read `••••••@••••••`, key ends `••••…••••`, the client key `fbx_••••••••`; copy buttons still copy the real value.

## Do's and Don'ts

- Do keep amber for the fuse only.
- Do show the time until an account comes back.
- Do keep one primary (off-white) button per view at most.
- Do give every control a visible 2px focus ring (`line-hover`) and 44px touch targets on phones.
- Do set text fields to 16px on touch screens (`pointer: coarse`); iOS zooms into smaller ones, and in an installed app the zoom can stick.
- Don't use colour for anything but state and provider logos.
- Don't use monospace for prose or labels, or the condensed face for reading text.
