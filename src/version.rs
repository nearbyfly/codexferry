//! Codex client-version observation and doctor state.
//!
//! Owns three small concerns shared by the daemon and the `doctor`
//! subcommand (spec 2026-08-22 §3):
//! - [`normalize_version`]: make `codex --version` output and the
//!   `client_version` query parameter comparable;
//! - [`CodexVersionTracker`]: per-process first-sighting detection;
//! - [`DoctorState`]: the `last_green` state file under XDG state.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Make version-ish strings from different sources comparable.
///
/// `codex --version` prints e.g. `codex-cli 0.158.0` while the
/// `client_version` query parameter is usually already bare (`0.158.0`).
/// Rule (spec §3.2): the LAST whitespace-separated token containing an
/// ASCII digit; versions are otherwise opaque — no semver parsing.
pub fn normalize_version(s: &str) -> Option<String> {
    s.split_whitespace()
        .rev()
        .find(|t| t.bytes().any(|b| b.is_ascii_digit()))
        .map(str::to_string)
}

/// A first-sighting event: `from` is the previous `current` (`None` at
/// process start), `to` the newly seen version.
#[derive(Debug)]
pub struct Transition {
    /// The previous value of `current`; `None` at process start.
    pub from: Option<String>,
    /// The version just observed.
    pub to: String,
}

/// Per-process codex client-version tracker (spec §3).
///
/// Logging/metrics fire ONCE per distinct version per process (the `seen`
/// set), so two codex versions alternating requests cannot spam the log.
/// `current` tracks the most recent version so a transition can name both
/// sides. All methods are sync and lock-only — safe from the async
/// handlers.
#[derive(Default)]
pub struct CodexVersionTracker {
    seen: Mutex<HashSet<String>>,
    current: Mutex<Option<String>>,
}

impl CodexVersionTracker {
    /// Create a tracker that has seen nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update `current` on every call; return `Some(Transition)` only when
    /// `version` is newly added to `seen` (its first sighting this process).
    ///
    /// Past [`MAX_TRACKED_VERSIONS`] distinct labels, new labels are treated
    /// as already-seen (silent): `client_version` is caller-supplied and the
    /// set previously grew one permanent entry per distinct string. The cap
    /// keeps the set — and the per-version metric series — bounded; real
    /// deployments see a handful of versions.
    pub fn observe(&self, version: &str) -> Option<Transition> {
        let mut current = self.current.lock().unwrap();
        let from = current.clone();
        *current = Some(version.to_string());
        let mut seen = self.seen.lock().unwrap();
        if seen.len() < MAX_TRACKED_VERSIONS && seen.insert(version.to_string()) {
            Some(Transition {
                from,
                to: version.to_string(),
            })
        } else {
            None
        }
    }
}

/// Per-process cap on the [`CodexVersionTracker`] `seen` set: past this many
/// distinct labels, new ones stay silent (see [`CodexVersionTracker::observe`]).
const MAX_TRACKED_VERSIONS: usize = 64;

/// Result of the most recent doctor run (spec §3.1). Timestamped as epoch
/// seconds — the repo deliberately has no chrono dependency.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LastRun {
    /// Whether that run ended green (every check passed).
    pub green: bool,
    /// The codex version observed during that run, if it could be read.
    pub codex_version: Option<String>,
    /// When the run finished, in seconds since the Unix epoch.
    pub at_unix: u64,
    /// One-line human summary of the run's verdict.
    pub summary: String,
}

/// Deserialize `last_run` leniently: any value that is not a well-formed
/// [`LastRun`] becomes `None` instead of failing the whole [`DoctorState`].
///
/// Why this exists: serde rejects the ENTIRE struct when a field that is
/// present has the wrong shape. Without this, a `last_run` written by a
/// different revision of the schema (or hand-edited from an older spec doc)
/// would make `DoctorState::read_from` fall back to `default()` and discard
/// a perfectly valid `last_green` — the one field the daemon reads. The
/// daemon would then warn "not verified" after a genuinely green run.
///
/// `last_run` is advisory (human-facing summary), `last_green` is load-
/// bearing, so degrading the former to keep the latter is the right trade.
/// Buffering through [`serde_json::Value`] is what makes the failure
/// recoverable: it consumes the field's tokens unconditionally, then the
/// typed conversion is allowed to fail.
fn deserialize_lenient_last_run<'de, D>(deserializer: D) -> Result<Option<LastRun>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Doctor's persisted state (spec §3.1). Doctor is the ONLY writer; the
/// daemon only reads. A missing or malformed file means "never green".
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct DoctorState {
    /// The codex version of the last GREEN run. A red run leaves this
    /// untouched, so the daemon's "unverified" warning persists until the
    /// next green run.
    pub last_green: Option<String>,
    /// The last run of either colour, green or red. Absent OR unparseable
    /// ⇒ `None`; an unparseable value never invalidates `last_green`.
    #[serde(default, deserialize_with = "deserialize_lenient_last_run")]
    pub last_run: Option<LastRun>,
}

