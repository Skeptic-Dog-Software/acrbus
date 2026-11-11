//! Whether a repository's review config is calibrated: configured for what's
//! in the repository, and accepted by a person. `acr configure` proposes a
//! config from a review of the whole repository, and `acr calibrate` revises
//! it from what the person has said of its reviews (`acr feedback`). Either
//! way, a person reads the proposal and accepts it, with `acr calibration
//! --accept`, which also accepts a config they wrote themselves.
//!
//! `acr run` refuses a repository that was never calibrated, or whose config
//! has changed since, and warns when only the Judge has: a new Judge prompt
//! or rule is acrbus's doing, not the repository's.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::APP_NAME;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What a calibration was made against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The repository's config and its decided non-issues.
    pub config: String,
    /// The Judge's binding, its prompt, and the rules that decide what
    /// blocks.
    pub judge: String,
}

/// One comparison of the Judge with a past review's keys, which records
/// from before calibration was configuring still hold.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationRun {
    pub at: i64,
    pub keys: Vec<String>,
    pub fingerprint: Fingerprint,
    pub agreed: usize,
    pub missed: usize,
    pub overrated: usize,
    /// Key findings the Judge rated Rejected: real ones lost, unless the
    /// Judge is right that they aren't defects.
    pub dropped: usize,
}

/// A person read a calibration's disagreements and accepted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub at: i64,
    pub fingerprint: Fingerprint,
    /// How many key comparisons it was accepted on, for an acceptance from
    /// before; none now.
    #[serde(default)]
    pub runs: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub runs: Vec<CalibrationRun>,
    #[serde(default)]
    pub accepted: Option<Accepted>,
    /// The latest feedback a recalibration folded in: feedback after it is
    /// still to fold in.
    #[serde(default)]
    pub feedback_folded_at: Option<i64>,
}

/// Where a repository stands for `acr run`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Never,
    ConfigChanged,
    JudgeChanged,
    Calibrated,
}

pub fn standing(record: &Record, now: &Fingerprint) -> Standing {
    match &record.accepted {
        None => Standing::Never,
        Some(a) if a.fingerprint.config != now.config => Standing::ConfigChanged,
        Some(a) if a.fingerprint.judge != now.judge => Standing::JudgeChanged,
        Some(_) => Standing::Calibrated,
    }
}

/// A short, stable hash of some texts, in order.
pub fn hash(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    h.finalize()[..12]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The functions named, cut from a source file's text: each runs from its
/// `fn` line to the first line that's only `}`. Used to fingerprint the
/// rules, so changing them voids calibrations made before.
pub fn source_of(src: &str, starts: &[&str]) -> String {
    starts
        .iter()
        .filter_map(|start| {
            let i = src.find(start)?;
            let end = src[i..].find("\n}\n")?;
            Some(&src[i..i + end])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn path(state: &Path, repo: &str) -> PathBuf {
    state.join("acr-calibration").join(format!("{repo}.json"))
}

pub fn load(state: &Path, repo: &str) -> Result<Record> {
    let path = path(state, repo);
    match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn save(state: &Path, repo: &str, record: &Record) -> Result<()> {
    let path = path(state, repo);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(record)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Accepts the repository's config as it is now, against `now`. `folded`
/// is the latest feedback it folds in, if it came from a recalibration.
pub fn accept(
    state: &Path,
    repo: &str,
    now: &Fingerprint,
    at: i64,
    folded: Option<i64>,
) -> Result<Accepted> {
    let mut record = load(state, repo)?;
    let accepted = Accepted {
        at,
        fingerprint: now.clone(),
        runs: 0,
    };
    record.accepted = Some(accepted.clone());
    if let Some(folded) = folded {
        record.feedback_folded_at = Some(folded.max(record.feedback_folded_at.unwrap_or(0)));
    }
    save(state, repo, &record)?;
    Ok(accepted)
}

/// Where a repository's calibration stands, and its runs on the current
/// config and Judge.
pub fn report(
    name: &str,
    record: &Record,
    now: &Fingerprint,
    unfolded: usize,
    pending: Option<&Path>,
) -> String {
    let stands = match standing(record, now) {
        Standing::Calibrated => "calibrated".to_string(),
        Standing::JudgeChanged => "calibrated, but on an earlier Judge: recalibrate soon".into(),
        Standing::ConfigChanged => "not calibrated: its config has changed since".into(),
        Standing::Never => "not calibrated".into(),
    };
    let mut out = format!("{name}: {stands}.");
    if let Some(a) = &record.accepted {
        out.push_str(&format!(
            "\nIts config was last accepted {}.",
            crate::util::rfc3339(a.at)
        ));
    }
    if let Some(path) = pending {
        out.push_str(&format!(
            "\nA proposed config is waiting for you to read: {}. Accept it with \
             `{APP_NAME} acr calibration --accept`.",
            path.display()
        ));
    }
    if unfolded > 0 {
        out.push_str(&format!(
            "\n{unfolded} piece(s) of feedback on its reviews aren't in its config yet: \
             `{APP_NAME} acr calibrate` folds them in."
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(config: &str, judge: &str) -> Fingerprint {
        Fingerprint {
            config: config.into(),
            judge: judge.into(),
        }
    }

    #[test]
    fn accepting_a_config_calibrates_it_until_the_config_changes() {
        let state = std::env::temp_dir().join(format!("tb-cal-{}", crate::util::new_id()));
        let now = fp("c1", "j1");
        let record = load(&state, "r").unwrap();
        assert_eq!(standing(&record, &now), Standing::Never);
        assert!(report("r", &record, &now, 0, None).contains("not calibrated"));
        // Accepting it is the calibration: no runs needed.
        accept(&state, "r", &now, 3, None).unwrap();
        let record = load(&state, "r").unwrap();
        assert_eq!(standing(&record, &now), Standing::Calibrated);
        assert_eq!(record.feedback_folded_at, None);
        // A changed config refuses; a changed Judge only warns.
        assert_eq!(standing(&record, &fp("c2", "j1")), Standing::ConfigChanged);
        assert_eq!(standing(&record, &fp("c1", "j2")), Standing::JudgeChanged);
        // A recalibration's acceptance records the feedback it folded in,
        // never moving it back.
        accept(&state, "r", &now, 5, Some(40)).unwrap();
        accept(&state, "r", &now, 6, Some(30)).unwrap();
        assert_eq!(load(&state, "r").unwrap().feedback_folded_at, Some(40));
        let shown = report(
            "r",
            &load(&state, "r").unwrap(),
            &now,
            2,
            Some(Path::new("/c/r.proposed.yaml")),
        );
        assert!(
            shown.contains("r: calibrated.")
                && shown.contains("/c/r.proposed.yaml")
                && shown.contains("2 piece(s) of feedback"),
            "{shown}"
        );
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_function_is_cut_from_its_source() {
        let src = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        assert_eq!(source_of(src, &["fn b("]), "fn b() {\n    2");
        assert_eq!(hash(&["x", "y"]), hash(&["x", "y"]));
        assert_ne!(hash(&["x", "y"]), hash(&["xy", ""]));
    }
}
