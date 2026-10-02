---
name: CLIProxyAPI-Rust dashboard
description: Pure-black OLED operator panel for a local AI API proxy.
colors:
  bg: "#000000"
  raise: "#0a0a0a"
  raise-hover: "#111111"
  line: "#1a1a1a"
  line-strong: "#2a2a2a"
  line-hover: "#3a3a3f"
  bar-idle: "#3f3f46"
  switch-on: "#e4e4e7"
  lit: "#ffffff"
  fg: "#f4f4f5"
  fg-2: "#a1a1aa"
  fg-3: "#7c7c85"
  ok: "#4ade80"
  warn: "#fbbf24"
  err: "#fb7185"
  brand: "#e11d48"
  provider-claude: "#e8956b"
  provider-codex: "#d4d4d8"
  provider-gemini: "#7aa2f7"
  provider-compat: "#a78bfa"
  provider-vertex: "#4cc9b0"
  provider-antigravity: "#f472b6"
  provider-kimi: "#67d2f0"
  provider-xai: "#ffffff"
  provider-meta: "#5b8def"
  provider-devin: "#b6e35a"
typography:
  body:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  title:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "22px"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "-0.015em"
  section:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    lineHeight: 1.3
  label:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    lineHeight: 1.4
  figure:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "20px"
    fontWeight: 500
    lineHeight: 1.2
    letterSpacing: "-0.02em"
  figure-compact:
    fontFamily: "ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 500
    lineHeight: 1.2
  data:
    fontFamily: "ui-monospace, SF Mono, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  hair: "1.5px"
  bar: "2px"
  xs: "4px"
  tag: "5px"
  sm: "6px"
  seg-inner: "7px"
  md: "8px"
  seg: "9px"
  lg: "12px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "20px"
  xl: "32px"
  xxl: "48px"
components:
  button:
    backgroundColor: "{colors.bg}"
    textColor: "{colors.fg}"
    rounded: "{rounded.md}"
    height: "32px"
    padding: "0 12px"
  button-hover:
    backgroundColor: "{colors.raise-hover}"
  button-primary:
    backgroundColor: "{colors.fg}"
    textColor: "{colors.bg}"
    rounded: "{rounded.md}"
    height: "32px"
    padding: "0 14px"
  input:
    backgroundColor: "{colors.raise}"
    textColor: "{colors.fg}"
    rounded: "{rounded.md}"
    height: "34px"
    padding: "0 10px"
  code-block:
    backgroundColor: "{colors.raise}"
    textColor: "{colors.fg-2}"
    typography: "{typography.data}"
    rounded: "{rounded.lg}"
    padding: "14px 16px"
---

## Overview

An operator panel that behaves like an always-on display: the screen is black, and only information is lit. Luminance, not color, carries hierarchy (fg → fg-2 → fg-3). Color is reserved for state (ok / warn / err) and for identifying providers with a small dot. Nothing glows. There are no cards: sections are separated by space and single hairlines.

Mode: Operate. Familiar controls, dense tables, tabular numbers, system fonts.

## Colors

- `bg` #000 everywhere; OLED pixels stay off. `raise` (#0a0a0a) only for inputs, code and inline panels.
- Text ramp: `fg` for primary values and titles, `fg-2` for body and secondary values, `fg-3` (5.1:1) for labels and metadata. Never go dimmer than `fg-3` for text.
- State: `ok` ready/success, `warn` cooling down, `err` failures. Used for dots and short status words, never for large fills.
- `brand` rose is the mark only. It is not a text color (4.47:1 on black).
- Provider dots: Claude clay, Codex light gray, Gemini blue, OpenAI-compatible violet.
- Supporting neutrals: `line-hover` for hovered control borders, `bar-idle` for traffic bars and disabled dots, `switch-on` for an enabled switch track, and `lit` (#fff) only for the instant a new row lights up and for the hovered primary button.

## Typography

One system sans family for all UI; monospace only for real code and data (endpoints, model ids, keys, config, numbers in logs). All numbers use `font-variant-numeric: tabular-nums`. Section titles are sentence case at 15px/600 with no eyebrows or tracking.

## Layout

Single column, max width 1180px, 32px side padding (16px on mobile). Sticky 56px top bar with tabs. More space above a section title (32–40px) than below it (12px). Overview splits into a 3:2 two-column grid above 900px.

## Elevation & Depth

Flat. Depth comes only from `raise` surfaces and 1px `line` hairlines. No shadows on the page; the one floating element (copy feedback) uses a soft offset shadow.

## Shapes

8px radius for controls, 12px for code blocks and inline panels, full round for status dots (6px). Small radii exist only where the element is small: 2px/1.5px traffic bars, 4px focus ring and skeleton lines, 5px tags, 9px/7px segmented control and its buttons.

## Components

- Buttons: outline (line-strong border) by default; one white-filled primary per view at most; ghost buttons for row actions.
- Tables: 12px fg-3 headers, 40px rows, 1px line separators, row hover `#070707`.
- Status: dot + word ("Ready", "Cooling 4:12", "Disabled", "Error").
- Live rows: a new request row lights up at full white and settles to its resting luminance over 1.8s (the one authored motion; disabled for reduced motion).

## Do's and Don'ts

- Do keep the background pure #000.
- Do show time-to-recovery for cooling accounts.
- Don't use glows, gradients, glass, or colored side borders.
- Don't use monospace for labels or headings.
- Don't put more than one primary (white) button in a view.