/// Where the state file lives: `$XDG_STATE_HOME/codexferry/doctor.json`,
/// defaulting to `~/.local/state/codexferry/doctor.json` (spec §3.1 —
/// daemon-owned state stays OUT of `~/.codex/`).
pub fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/state")
        });
    base.join("codexferry").join("doctor.json")
}

impl DoctorState {
    /// Read from `path`; missing/malformed → default (never green).
    pub fn read_from(path: &Path) -> DoctorState {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write atomically (tmp + rename), creating parent dirs. Write
    /// failures are the caller's to log — they never change a verdict.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Read the production state file (spec §3.1 path rule).
    pub fn read() -> DoctorState {
        Self::read_from(&state_path())
    }

    /// Write the production state file.
    pub fn write(&self) -> std::io::Result<()> {
        self.write_to(&state_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_extracts_last_digit_token() {
        assert_eq!(
            normalize_version("codex-cli 0.158.0"),
            Some("0.158.0".to_string())
        );
        assert_eq!(normalize_version("0.158.0"), Some("0.158.0".to_string()));
        // The LAST token carrying a digit wins, per spec §3.2.
        assert_eq!(normalize_version("codex 1.2 beta 3"), Some("3".to_string()));
    }

    #[test]
    fn normalize_returns_none_without_digits() {
        assert_eq!(normalize_version(""), None);
        assert_eq!(normalize_version("no digits here"), None);
        assert_eq!(normalize_version("   "), None);
    }

    #[test]
    fn first_sighting_returns_transition_from_none() {
        let t = CodexVersionTracker::new();
        let tr = t.observe("0.1.0").expect("first sighting fires");
        assert_eq!(tr.from, None);
        assert_eq!(tr.to, "0.1.0");
    }

    #[test]
    fn repeat_sighting_is_silent() {
        let t = CodexVersionTracker::new();
        t.observe("0.1.0");
        assert!(t.observe("0.1.0").is_none());
        // Flapping back and forth: each DISTINCT version logs once only.
        assert!(t.observe("0.2.0").is_some());
        assert!(t.observe("0.1.0").is_none());
        assert!(t.observe("0.2.0").is_none());
    }

    #[test]
    fn second_version_transitions_from_previous_current() {
        let t = CodexVersionTracker::new();
        t.observe("0.1.0");
        let tr = t.observe("0.2.0").expect("second distinct version fires");
        assert_eq!(tr.from.as_deref(), Some("0.1.0"));
        assert_eq!(tr.to, "0.2.0");
    }

    /// Pins the "`current` advances on EVERY call" half of `observe`'s
    /// contract. This is the ONLY test that distinguishes a correct
    /// implementation from one that early-returns `None` on a re-sighting
    /// without updating `current`: every other test reads `current` (via
    /// `Transition::from`) only after a FIRST sighting, so they all still
    /// pass under that broken variant. Not redundant with the flapping
    /// test — that one asserts which calls fire, this one asserts the
    /// silent call still moved `current`. Do not delete.
    #[test]
    fn silent_resighting_still_advances_current() {
        let t = CodexVersionTracker::new();
        t.observe("0.1.0");
        t.observe("0.2.0");
        assert!(t.observe("0.1.0").is_none()); // silent, but must move current
        let tr = t.observe("0.3.0").expect("third distinct version fires");
        assert_eq!(tr.from.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn read_missing_or_malformed_defaults_to_never_green() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        assert_eq!(
            DoctorState::read_from(&dir.path().join("nope.json")).last_green,
            None
        );
        // Malformed file.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert_eq!(DoctorState::read_from(&bad).last_green, None);
    }

    #[test]
    fn write_then_read_round_trips_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/doctor.json");
        let state = DoctorState {
            last_green: Some("0.158.0".into()),
            last_run: Some(LastRun {
                green: true,
                codex_version: Some("codex-cli 0.158.0".into()),
                at_unix: 12345,
                summary: "all checks passed".into(),
            }),
        };
        state.write_to(&path).unwrap();
        let back = DoctorState::read_from(&path);
        assert_eq!(back.last_green.as_deref(), Some("0.158.0"));
        assert!(back.last_run.unwrap().green);
    }

    /// The field the daemon actually depends on is `last_green`. A
    /// `last_run` written by an older spec revision (or a future one) must
    /// NOT take it down with it: serde fails the WHOLE struct on a present
    /// field of the wrong shape, which used to silently reset `last_green`
    /// to `None` and make the daemon warn "not verified" after a green run.
    #[test]
    fn malformed_last_run_preserves_last_green() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doctor.json");
        // Exactly the shape the old spec doc documented: `status`/`at`
        // instead of `green`/`at_unix`.
        std::fs::write(
            &path,
            r#"{
  "last_green": "0.158.0",
  "last_run": {
    "status": "green",
    "codex_version": "0.158.0",
    "at": "2026-08-22T12:34:56Z",
    "summary": "all checks passed"
  }
}"#,
        )
        .unwrap();
        let state = DoctorState::read_from(&path);
        assert_eq!(
            state.last_green.as_deref(),
            Some("0.158.0"),
            "a malformed last_run must not destroy last_green"
        );
        assert!(
            state.last_run.is_none(),
            "an unparseable last_run degrades to None"
        );
    }

    /// Other malformed `last_run` shapes degrade the same way: wrong JSON
    /// type, missing required fields, wrong field types.
    #[test]
    fn last_run_of_any_wrong_shape_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        for (i, body) in [
            r#"{"last_green":"1.0.0","last_run":"green"}"#,
            r#"{"last_green":"1.0.0","last_run":[]}"#,
            r#"{"last_green":"1.0.0","last_run":null}"#,
            r#"{"last_green":"1.0.0","last_run":{}}"#,
            r#"{"last_green":"1.0.0","last_run":{"green":"yes","at_unix":1,"summary":"s"}}"#,
        ]
        .iter()
        .enumerate()
        {
            let path = dir.path().join(format!("s{i}.json"));
            std::fs::write(&path, body).unwrap();
            let state = DoctorState::read_from(&path);
            assert_eq!(
                state.last_green.as_deref(),
                Some("1.0.0"),
                "last_green lost for input: {body}"
            );
            assert!(state.last_run.is_none(), "expected None for input: {body}");
        }
    }

