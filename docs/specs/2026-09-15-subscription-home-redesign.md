# Subscription-first home redesign

## Outcome

MultiCore becomes a production-shaped 480 × 720 native client rather than a minimal connection demo. The home screen exposes the subscription state, a real refresh action, current routing, and Mihomo selectors without exposing either core.

## Data contract

- Every successful subscription fetch captures the bounded `subscription-userinfo` response header.
- The persisted snapshot generation also stores the source URL and safe parsed metadata under the same owner-only ACL as the credential-bearing Mihomo/Xray configs.
- API responses never return the source URL. Status returns only downloaded bytes, optional total bytes, expiry epoch, last refresh epoch, and whether refresh is available.
- `POST /v1/subscriptions/refresh` reuses the stored source URL and keeps the last-good generation on any failure. It requests `multicore-json-massive` first and stops after a successful HTTP response, including a malformed bundle; the separate `multicore-mihomo` + `multicore-xray` pair is used only when massive is unavailable or unsupported.
- Legacy snapshots have no source record. Their refresh action opens URL entry once; the next successful import creates a refreshable generation.

## Interaction contract

- Window target: 480 × 720, responsive from 420 × 640 to 620 × 920.
- A compact title row contains `Главная` and diagnostics.
- A subscription surface shows source host/title, downloaded traffic, remaining days or expiry date, and `Обновить`.
- The connection surface remains the only connect/disconnect action and shows the current route.
- Mihomo groups and nodes remain below. Group navigation works in ready and connected states.
- Selecting a node while ready queues that opaque node identity. After connect succeeds, the desktop applies it immediately through the authenticated selector API; the UI marks it as queued instead of pretending the click failed.
- No fake support button, telemetry, quota, or provider title is invented when the server does not supply it.

## Failure behavior

- Refresh, import, connect, and selector mutations remain serialized.
- Refresh errors are safe and inline; URLs, response bodies, bearer tokens, and raw controller names remain redacted.
- A queued pre-connect selection is discarded if the catalog revision changes or refresh replaces the snapshot.
