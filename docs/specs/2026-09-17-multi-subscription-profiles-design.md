# Multi-subscription profiles design

## Goal

Allow several independent MultiCore subscription URLs without merging unrelated Mihomo/Xray configurations. One profile is active at a time; each profile owns its last-good configs, provider metadata, logo, selector choices, and refresh lifecycle.

## Current-state migration

The current store contains zero or one subscription inside a single persistent snapshot root. On first startup with no profile index:

1. Load the latest valid legacy snapshot exactly as today.
2. If it has a subscription, generate a random stable profile ID and copy the complete last-good generation into that profile's store.
3. Verify the copied profile can be loaded.
4. Atomically publish the new profile index with that ID active.
5. Keep legacy generations untouched as rollback material. Once the index exists, normal startup uses profile stores only.

Migration never requests the subscription again and never exposes the saved URL.

## Storage model

```text
%LOCALAPPDATA%\MultiCore\
  profiles\
    index.json
    <profile-id>\
      generations\...
      selections.json
  preferences.json
  device-identity
  updates\...
```

`profiles/index.json` is versioned and contains only profile IDs, ordering, creation timestamps, and the active profile ID. Credential-bearing source URLs remain only inside each profile's private `subscription.json`. Display metadata is derived from validated profile snapshots instead of duplicated into the index.

The daemon profile index is the authority for the active profile. A desktop preference
may cache the last observed active ID only as a restore hint; it cannot override a newer
valid index value and is cleared when that profile no longer exists.

Profile IDs are random UUIDs generated locally. Provider titles and URLs never become directory names.

The initial product limit is 16 profiles. This bounds startup scanning, UI size, and refresh fan-out.

## Active-profile invariant

Exactly zero or one profile is active. Only the active profile may publish runtime configs and control Mihomo/Xray. Catalog, latency, mapping, connect, and tray route operations always address the active profile.

- Activation is allowed only while disconnected and with no mutation in flight.
- Attempting to activate or delete a profile while connected returns `409 Conflict` with a safe reason.
- Activating a profile loads its last-good snapshot, clears transient catalog/latency state, and then exposes its selectors.
- Switching profiles never combines or rewrites provider configs.

## Import, refresh, delete, and duplicates

- Import validates and fetches into a new isolated profile store. The index changes only after the new profile is completely durable and loadable.
- Successful import activates the new profile.
- Failed import leaves the index and active runtime unchanged.
- An exact already-saved source URL refreshes and activates the existing profile instead of creating a duplicate. URL comparison is byte-for-byte after trimming; query parameters are not reordered or decoded.
- Refresh is per profile. Refreshing an inactive profile updates only its store and presentation metadata.
- Deleting an inactive profile removes it after the index commit.
- Deleting the active disconnected profile atomically chooses the next ordered profile, or produces the empty state when none remain.
- Failed refresh preserves that profile's full last-good configuration, metadata, logo, and selections.

## Selector persistence

Each profile stores confirmed group/node choices separately. Choices use bounded opaque group and node IDs plus the catalog revision that confirmed them.

- A matching choice is restored only if both IDs exist in the newly loaded catalog.
- Missing or renamed groups/nodes are ignored, never guessed by list position.
- Ready-state queued choices are persisted after local selection so restart does not discard user intent.
- Connected choices are persisted only after Mihomo confirms the controller's live `now` value.
- A refresh keeps still-valid choices and drops invalid ones.

## Daemon API

Existing endpoints remain active-profile compatibility aliases. New authenticated endpoints:

- `GET /v1/subscriptions` — safe ordered summaries plus `active_profile_id`; no source URLs.
- `POST /v1/subscriptions/import` with `{ "url": "..." }` — create/refresh and activate.
- `PUT /v1/subscriptions/{profile_id}/activate` — disconnected activation.
- `POST /v1/subscriptions/{profile_id}/refresh` — targeted refresh.
- `DELETE /v1/subscriptions/{profile_id}` — disconnected deletion.
- `POST /v1/subscriptions/refresh` — refresh the active profile for compatibility.

All IDs are opaque bounded ASCII values. Unknown or stale IDs return `404`; connected mutation conflicts return `409`; secrets never appear in response bodies or logs.

## Desktop UX

The Home subscription shelf remains compact. Activating the shelf opens one anchored profile menu containing:

- ordered subscriptions with logo, title, usage/expiry summary, and active check;
- `Добавить подписку`;
- refresh for each profile;
- a guarded delete action.

The active profile continues to own the existing connection pill and inline routes. Multi-sub does not introduce a permanent sidebar page or merge server lists across providers. The URL editor stays in Settings and imports a new profile instead of replacing the active profile.

Tray profile switching is excluded from the first multi-sub release; route controls in tray continue to address only the active profile.

## Persistence and update behavior

Profile stores and `preferences.json` live outside the installation directory. An application update must preserve every profile, source URL, active ID, and selector choice. Schema migration is monotonic, atomic, and rollback-safe; code never deletes the previous representation before the replacement is verified.

## Error handling

- One corrupt profile does not prevent other valid profiles from loading; it is marked unavailable in diagnostics.
- Corrupt index: recover only from validated child profile stores, never from directory names or untrusted provider text.
- Profile refresh failure: retain last-good and show a per-profile error.
- Active profile becomes unreadable: start disconnected and require explicit selection of another valid profile.
- Delete cleanup failure after an index commit is logged as bounded maintenance debt; it does not resurrect a deleted profile.

## Testing

- Legacy one-profile migration without network access.
- Atomic import, refresh, activation, deletion, and rollback tests.
- Duplicate exact-URL behavior without logging the URL.
- Independent last-good generations and logos for two profiles.
- Per-profile selector restoration and invalid-selection dropping.
- API authentication, `404`, and connected `409` contracts.
- Desktop menu empty/one/many/loading/error captures.
- Updater preservation test across multiple profiles.

## Failure-mode review

- **Critical:** merging configs would collide on group names, ports, and Xray tags. Profiles remain isolated and only one publishes runtime state.
- **Critical:** partial import could replace a working profile. The index changes only after a complete durable profile exists.
- **Critical:** switching while connected could desynchronize cores and UI. Activation is rejected until disconnected.
- **Minor:** provider-renamed nodes cannot be safely matched. Missing opaque IDs are dropped instead of guessed.

## Non-goals

- Combining nodes from several subscriptions into one Mihomo catalog.
- Choosing Mihomo from one subscription and Xray from another.
- Concurrently running multiple active profiles or TUN devices.
- Cloud synchronization of profiles.
