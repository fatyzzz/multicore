# Product

<!-- impeccable:product-schema 1 -->

## Platform

windows

## Users

Windows users who want a simple daily VPN client but need routing capabilities that neither Xray nor Mihomo provides alone.

## Product Purpose

MultiCore starts and coordinates two local engines behind one native interface. Mihomo owns TUN, DNS, rules, process routing, and selectors. Xray provides the FinalMask-capable transport layer through local SOCKS bridges. Success means the user can import one subscription, connect once, understand which route is active, and diagnose failures without reading raw configuration.

## Positioning

One native Windows client combines Mihomo's policy plane with Xray's transport plane while keeping their ownership boundaries explicit and observable.

## Operating Context

- Daily connect/disconnect from a compact desktop window or tray.
- Subscription import and refresh using the provider's Mihomo YAML and Xray JSON responses.
- Route selection before or during a connection.
- Troubleshooting through owned-process health, redacted logs, and the Mihomo-to-Xray loopback mapping.

## Capabilities and Constraints

- Rust-native Slint UI; no mandatory WebView or Zashboard.
- Mihomo alone owns TUN, DNS, rules, process routing, and selector groups.
- Xray alone owns FinalMask-capable outbound transports exposed as named loopback SOCKS inbounds.
- Subscription user agents are `multicore-json-massive`, `multicore-mihomo`, and `multicore-xray`; a successful massive response prevents redundant fallback requests.
- Accepted payloads are Mihomo YAML and Xray JSON only.
- Xray JSON is stored byte-exact; runtime-only bootstrap overlays are limited to endpoint resolution and interface binding.
- The client must not fabricate traffic, latency, health, or subscription data.
- The current deliverable is an unsigned portable Windows x64 preview.

## Brand Commitments

The product name may change. The interface must remain direct, compact, dark, and free of a large decorative logo or provider-driven clutter. Flags and semantic symbols are useful route data, not brand decoration.

## Evidence on Hand

- Current implementation and deterministic preview fixtures in `apps/multicore-desktop` and `scripts`.
- User-supplied functional reference in `fl8-old-visual`.
- Real subscription metadata, selector catalog, runtime health, and redacted core logs are already available through the local daemon.
- No approved logo, signed release channel, or real-time traffic-rate telemetry exists; future UI must not invent them.

## Product Principles

1. One obvious daily action; advanced controls stay findable but subordinate.
2. Report applied runtime truth, never optimistic fiction.
3. Keep Mihomo policy and Xray transport responsibilities visible and separate.
4. Preserve the last working profile and provide actionable recovery when an update fails.
5. Native Windows behavior and keyboard accessibility are production requirements.
