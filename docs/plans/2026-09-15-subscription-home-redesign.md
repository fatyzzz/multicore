# Subscription-first home implementation plan

1. Add RED tests for bounded subscription header parsing, secure generation persistence, safe DTO serialization, refresh atomicity, and legacy behavior.
2. Extend the snapshot/fetch pipeline with a redacted source record and safe usage metadata; add authenticated refresh endpoint.
3. Extend the desktop daemon client/view model with refresh state, safe formatting, and queued pre-connect selection applied after connect.
4. Recompose the Slint home at 480 × 720 with a subscription surface, explicit refresh, current route, and bottom selectors.
5. Verify group clicks, queued node selection, real header formatting, six deterministic captures, full tests/Clippy, isolated packaging, and normal/crash cleanup.
