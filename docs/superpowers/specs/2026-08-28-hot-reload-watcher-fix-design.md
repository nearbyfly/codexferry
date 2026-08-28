# Hot-Reload Watcher Fix Design (rename-immune directory watch)

**Date:** 2026-08-28
**Status:** approved for implementation
**Base:** main @ `303d231`
**Diagnosis session:** opencodego-chat route addition failed to appear;
full root-cause investigation in the 2026-08-28 session (local notes).

## Problem

`spawn_watcher` (`src/config.rs:802-857`) watches the **config file's
inode** (`watcher.watch(&path, NonRecursive)`; inotify follows the
`~/bin/cxf.toml` symlink to the repo file's inode). Editors that save
atomically (temp file + `rename(2)`) replace the file's inode; the kernel
delivers `IN_IGNORED` on the old inode, notify removes the watch
descriptor, and nothing re-arms it — **the watcher is permanently deaf
from the first atomic save onward** (one last event may fire during the
rename itself, which the ENOENT-retry path consumes).

Diagnostic evidence (2026-08-28 session):

- The daemon's inotify fd was alive with **zero watch descriptors**
  (`/proc/<pid>/fdinfo/<fd>` had no `wd:` lines) after the user's
  editor save; `touch` and symlink recreation fired nothing.
- Journal for 7 days: **zero** `"config reloaded successfully"` lines —
  hot-reload has never fired via a config edit on this host; frequent
  daemon restarts during the release-heavy week masked the dead watcher
  (each restart re-armed it and loaded the new config at startup).
- The watch call is unchanged since the repo import
  (`git log -S "watcher.watch"` → import only); the comment claiming
  "the parent directory is what actually gets watched" is wrong —
  notify's inotify backend watches the file inode for a file path.
- Test layers all write configs **in-place** (`std::fs::write`, shell
  `>` redirection → same inode), so the broken save style was never
  exercised anywhere in the pyramid.

## Goal

Config edits reach the running daemon regardless of save style (in-place
write, atomic temp+rename) and regardless of whether the configured path
is a symlink, for the daemon's whole lifetime — no silent deafness.

## Design

### Watch target: canonicalized parent directory

At `spawn_watcher` start:

1. `path.canonicalize()` — resolves symlinks (the `~/bin/cxf.toml` →
   repo-file layout binds the watch to the REAL file's directory).
   On failure (file momentarily absent): fall back to watching the
   UN-resolved path's parent directory and `warn!` — never panic,
   matching the module's degrade posture.
2. Split into `(parent_dir, file_name)`; `watcher.watch(&parent_dir,
   RecursiveMode::NonRecursive)`. The directory's inode is stable across
   file replacements, so rename-saves, in-place writes, and editor
   atomic saves all produce events on it.

**Documented limitation:** the symlink is resolved once at startup.
Re-pointing or replacing the symlink itself does not re-bind the watch;
that requires a daemon restart. Editing the target file through either
path works (both write the watched directory's entry).

### Event filter: kind + filename

The callback keeps the `EventKind::Modify(_) | EventKind::Create(_)`
gate and adds a path filter: the event is relevant only when some entry
of `event.paths` has a final component equal to `file_name`. Editor
temporary files (`.swp`, `4913`, unsaved-file fragments) and unrelated
directory activity are ignored. The parse+ENOENT-retry body is unchanged
(it still parses `watch_path` — now the canonicalized file path — and
sends into the channel; defense-in-depth against exotic event orderings).

### Comment correction

The misleading block above `watcher.watch` is rewritten to state the
actual mechanics: directory watch + filename filter, why an inode watch
dies on rename (IN_IGNORED, no re-arm), and the symlink-resolved-once
limitation.

## Testing

- **Integration regression (the missing pin):** in
  `tests/endpoints_metrics.rs`, a test performs **two successive
  editor-style saves** — write a temp file in the config's directory,
  `fs::rename` it over the config (new inode each time) — and after
  EACH save polls `/v1/models?client_version=…` until the new route
  appears (deadline ~5s). The second save is the assertion's soul: it
  proves the first replacement did not kill the watch. Red on current
  code (second poll times out), green after the fix.
- **Integration, symlink layout:** the daemon's `CODEXFERRY_CONFIG`
  points at a symlink to the real config in another directory; an
  atomic-rename edit of the real file triggers the reload — mirrors the
  production layout from this incident.
- **Existing coverage stays:** `models_endpoint_reflects_hot_reload`
  (in-place `fs::write`) must keep passing — the benign save style
  remains supported.
- **Unit:** the filename-relevance predicate (pure function over
  `&[PathBuf]` + `&str`) gets direct unit tests including the
  `.swp`-sibling and renamed-from cases.

## Out of scope

- The `/v1/models` SWR work (separate spec/plan, already queued).
- Watching both the symlink's directory and the resolved directory
  (dual watch) — the canonicalize-once limitation is accepted and
  documented instead.
- Any e2e scenario addition — the integration layer covers the
  mechanics deterministically and faster.
- notify backend swaps (PollWatcher) or IN_IGNORED re-arm hacks.
