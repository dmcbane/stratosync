# Releasing stratosync

Releases are tag-driven. `.github/workflows/release.yml` triggers on every
push of a tag matching `v*`, builds binaries for x86_64 and aarch64 via
`cross`, packages them as a tarball / `.deb` / `.rpm`, and creates a GitHub
Release with all artifacts attached and auto-generated release notes.

## Cutting a normal release

1. **Decide the version**, in Cargo SemVer form. Examples:
   - `0.12.0` — stable
   - `0.12.0-beta.1` — pre-release (GitHub auto-flags pre-release on the tag)
   - `0.12.1` — patch on top of an existing minor

2. **Bump the workspace version** in `Cargo.toml`:
   ```toml
   [workspace.package]
   version = "0.12.0-beta.2"
   ```
   `cargo build` will re-resolve `Cargo.lock` automatically; commit both files.

3. **Close out the CHANGELOG section.** Rename
   `## [0.12.0-beta.1] - YYYY-MM-DD` (the previous header) and add a new
   `## [Unreleased]` block above it for the next cycle, or — if you're
   keeping the simpler "latest in flight" pattern this repo currently uses
   — just rename the in-flight header from `unreleased` to today's date and
   start the next entry on top.

4. **Land the version-bump PR** through the normal review process. Don't
   tag uncommitted main.

5. **Tag and push** from `main` after the PR merges:
   ```bash
   git checkout main && git pull --ff-only
   git tag -a v0.12.0-beta.2 -m "Stratosync v0.12.0-beta.2"
   git push origin v0.12.0-beta.2
   ```

6. **Watch the workflow:**
   ```bash
   gh run watch $(gh run list --workflow=release.yml --limit 1 --json databaseId -q '.[0].databaseId')
   ```
   On success (~5 minutes) the artifacts land at:
   `https://github.com/dmcbane/stratosync/releases/tag/v0.12.0-beta.2`

7. **Verify** by downloading one of the artifacts on a clean box and
   running `stratosync version` / `stratosyncd --version`. Both should
   report the new version.

## What ships in each release

Per arch (`x86_64`, `aarch64`):
- `stratosync-VERSION-linux-ARCH.tar.gz` — portable bundle with binaries +
  systemd unit + Nautilus extension + license/readme.
- `stratosync_VERSION_ARCH.deb` — Debian/Ubuntu package.
- `stratosync-VERSION-1.ARCH.rpm` — Fedora/RHEL package.

The AUR package (`stratosync-git`) tracks `main` and is independent of the
release pipeline.

## Recovering from a botched release

If the workflow fails or the artifacts are wrong, the cleanest fix is to
**move the tag**. This is safe when no one has pulled the broken release
yet (artifacts didn't get uploaded if the workflow failed early).

```bash
# 1. Land the fix on main as a normal PR.

# 2. After merge, pull and move the tag:
git checkout main && git pull --ff-only
git tag -d v0.12.0-beta.1                    # delete locally
git push origin :refs/tags/v0.12.0-beta.1    # delete remotely
git tag -a v0.12.0-beta.1 -m "Stratosync v0.12.0-beta.1"
git push origin v0.12.0-beta.1               # re-triggers release.yml
```

If a GitHub Release was already created and contains attached artifacts,
delete it via the web UI first (the `gh release delete` command works
too), or the `softprops/action-gh-release` step will refuse to overwrite.

For a release that's already been distributed, **don't move the tag** —
cut a new patch release (e.g. `v0.12.0-beta.2`) instead. The `Cargo.toml`
SemVer pin discipline will protect downstream consumers.

## Updating example download URLs in docs

The Quick Start sections in `README.md`, `docs/index.html`, and
`docs/installation.md` reference specific filenames like
`stratosync_0.12.0_amd64.deb`. These are illustrative — the page tells
users to "replace `0.12.0` with the latest release version" — but worth
keeping current. There's no automation; bump them as part of the
version-bump PR.
