# 2026-10-04 — ETag spelling mismatch made every upload a conflict

## Symptom
`upload conflict — invoking resolver` on nearly every upload, with
`local=` and `remote=` values of visibly different shapes:

- gdrive: `local=17EvqIs0KLB0…` (Drive item ID) or a 32-hex md5,
  `remote=20967fb0…` (40-hex sha1)
- onedrv: `local=4C6B1755…` (40-hex uppercase), `remote=4139BC84D0721EB5#4139BC84D0721EB5!sab60…` (item ID)

Usually followed by "identical bytes, refreshing ETag" (an extra full
download). For an edited binary, the resolver fell through to
keep-both, which made spurious `.conflict` copies (likely the Aug 2026
Pictures JPG conflicts).

## Root cause
Four producers wrote `file_index.etag`, and each chose a different value:

| producer | gdrive | OneDrive |
|---|---|---|
| delta poller (`delta.rs`) | `md5Checksum` | Graph `sha1Hash` (UPPER hex) / `quickXorHash` (base64) |
| `stat` / `list_recursive` (lsjson `--hash`) | sha1 | item ID (lsjson has only `quickxor`, which wasn't selected) |
| `list` (lsjson, no `--hash`) | item ID | item ID |
| post-upload / resolver refresh | = `stat` | = `stat` |

`RcloneBackend::upload` compared the stored value to `stat().etag` with
`!=`.

## Fix
- `check_if_match` compares the stored value against every lsjson hash,
  normalized with `hashes::hash_to_hex`. IDs never match when hashes
  exist.
- OneDrive delta now stores quickxor as lowercase hex; lsjson picks
  `quickxor`; `list` passes `--hash`.
- gdrive keeps sha1 as `stat`'s choice on purpose. The full-listing
  poller (`poller.rs`) compares stored vs listed ETag verbatim, so
  changing the hash family would mark every cached row stale.

Verified: rclone's remote `quickxor` hex equals the local
`rclone hashsum quickxor` of the downloaded file; `--base64` output
uses the URL-safe alphabet, Graph uses standard.

## Follow-up
- gdrive delta still stores md5 while full listings yield sha1, so a
  token-expiry fallback to a full listing marks delta-touched rows
  stale. The fix is to request `sha1Checksum` in the Drive changes
  fields.
- Legacy rows: at fix time gdrive had 548 cached rows with ID ETags and
  onedrv had 310 ID + 58 Graph-sha1 rows. Each goes through the resolver
  once.
