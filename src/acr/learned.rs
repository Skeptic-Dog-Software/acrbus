//! What a repository's reviewers open: learned from their tool calls, kept
//! per repository and per lens, so a round's facts can carry the files the
//! last rounds' reviewers kept reading, and a reviewer reads them from the
//! prompt cache instead of spending turns to open them.
//!
//! Nothing is taken away by it: a reviewer still opens whatever it likes.
//! The record is counts only: for each lens, how many reviewer calls opened
//! each file. A file's text comes from the checkout under review, never
//! from here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The most files the record keeps for a lens: the least opened go first.
const MOST_PER_LENS: usize = 200;

/// The fewest reviewer calls, across lenses, that must have opened a file
/// for a round's facts to carry it.
pub const MIN_OPENS: u32 = 2;

/// For each lens, how many reviewer calls opened each file, by its path in
/// the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Learned {
    #[serde(default)]
    pub lenses: BTreeMap<String, BTreeMap<String, u32>>,
}

impl Learned {
    /// The record for repository `name` under `dir`.
    pub fn path(dir: &Path, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        dir.join(format!("{safe}.json"))
    }

    /// The record at `path`, or an empty one if there's none or it can't be
    /// read: what it holds is only a hint.
    pub fn load(path: &Path) -> Learned {
        fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Writes it whole, through a file beside it, so a reader never sees
    /// half of it.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let part = path.with_extension("json.part");
        fs::write(&part, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", part.display()))?;
        fs::rename(&part, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// One reviewer call of `lens` opened `files`: each counts once.
    pub fn add(&mut self, lens: &str, files: &[String]) {
        let counts = self.lenses.entry(lens.to_string()).or_default();
        let once: BTreeSet<&String> = files.iter().collect();
        for f in once {
            *counts.entry(f.clone()).or_default() += 1;
        }
        if counts.len() > MOST_PER_LENS {
            let mut ranked: Vec<(String, u32)> =
                counts.iter().map(|(p, n)| (p.clone(), *n)).collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            ranked.truncate(MOST_PER_LENS);
            *counts = ranked.into_iter().collect();
        }
    }

    /// Every file the record holds, most opened first across all lenses,
    /// then by path, leaving out `except`.
    pub fn ranked(&self, except: &BTreeSet<String>) -> Vec<(String, u32)> {
        let mut total: BTreeMap<&String, u32> = BTreeMap::new();
        for counts in self.lenses.values() {
            for (path, n) in counts {
                if !except.contains(path) {
                    *total.entry(path).or_default() += n;
                }
            }
        }
        let mut ranked: Vec<(String, u32)> =
            total.into_iter().map(|(p, n)| (p.clone(), n)).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked
    }
}

/// `opened`, a path a reviewer's tool call named, as a path in the
/// repository at `repo`: relative paths as they are, and an absolute one by
/// its longest ending that names a file in `repo`, since a reviewer may
/// have read it in a copy of its own. None for a file outside the
/// repository, or one that isn't there.
pub fn in_repo(repo: &Path, opened: &str) -> Option<String> {
    let p = Path::new(opened.trim());
    let parts: Vec<&str> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let climbs = p.components().any(|c| c == Component::ParentDir);
    if parts.is_empty() || climbs {
        return None;
    }
    let is_file =
        |rel: &str| fs::symlink_metadata(repo.join(rel)).is_ok_and(|m| m.file_type().is_file());
    if p.is_relative() {
        let rel = parts.join("/");
        return is_file(&rel).then_some(rel);
    }
    (0..parts.len())
        .map(|i| parts[i..].join("/"))
        .find(|rel| is_file(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tb-learned-{}", crate::util::new_id()));
        fs::create_dir_all(d.join("src/deep")).unwrap();
        fs::write(d.join("src/lib.rs"), "lib").unwrap();
        fs::write(d.join("src/deep/a.rs"), "a").unwrap();
        fs::write(d.join("README.md"), "readme").unwrap();
        d
    }

    #[test]
    fn a_read_path_is_found_in_the_repository_however_it_was_named() {
        let repo = dir();
        assert_eq!(in_repo(&repo, "src/lib.rs").as_deref(), Some("src/lib.rs"));
        assert_eq!(
            in_repo(&repo, "./src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        // Read in a copy of its own, somewhere else entirely.
        assert_eq!(
            in_repo(&repo, "/elsewhere/copies/r1-x/repo/src/deep/a.rs").as_deref(),
            Some("src/deep/a.rs")
        );
        assert_eq!(
            in_repo(&repo, &repo.join("README.md").display().to_string()).as_deref(),
            Some("README.md")
        );
        // Not in the repository, a folder, or a climb out of it.
        assert_eq!(in_repo(&repo, "/etc/hosts"), None);
        assert_eq!(in_repo(&repo, "src"), None);
        assert_eq!(in_repo(&repo, "../outside.rs"), None);
        assert_eq!(in_repo(&repo, ""), None);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn opens_are_counted_per_call_and_ranked_across_lenses() {
        let mut l = Learned::default();
        l.add(
            "correctness",
            &["a.rs".into(), "a.rs".into(), "b.rs".into()],
        );
        l.add("correctness", &["a.rs".into()]);
        l.add("security", &["b.rs".into(), "c.rs".into()]);
        assert_eq!(
            l.lenses["correctness"]["a.rs"], 2,
            "a call counts a file once"
        );
        let ranked = l.ranked(&BTreeSet::new());
        assert_eq!(
            ranked,
            vec![("a.rs".into(), 2), ("b.rs".into(), 2), ("c.rs".into(), 1)],
            "most opened first, then by path"
        );
        let except = BTreeSet::from(["a.rs".to_string()]);
        assert_eq!(l.ranked(&except)[0].0, "b.rs");
    }

    #[test]
    fn the_record_keeps_a_lens_s_most_opened_files() {
        let mut l = Learned::default();
        for i in 0..(MOST_PER_LENS + 20) {
            let f = format!("f{i:04}.rs");
            l.add("x", std::slice::from_ref(&f));
            if i < 5 {
                l.add("x", &[f]);
            }
        }
        let kept = &l.lenses["x"];
        assert_eq!(kept.len(), MOST_PER_LENS);
        for i in 0..5 {
            assert_eq!(kept[&format!("f{i:04}.rs")], 2, "the most opened stay");
        }
    }

    #[test]
    fn the_record_round_trips_and_a_bad_one_reads_as_empty() {
        let d = dir();
        let path = Learned::path(&d.join("learned"), "my repo/x");
        assert!(path.ends_with("my_repo_x.json"));
        let mut l = Learned::default();
        l.add("lens", &["src/lib.rs".into()]);
        l.save(&path).unwrap();
        assert_eq!(Learned::load(&path), l);
        fs::write(&path, "not json").unwrap();
        assert_eq!(Learned::load(&path), Learned::default());
        assert_eq!(Learned::load(&d.join("none.json")), Learned::default());
        let _ = fs::remove_dir_all(&d);
    }
}
