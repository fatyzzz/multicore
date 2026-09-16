# Route latency design

## Goal

Expose a small, honest latency signal for Mihomo routes without making the home
screen wider or leaking raw proxy names through the desktop API.

## Behavior

- The routes section has one compact `Проверить` action.
- Each visible node shows `N ms`, `таймаут`, or `ошибка` in the existing trailing
  metadata area.
- A successful subscription refresh schedules a probe. If the runtime is
  connected it runs immediately; otherwise it runs once after the next
  successful connection.
- Only one probe may be in flight. Results from an older catalog revision are
  ignored.

## API and trust boundary

`POST /v1/latencies` uses the existing bearer authentication and loopback-only
daemon boundary. The request may identify a catalog revision and bounded opaque
group/node IDs. Omitting targets probes the bounded current catalog.

The daemon resolves opaque IDs to raw Mihomo names internally and calls the
loopback Mihomo controller delay endpoint with bounded request time and result
count. The response contains only:

```json
{
  "entries": [
    {
      "group_id": "opaque",
      "node_id": "opaque",
      "latency_ms": 42,
      "status": "ok"
    }
  ]
}
```

`status` is one of `ok`, `timeout`, or `error`; `latency_ms` is null unless the
probe succeeded. Subscription URLs, controller secrets, and raw proxy names are
never returned.

## Failure states

- Disconnected runtime: keep a pending automatic probe and show no fabricated
  number.
- Timeout/controller failure: preserve the catalog and render a compact status.
- Refresh or catalog revision change: discard stale results.
- Repeated clicks during an active probe: coalesce into the current request.
