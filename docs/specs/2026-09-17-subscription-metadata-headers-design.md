# Subscription metadata headers design

## Goal

MultiCore will consume the display-oriented subscription response headers used by
INCY, FlClashX and Happ/Rabbit-Hole style panels. The UI will show the provider's
subscription name and bounded announcement instead of inventing labels such as
`gate8` or `work`. A successful `multicore-json-massive` response remains terminal
and must not cause Mihomo or Xray fallback requests.

Every subscription request also sends a stable Windows HWID compatibility header
set. Provider service logos may be displayed inside the primary connection control
after bounded download, validation and local caching.

## Accepted metadata

Header names are case-insensitive. Values are read from HTTP response headers only;
inline `#header:` comments inside YAML or JSON are outside this change.

| Canonical field | Accepted response headers | Rules |
| --- | --- | --- |
| Subscription title | `profile-title`, then `flclashx-servicename`, then sanitized `Content-Disposition` filename | Plain UTF-8 or `base64:` for the first two. Standard and URL-safe Base64, missing padding accepted. Final fallback is the source host. |
| Service logo | `flclashx-servicelogo` | HTTPS URL only. Downloaded separately with redirects disabled, strict type/size/dimension limits and stored in the private snapshot generation. Logo failure never rejects the subscription. |
| Usage and expiry | any header whose name is exactly `subscription-userinfo` or ends in `-subscription-userinfo` | Parse `upload`, `download`, `total`, `expire` as bounded unsigned integers. Used traffic is saturating `upload + download`. Zero total/expiry means unknown/unlimited. |
| Refresh interval | `profile-update-interval` | Hours, clamped to 15 minutes through 30 days after checked conversion. It is metadata only; it does not silently enable scheduled refresh. |
| Provider website | `profile-web-page-url` | `http` or `https`, no credentials, bounded length. |
| Support | `support-url` | `http`, `https`, or `tg`, no credentials, bounded length. Opened only after an explicit user action. |
| Announcement text | `announce`, then `sub-info-text`, then `banner-text` | Plain UTF-8 or `base64:`. Strip control/bidi characters, collapse whitespace, cap at 512 Unicode scalars. `0` disables it. No markup. |
| Announcement target | `announce-url`, then `sub-info-button-link`, then `banner-button-url` | Same URL validation as support. |
| Announcement action label | `sub-info-button-text`, then `banner-button-text` | Plain text, sanitized and capped at 32 Unicode scalars. |
| Announcement tone | `sub-info-color` | Only `blue`, `green`, and `red` map to semantic info/success/danger tones. Arbitrary provider colors are ignored. |

`flclashx-background`, `flclashx-widgets`, `flclashx-settings`, routing headers and
other remote behavior/style overrides are deliberately ignored. MultiCore accepts
provider content, not remote control over its application or network settings.

## HWID request headers

Every `multicore-json-massive`, `multicore-mihomo` and `multicore-xray` request sends:

- `x-hwid`: an uppercase UUID-shaped identifier derived once from Windows
  `MachineGuid`, hostname, OS, architecture and user using the documented INCY
  double-SHA-256 `incy_hwid_` scheme, then persisted in the owner-only application
  data directory for stability;
- `x-device-os: windows`;
- `x-ver-os`: bounded Windows version string;
- `x-device-model`: bounded model string with `Windows PC` fallback;
- the existing User-Agent and Accept values.

The raw MachineGuid, hostname and user name are never sent, persisted in the
snapshot, returned by the daemon API, or logged. If hardware inputs cannot be read,
the client creates and persists a cryptographically random installation identifier
in the same uppercase UUID format. A corrupt identifier file is replaced atomically.

## Fetch and precedence

The `HttpResponse` transport carries one bounded `SubscriptionMetadataHeaders`
value alongside the body.

1. A successful HTTP response to `multicore-json-massive` is parsed once. Its body
   and metadata are authoritative; no other User-Agent is requested.
2. If massive is unavailable or returns a non-success status, the existing
   `multicore-mihomo` and `multicore-xray` requests run in parallel.
3. In fallback mode, metadata is merged field-by-field: the first valid Mihomo
   value wins; a missing or invalid value falls back to Xray. One malformed header
   must not suppress a valid counterpart.
4. A malformed optional metadata field never rejects an otherwise valid config.
5. All response bodies retain their existing size limits. Metadata has a separate
   aggregate raw byte budget and per-field limits before Base64 decoding.

## Persistence and API

`SubscriptionInfo` persists only sanitized presentation values plus the existing
source host and timestamps. The credential-bearing source URL remains in the private
`SubscriptionRecord` and never enters a desktop DTO or log.

The daemon subscription DTO exposes:

- `display_name`
- uploaded, downloaded and total bytes
- expiry and last-update timestamps
- optional refresh interval
- bounded announcement text, label and semantic tone
- booleans indicating support/home/announcement links

URLs stay inside the daemon. A separate authenticated explicit-action endpoint may
resolve/open them later; this change must not leak them through ordinary status or
diagnostic serialization.

## UI behavior

- The subscription shelf title is the sanitized `display_name`; the host is only a
  fallback.
- The compact metadata line shows used/total traffic and expiry without fabricating
  missing values.
- A bounded announcement strip appears immediately below the shelf only when text is
  present. It uses a semantic icon and tone, not arbitrary provider styling.
- Support and website actions appear only when the corresponding validated target is
  available.
- A validated cached service logo replaces the static glyph inside the primary
  connection control. Missing, invalid or failed logos use the built-in MultiCore
  mark without changing layout.
- Refresh replaces metadata atomically together with the config snapshot. A failed
  refresh preserves the entire last-good config and metadata.

## Failure modes

- Invalid Base64 or invalid UTF-8: ignore that candidate and continue fallback order.
- Credential-like or control-heavy title: ignore it and use the next name source.
- Oversized announcement: truncate only after safe decoding and sanitization.
- Unsafe URL scheme or embedded credentials: omit the action.
- Logo download, decode or validation failure: keep the built-in mark and continue
  importing the otherwise valid subscription.
- Conflicting Mihomo/Xray metadata: first valid Mihomo field wins deterministically.
- Older persisted snapshots: new optional fields deserialize with defaults and retain
  the current source-host fallback.
