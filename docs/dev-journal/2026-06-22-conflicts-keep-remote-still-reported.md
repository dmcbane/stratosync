# `conflicts keep-remote` — conflict still reported after resolution

**Date:** 2026-06-22
**Version:** fixed in v0.13.0-beta.16

## Symptom

Running `stratosync conflicts keep-remote <path>` on a conflict entry that
had no local copy left the conflict visible in `stratosync conflicts list`.
The command printed "Resolved: kept remote version of …" but re-running
`list` showed the same conflict again.

## Root cause (three interlocking bugs)

### 1. `list` double-reported every conflict

`list` ran two separate queries:
- `WHERE status='conflict'`
- `WHERE name LIKE '%.conflict.%'`

A conflict sibling satisfies *both* conditions, so it was printed twice
(once as "CONFLICT", once as "FILE"). This made it look like two separate
conflicts when only one existed, and made any resolution look ineffective.

### 2. `resolve_path` couldn't find conflict siblings by their FUSE path

When the daemon creates a conflict it stores the sibling with:
- `parent_inode` = canonical's parent (so it appears in FUSE at the
  canonical's directory)
- `remote_path` = `.stratosync-conflicts/…` (namespaced on the backend)

`resolve_path` looked up paths exclusively via `get_by_remote_path()`.
The user sees `docs/file.conflict.…txt` in FUSE and passes that path; the
DB lookup searches for that string as a `remote_path` but finds nothing
because the stored value is `.stratosync-conflicts/docs/file.conflict.…txt`.
Result: "file not found in database" — the command errors out and the
conflict stays.

### 3. Role inversion (data-loss risk) if the sibling was ever resolved

`find_conflict_sibling()` returns "the other one" relative to what you pass:
given the canonical it finds the sibling, and given the sibling it finds
the canonical. But the old code always stored the result in
`ctx.conflict_sibling`, meaning if `resolve_path` ever managed to find the
*sibling* entry, `finalize_resolution` would:
- Delete `ctx.conflict_sibling` = **the canonical** (data loss)
- Call `set_cached` on `ctx.entry` = the sibling → sibling survives as
  `status='cached'`, still matches `name LIKE '%.conflict.%'` → still
  reported forever

This path was hard to hit in current code (bug #2 blocked it) but would
fire on any legacy DB row whose conflict `remote_path` predated the
`.stratosync-conflicts/` prefix namespace.

## Fix

1. **`list`** — replaced the two-query approach with a single combined
   query (`status='conflict' OR name LIKE '%.conflict.%'`) in
   `collect_conflict_entries`, with inode-level deduplication. Also added
   "resolve via: /mount/path/canonical" output so the user knows what path
   to pass to `keep-remote`/`keep-local`.

2. **`resolve_path`** — added a name-based fallback: when the primary
   `remote_path` lookup fails and the filename contains `.conflict.`, search
   by `name` within the mount. The conflict name format includes a timestamp
   and hash making it practically unique.

3. **Role normalization** — new `normalize_conflict_roles()` always produces
   `(canonical_entry, Option<sibling_entry>)` regardless of which end of the
   pair the user passed. Called unconditionally in `resolve_path` before
   building `ResolveContext`. `finalize_resolution` was also refactored to
   take individual parameters (`db, backend, entry, sibling, cache_path`)
   instead of `&ResolveContext`, enabling unit tests with `MockBackend`.

## Follow-up

The `.stratosync-conflicts/` namespace was added in v0.12.0. DBs created
before that could have conflict rows whose `remote_path` lacks the prefix,
making them findable by FUSE path and thus triggering the role-inversion
data-loss path (bug #3). No migration was written to backfill the prefix
(the remote files would need to be moved too). If a user reports canonicals
vanishing after conflict resolution on an old DB, that is the likely cause.
