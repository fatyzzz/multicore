# MultiCore — subscription identity and automatic latency

- Subscription requests now include the stable mandatory device headers expected by the backend.
- Provider title, traffic, expiry, announcements, and a safely cached provider logo are shown without exposing the subscription URL.
- The provider logo lives inside the single connection control; a power badge keeps the action recognizable.
- Route latency is measured automatically for the selected group after catalog load, group changes, and every 60 seconds while the window is visible.
- Latency values keep a stable column and semantic green/amber/red thresholds; results from other groups are retained.
- Home is denser and keeps connection, subscription, groups, and servers together. The old manual latency action and blank route page are gone.

The release does not add multiple stored subscriptions or direct provider-link actions.
