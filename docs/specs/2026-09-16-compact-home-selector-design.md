# Compact Home and Selector Semantics

## Scope

- Replace the tall connection hero with one horizontal control strip: power, state/server summary, contextual action.
- Keep subscription metadata in one compact row.
- Give all remaining Home height to selector groups and the selected group's node list.
- Preserve every group selection independently.
- Only the daemon-designated primary group may update the connection summary.

## Data contract

The daemon's `GroupDto.selected` flag designates the group used for the connection summary. The desktop stores this separately from `selected_catalog_group`, which remains presentation-only navigation. Node changes in Games, Russian sites, or any other secondary group never mutate `UiState.node`.

## Failure modes

- If no group is designated primary, the first bounded group is the summary fallback.
- A stale selector revision continues to invalidate the catalog and request reconciliation.
- Compact layout must remain usable at the existing 700x660 minimum size.
