---
name: MultiCore
description: Compact native Windows control center for a dual-core network client.
colors:
  canvas: "#0D0F12"
  surface: "#15181D"
  surface-raised: "#1C2026"
  stroke: "#2A3038"
  text-primary: "#F3F5F7"
  text-secondary: "#B8C0CA"
  text-muted: "#7F8996"
  accent: "#4C9DFF"
  success: "#3CCB7F"
  warning: "#E7AA45"
  danger: "#F06A6A"
typography:
  headline:
    fontFamily: "Segoe UI Variable Text, Segoe UI, sans-serif"
    fontSize: "18px"
    fontWeight: 600
  title:
    fontFamily: "Segoe UI Variable Text, Segoe UI, sans-serif"
    fontSize: "14px"
    fontWeight: 600
  body:
    fontFamily: "Segoe UI Variable Text, Segoe UI, sans-serif"
    fontSize: "13px"
    fontWeight: 400
  label:
    fontFamily: "Segoe UI Variable Text, Segoe UI, sans-serif"
    fontSize: "12px"
    fontWeight: 400
rounded:
  control: "8px"
  row: "14px"
  shelf: "16px"
  connection: "18px"
  circle: "999px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  pane: "20px"
components:
  connection-control:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.circle}"
    size: "72px"
  route-chip:
    backgroundColor: "{colors.surface-raised}"
    textColor: "{colors.text-secondary}"
    rounded: "{rounded.row}"
    height: "44px"
  route-row:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.row}"
    height: "46px"
  subscription-shelf:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text-primary}"
    rounded: "{rounded.shelf}"
    height: "54px"
---

# Design System: MultiCore

## Overview

**Creative North Star: "Signal Grid Control Center"**

MultiCore is a compact native Windows network utility. The interface feels operational and calm: one unmistakable connection control, honest subscription state, and routing controls that stay on the same page. A cursor-reactive signal mesh gives the connection control technical character without decorating the whole window.

The product is provider-aware but not provider-branded. A cached provider logo may identify the subscription inside the circular connection control, while a permanent power badge preserves the action affordance. The shell never becomes a marketing dashboard or a mobile layout stretched onto desktop.

**Key Characteristics:**

- One dominant connect/disconnect control.
- Dense one-page routing with stable, color-coded latency values.
- Flat tonal depth and quiet borders, with one bounded interactive signal effect.
- Sentence-case Russian copy and explicit unknown/error states.

## Colors

The palette is a cool near-black neutral system with one blue action accent and semantic health colors.

The signal mesh uses translucent derivatives of the same status palette: blue `#77B8FF`, green `#66E8A6`, and red `#FF8B8B`. They are interaction layers, never standalone accents.

**The One Accent Rule.** Blue identifies selection, focus, and the primary action. Green, amber, and red communicate measured state only; they never become competing CTAs.

## Typography

**Display and Body Font:** Segoe UI Variable Text with Segoe UI and system sans-serif fallbacks.

**Character:** Native, compact, and highly legible. Numerical latency values use stable right alignment so scanning never moves the column.

### Hierarchy

- **Headline** (600, 18px): connection state inside the primary strip.
- **Title** (600, 14px): section and subscription titles.
- **Body** (400, 13px): controls and explanatory copy.
- **Label** (400, 12px): metadata, latency, and quiet status text.

**The Sentence Case Rule.** Status labels and headings use sentence case; uppercase Russian status copy is prohibited.

## Layout

- Default window is approximately 840 × 720px; minimum is 700 × 620px.
- A 176px navigation rail owns only Home, Status, and Settings.
- Home starts with the 96px connection strip, then the 54px subscription shelf, optional announcement, group selector, and server list.
- Every route group and the selected group's servers stay on Home. A route action may refresh or focus Home but never navigates to a separate route page.
- Group chips are 44px high inside a 60px horizontal scroller so the native scrollbar never covers them.
- Server rows are 46px high. Their latency uses one fixed 76px right-aligned column plus a permanently reserved 40px selection gutter.
- The main pane has no decorative full-window guides. The signal mesh is clipped to the 96px connection strip and visually anchored to its circular action.

## Elevation & Depth

The system is flat by default. Depth comes from tonal surfaces, one-pixel borders, and state-tinted fills. Shadows, glass, and blur are absent. A single radial glow may reveal the connection mesh under the pointer; route controls may use a quieter local pointer wash.

**The One Boundary Rule.** A surface uses one boundary mechanism at a time. Nested borders and card-inside-card framing are avoided.

## Shapes

Controls use 8px corners, route rows and chips use 14px corners, the subscription shelf uses 16px, and the connection strip uses 18px. The clipped signal field uses 24px corners so its reveal cannot form sharp artifacts. The 72px connection control is circular. Nested radii follow the outer-radius-equals-inner-radius-plus-inset relationship.

## Components

### Connection Control

- One 72px circular button owns connect/disconnect/retry behavior.
- A provider logo is clipped inside the button when available.
- A small decorative power badge remains visible over provider logos; it is not a second action or touch target.
- Focus, pressed, disabled, success, danger, and neutral states remain visible. Pointer movement reveals a local 8px signal mesh; its opacity settles in 180ms and it never loops on a timer.

### Subscription Shelf

- Fixed at 54px with exactly two text lines.
- The first line is the provider title. The second is usage and expiry, or the refresh error in its place.
- Remote URLs and provider action links are never rendered directly.

### Route Chips and Rows

- Chips navigate groups locally without leaving Home.
- Rows preserve a fixed latency column and a fixed selection gutter in every state.
- Pending takes precedence over selected: an ellipsis is shown until the daemon confirms the route, then a checkmark appears.
- Latency tone is green below 80ms, amber from 80–149ms, and red at 150ms or above; timeouts and unavailable results are red, unknown is muted.

### Navigation

- Home, Status, and Settings are the only persistent destinations.
- External or tray route actions resolve to Home and may refresh the catalog; no blank route page exists.

## Do's and Don'ts

### Do:

- **Do** keep connection, subscription, every route group, and the selected server list together on Home.
- **Do** preserve one clear CTA and at least 44px interactive targets with visible keyboard focus.
- **Do** display only real metrics and explicit unknown states.
- **Do** keep provider identity subordinate to the product action.
- **Do** keep the cursor-reactive mesh clipped to its owning control and driven by real pointer or focus state.

### Don't:

- **Don't** add decorative logo/header stacks, duplicate Home headings, or repeated Auto explanations.
- **Don't** add card soup, fake charts, invented latency, raw YAML/JSON, or duplicated engine switches.
- **Don't** use emoji as interface chrome; route flags are allowed because they are data.
- **Don't** expose controls before a real backend action exists.
- **Don't** run decorative lines through the app shell or repeat the full mesh on every card.