    /// Regression guard for the `#[serde(default)]` half of the contract:
    /// an ABSENT `last_run` must still deserialize to `None`, not error.
    #[test]
    fn absent_last_run_still_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doctor.json");
        std::fs::write(&path, r#"{"last_green":"0.158.0"}"#).unwrap();
        let state = DoctorState::read_from(&path);
        assert_eq!(state.last_green.as_deref(), Some("0.158.0"));
        assert!(state.last_run.is_none());
    }

    /// A well-formed `last_run` in the real wire format still parses fully —
    /// tolerance must not swallow valid data.
    #[test]
    fn valid_last_run_parses_from_wire_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doctor.json");
        std::fs::write(
            &path,
            r#"{
  "last_green": "0.158.0",
  "last_run": {
    "green": false,
    "codex_version": "0.159.0",
    "at_unix": 1755865496,
    "summary": "codex wiring check failed"
  }
}"#,
        )
        .unwrap();
        let run = DoctorState::read_from(&path).last_run.expect("parses");
        assert!(!run.green);
        assert_eq!(run.codex_version.as_deref(), Some("0.159.0"));
        assert_eq!(run.at_unix, 1_755_865_496);
        assert_eq!(run.summary, "codex wiring check failed");
    }

    #[test]
    fn state_path_is_never_under_codex_home() {
        let p = state_path().to_string_lossy().to_string();
        assert!(
            !p.contains(".codex/"),
            "state must live on codexferry's side: {p}"
        );
        assert!(
            p.ends_with("codexferry/doctor.json"),
            "unexpected path: {p}"
        );
    }
}
