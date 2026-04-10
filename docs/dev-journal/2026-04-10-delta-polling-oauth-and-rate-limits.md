# Delta Polling: OAuth Token Refresh and Google Drive Rate Limits

**Date:** 2026-04-10
**Context:** Implementing Google Drive and OneDrive delta (change token) polling (v0.7.0-v0.7.2)
**Related Commits:** e49ce69, bb8d4c9, 857f4c4, a2915a2, d4e00f4, 284edee, 76748c1

## Problem

After implementing delta polling (Google Drive Changes API with `pageToken`), the daemon hit a cascade of OAuth and rate-limit issues when running against a real Google Drive remote:

1. **403 rate limits misclassified as PermissionDenied** — Google returns quota errors as HTTP 403 with `rateLimitExceeded` in the body, not HTTP 429. The poller treated these as hard auth failures instead of backing off.

2. **"Could not determine client ID from request"** — When rclone uses its built-in shared OAuth credentials (most users), the `client_id` isn't stored in the rclone config. Our direct token refresh sent empty credentials to Google's token endpoint.

3. **Stale token after expiry** — `rclone config show` just reads the config file; it does NOT trigger an OAuth refresh. Re-reading the config returned the same expired token.

4. **Redundant full listings on rate limit** — When `get_start_token()` failed after a successful full listing, the token was never stored. Every retry re-ran the expensive full listing.

## Investigation

Each issue was discovered through daemon logs on a live Google Drive mount (`gdrvtest:`):

| Issue | Log Pattern | Root Cause |
|-------|-------------|------------|
| 403 rate limit | `permission denied: HTTP 403 Forbidden: ... rateLimitExceeded` | `map_http_error` checked status before body |
| Empty client_id | `token refresh HTTP 400 Bad Request: Could not determine client ID` | `config.get("client_id").unwrap_or_default()` → empty string |
| Stale token | `permission denied: HTTP 401 Unauthorized: Invalid Credentials` | `rclone config show` is read-only, doesn't refresh |
| Redundant listing | `no change token; running initial full listing` (repeated) | Token never stored when `get_start_token` failed |

## Solution

1. **Rate limit detection** (857f4c4): Check response body for `rateLimitExceeded`, `RATE_LIMIT_EXCEEDED`, `userRateLimitExceeded` BEFORE the status code match. Map to `SyncError::Transient`.

2. **Always use rclone for token refresh** (d4e00f4): Removed direct OAuth refresh entirely. Both providers re-read the token from `rclone config show`, which works regardless of how credentials were configured.

3. **Two-step token refresh** (284edee): `rclone about <remote>: --json` forces rclone to authenticate (auto-refreshing the token), THEN `rclone config show` reads the fresh token. Encapsulated in `get_fresh_oauth_token()`.

4. **Skip redundant listings** (76748c1): Check `snapshot_remote_index()` — if DB already has entries, skip the listing and only retry `get_start_token()`. Also added 401 retry with force-refresh on all API calls.

## Key Learnings

- Google Drive uses HTTP 403 for rate limits, not 429. Always check the response body before mapping status codes.
- rclone's built-in shared client_id (`project_number:202264815644`) has very tight per-minute quotas shared across ALL rclone users. Users should configure their own Google Cloud project for reliable operation.
- `rclone config show` is purely a config file read — it never triggers network operations. You must run an actual rclone command (like `about`) to trigger token refresh.
- When designing a multi-step initialization (listing + token), make each step idempotent so failures partway through don't restart from scratch.

## Tags

`#oauth` `#google-drive` `#rate-limiting` `#rclone` `#delta-polling`
