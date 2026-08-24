# Release Process

This document covers **when** to release. The mechanics live in
[`scripts/release.sh`](./scripts/release.sh).

## Versioning

Strict [semver](https://semver.org/) on `Cargo.toml`'s `version`:

- **patch** (`v0.X.Y`): bug fixes, doc-only, e2e coverage additions.
  Backwards-compatible; no API expectation from end users yet (pre-1.0).
- **minor** (`v0.X.0`): new user-visible capability, any new mode of
  `doctor`, any new e2e scenario type, any e2e harness rewrite that
  exercises a different contract.
- **major** (`vX.0.0`): first release that promises a stable public API
  for downstream tooling. Not yet declared; stay on `0.x` until the
  config schema and the `codexferry doctor` exit-code matrix are stable.

## Cadence

Recommended: **release when something worth noting lands**, not on a
calendar. Two natural triggers:

- After a PR merges that closes a class of bugs / adds a new user-visible
  feature. The PR description's "what changed" becomes the release body.
- When `cargo test` or `scripts/e2e.sh all` hits a wall that needs a
  labeled commit to mark "this is the snapshot we're claiming works".

Avoid creating releases just to bump a number. Each release entry should
be meaningful to a user.

## Operating cadence (suggestion)

| Trigger | Action |
|---|---|
| Internal bug fix lands | Nothing — let commits accumulate |
| First PR after a meaningful chunk closes | Run `scripts/release.sh v0.X.(Y+1)` (or new minor) |
| Monthly, if there's any `feat:` commit since the last release | Run it then |
| A `feat:` in the last two weeks warrants a `0.X+1.0` release | Just do it |

## One-shot setup (run once)

```bash
# Add the GitHub remote if not present (replace placeholder if different).
git remote add github git@github.com:nearbyfly/codexferry.git
git remote -v  # confirm both remotes

# Export a GitHub PAT with `repo` scope for the Release API call.
echo 'export GITHUB_TOKEN=ghp_xxx' >> ~/.bashrc
```

## Routine release

```bash
# 1. Confirm everything green on main.
git checkout main
git pull --ff-only
cargo build && cargo test
scripts/e2e.sh all

# 2. Dry-run first (sees what would happen, no network).
scripts/release.sh v0.1.1 --dry-run

# 3. Real run. Use --bump-cargo when Cargo.toml and the tag should
#    move together; skip when you want to publish source-only with an
#    unchanged Cargo.toml (rare).
scripts/release.sh v0.1.1 --bump-cargo

# 4. Verify on github.com/nearbyfly/codexferry/releases that the release
#    exists and the changelog body was populated.
```

## Why `docs/` is stripped from the GitHub mirror

`docs/superpowers/{specs,plans}/` is internal design workflow
(references like `spec §N` are for the AI agents working on the repo,
not for users of the binary). It also includes the **local Gitea URL**
in some examples, which would be noise on a public mirror. The
`release.sh` script does `git rm -rf docs` from the release branch so
the GitHub tag never carries it. The `main` branch keeps everything.

## Rollback

A bad release can be retracted without history rewrite:

```bash
# On GitHub: edit the release, mark it "draft" or delete via API.
# On both remotes: delete the tag and the release branch (branches
# in this repo's history are deletable; tags are deletable on the
# default branch only with force).
git push origin --delete release/v0.1.1
git push github --delete release/v0.1.1
git tag --delete v0.1.1
git push origin --delete v0.1.1
git push github --delete v0.1.1
```

The Gitea-only commits stay in `main` untouched; the public mirror is
the only thing that disappears. Fix forward with a new tag.
