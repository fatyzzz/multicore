# MultiCore subscription HTTP contract

This document is the backend-facing contract implemented by the current MultiCore client. Header names are case-insensitive. Subscription bodies are UTF-8 and are never share links, Base64 subscriptions, Clash JSON, or sing-box documents.

## Fetch sequence

For one subscription URL, the client first sends one request with `User-Agent: multicore-json-massive`.

- Any successful HTTP status is terminal: the client parses that body and does not make Mihomo/Xray fallback requests.
- Network failure or a non-success HTTP status triggers two parallel requests with `multicore-mihomo` and `multicore-xray`.
- Both fallback responses must succeed and validate before the client publishes anything.
- A failed refresh preserves the complete previous last-good snapshot.

## Request headers

Every config request includes these identity headers:

| Header | Value |
| --- | --- |
| `x-hwid` | Stable uppercase UUID-shaped installation identifier. Derived through the INCY-compatible scheme when Windows inputs are available; otherwise random and persisted. |
| `x-device-os` | `windows` |
| `x-ver-os` | Bounded Windows version string. |
| `x-device-model` | Bounded device model, with `Windows PC` fallback. |

The client never sends raw MachineGuid, hostname, or user name.

### Combined request

```http
User-Agent: multicore-json-massive
Accept: application/vnd.multicore.bundle+json, application/json
```

Successful body: a JSON array of exactly two elements:

```json
[
  "<complete Mihomo YAML as a JSON string>",
  { "<complete Xray configuration>": "as one JSON object" }
]
```

The first decoded element must be the full Mihomo YAML string. The second element must be one JSON object, not a quoted JSON string or an array.

### Mihomo request

```http
User-Agent: multicore-mihomo
Accept: application/json, application/yaml, text/yaml, text/plain
```

Successful body: one complete Mihomo YAML mapping ready to run. Mihomo owns TUN, DNS, process routing, rules, and selectors.

### Xray request

```http
User-Agent: multicore-xray
Accept: application/json, application/yaml, text/yaml, text/plain
```

Successful body: one complete Xray JSON object. It contains the provider-defined FinalMask-capable outbounds and matching loopback SOCKS inbounds used by Mihomo.

### Service-logo request

When a valid logo URL is returned, the client performs a separate request:

```http
User-Agent: multicore-service-logo
Accept: image/png, image/jpeg, image/webp, image/svg+xml
```

Identity headers are deliberately omitted from logo requests. Redirects are disabled. Only HTTPS logo URLs resolving exclusively to global addresses are accepted; the result is bounded, decoded, normalized, and cached as PNG.

## Accepted response metadata

Metadata may be returned on the successful combined response. In fallback mode, fields are merged independently: the first valid Mihomo value wins, and only a missing/invalid field falls back to Xray.

| Purpose | Accepted response headers | Format and behavior |
| --- | --- | --- |
| Display title | `profile-title`, then `flclashx-servicename`, then `Content-Disposition: ...; filename=...` | Plain UTF-8 or `base64:` for the first two. Standard/URL-safe Base64 with optional padding. Sanitized, maximum 128 characters; credential-like values are rejected. Source host is the final fallback. |
| Usage and expiry | `subscription-userinfo` or any name ending in `-subscription-userinfo` | Semicolon fields `upload=<u64>; download=<u64>; total=<u64>; expire=<u64>`. Byte counters use bytes; `expire` is Unix seconds. `total=0` or `expire=0` means unspecified/unlimited. |
| Refresh interval | `profile-update-interval` | Integer hours. Converted to seconds and clamped from 15 minutes through 30 days. Metadata only; it does not enable background refresh by itself. |
| Provider page | `profile-web-page-url` | `http` or `https`, maximum 2048 characters, host required, embedded credentials rejected. |
| Support | `support-url` | `http`, `https`, or `tg`; same bounds and credential rejection. |
| Service logo | `flclashx-servicelogo` | HTTPS URL only; host required; no embedded credentials. Logo failure does not fail the subscription. |
| Announcement text | `announce`, then `sub-info-text`, then `banner-text` | Plain UTF-8 or `base64:`, sanitized and capped at 512 characters. Value `0` suppresses the announcement. Markup is not interpreted. |
| Announcement target | `announce-url`, then `sub-info-button-link`, then `banner-button-url` | `http`, `https`, or `tg`; bounded, with no embedded credentials. |
| Announcement label | `sub-info-button-text`, then `banner-button-text` | Sanitized plain text, maximum 32 characters. |
| Announcement tone | `sub-info-color` | Only `blue`, `green`, or `red`, mapped to semantic info/success/danger styling. |

Unknown metadata headers are ignored. Remote styling/behavior headers such as `flclashx-background`, widgets, settings, or routing overrides are intentionally not supported.

## Metadata limits and safety

- Maximum accepted raw metadata total: 16 KiB.
- Maximum accepted raw value per metadata field: 4 KiB.
- Header values containing CR or LF are discarded.
- URLs and subscription credentials never enter normal status responses, diagnostics, or logs.
- Control and bidirectional-control characters are removed from provider text.
- Malformed optional metadata never rejects an otherwise valid configuration.

## Backend consistency requirements

All three representations must come from one immutable backend revision.

Recommended headers on every successful config response:

```http
X-Multicore-Revision: <same immutable revision across all representations>
ETag: "<representation-specific etag>"
Cache-Control: private, no-cache
```

The current client validates body/config consistency and atomic local publication but does not yet use `X-Multicore-Revision` or `ETag` for conditional requests. Backends should still emit them so future clients can detect cross-representation revision mismatches.

## Minimal complete example

```http
HTTP/1.1 200 OK
Content-Type: application/vnd.multicore.bundle+json; charset=utf-8
Profile-Title: base64:TXVsdGlDb3JlIFByZW1pdW0=
Subscription-Userinfo: upload=1024; download=2048; total=107374182400; expire=1798761600
Profile-Update-Interval: 6
FlClashX-ServiceLogo: https://cdn.example.com/logo.png
Announce: Плановое обслуживание 20 сентября
Sub-Info-Color: blue
X-Multicore-Revision: revision-42
Cache-Control: private, no-cache

["mixed-port: 7890\nproxy-groups: []\n",{"inbounds":[],"outbounds":[],"routing":{"rules":[]}}]
```

## Multi-subscription note

Multi-subscription support does not change this HTTP contract. Every saved profile has its own URL and independently runs the same fetch sequence. MultiCore never asks one endpoint to return several subscriptions and never merges configurations from different profile URLs.
