//! A repository's labels, as the names its review's lenses share.
//!
//! Several people can calibrate one repository's review, each with a config
//! of their own. The labels on the repository are what they share, so a
//! calibration reads them all first, says which name a kind of review
//! concern, and keeps the names they already give each one. A new name comes
//! only from a person accepting it, and accepting a config is the only thing
//! that creates labels: reviews put existing ones on a change.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::config::{Config, Labels};

/// A label on the repository, as GitHub has it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoLabel {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// All of the repository's labels, read with `gh`. An error says why they
/// couldn't be: no GitHub remote, or `gh` not signed in.
pub fn read(repo: &Path) -> Result<Vec<RepoLabel>> {
    let out = Command::new("gh")
        .args([
            "label",
            "list",
            "--limit",
            "1000",
            "--json",
            "name,description",
        ])
        .current_dir(repo)
        .output()
        .context("running gh")?;
    if !out.status.success() {
        bail!(
            "gh label list: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("reading what gh label list gave")
}

/// What a label is, as the calibration sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// It names a kind of review concern, in whatever pattern.
    Dimension,
    /// A type, size, process or any other label.
    Other,
}

/// One label, classified by the calibration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Class {
    pub label: String,
    pub kind: Kind,
    /// The lens a dimension's label names.
    #[serde(default)]
    pub lens: Option<String>,
    /// A dimension that matters in the repository now, so its lens is in
    /// the config.
    #[serde(default)]
    pub relevant: Option<bool>,
    /// The evidence for it.
    #[serde(default)]
    pub why: String,
}

pub fn parse(text: &str) -> Result<Vec<Class>> {
    serde_yaml_ng::from_str(text).context("reading the ```labels block, a YAML list")
}

/// The calibration's brief on the repository's labels.
pub fn brief(labels: &[RepoLabel]) -> String {
    if labels.is_empty() {
        return "# The repository's labels\n\nIt has none yet, so name its lenses as you \
                see fit.\n\n"
            .to_string();
    }
    let mut out = String::from(
        "# The repository's labels\n\n\
         Several people may calibrate this repository's review, each with a config of their \
         own. Its labels are the names they share, so reuse them. These are all of them:\n\n",
    );
    for l in labels {
        if l.description.trim().is_empty() {
            out.push_str(&format!("- `{}`\n", l.name));
        } else {
            out.push_str(&format!("- `{}`: {}\n", l.name, l.description.trim()));
        }
    }
    out.push_str(
        "\nClassify every one of them in a ```labels block: a YAML list with one entry for each \
         label, `{label, kind, lens, relevant, why}`.\n\
         - `kind: dimension` when the label names a kind of review concern, whatever its \
         pattern, such as `confirmed:correctness`, `review:security` or `perf`. `lens` is the \
         lens it names, and `relevant` whether that concern matters in this repository now, \
         with the evidence in `why`. A relevant one's lens is in your config, under the name \
         the labels already give it. One that isn't relevant is left out of the config, and \
         its label stays.\n\
         - `kind: other` for anything else, such as a type, size or process label.\n\n\
         Several labels can name one lens, such as `wait:x` and `confirmed:x`. Name a lens \
         anew only for a concern no label names, and say in `## Why` why none fits. If the \
         labels already show a pattern for a lens's state, such as `wait:{lens}` and \
         `confirmed:{lens}`, set `integrations.labels` to it.\n\n",
    );
    out
}

/// Holds a calibration's classification to the repository's labels: every
/// label classified once, none made up, and each relevant concern's lens in
/// the config.
pub fn check(cfg: &Config, repo: &[RepoLabel], classes: Option<&[Class]>) -> Result<()> {
    if repo.is_empty() {
        return Ok(());
    }
    let Some(classes) = classes else {
        bail!(
            "the repository has {} label(s), and the answer has no ```labels block \
             classifying them",
            repo.len()
        );
    };
    let names: BTreeSet<&str> = repo.iter().map(|l| l.name.as_str()).collect();
    let mut seen = BTreeSet::new();
    let mut wrong = Vec::new();
    for c in classes {
        if !names.contains(c.label.as_str()) {
            wrong.push(format!("{} isn't one of the repository's labels", c.label));
            continue;
        }
        if !seen.insert(c.label.as_str()) {
            wrong.push(format!("{} is classified twice", c.label));
        }
        if c.kind != Kind::Dimension {
            continue;
        }
        match (&c.lens, c.relevant) {
            (Some(lens), Some(true)) if !cfg.lenses.iter().any(|l| &l.id == lens) => {
                wrong.push(format!(
                    "{} names lens {lens}, relevant here, but the config has no lens {lens}",
                    c.label
                ))
            }
            (Some(_), Some(_)) => {}
            _ => wrong.push(format!(
                "{} is a dimension, so it needs `lens` and `relevant`",
                c.label
            )),
        }
    }
    let missed: Vec<&str> = names.difference(&seen).copied().collect();
    if !missed.is_empty() {
        wrong.push(format!("not classified: {}", missed.join(", ")));
    }
    if !wrong.is_empty() {
        bail!(
            "the classification of the repository's labels doesn't hold: {}",
            wrong.join("; ")
        );
    }
    Ok(())
}

/// The labels a config could put on a change: its overall two, then each
/// lens's two.
pub fn wanted(names: &Labels, lenses: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = names
        .ready
        .iter()
        .chain(&names.not_ready)
        .cloned()
        .collect();
    for lens in lenses {
        for pattern in [&names.waiting, &names.confirmed].into_iter().flatten() {
            out.push(pattern.replace("{lens}", lens));
        }
    }
    out
}

/// The lens a label names under `pattern`, such as `x` for `wait:x` under
/// `wait:{lens}`.
fn lens_in<'a>(pattern: &str, label: &'a str) -> Option<&'a str> {
    let (before, after) = pattern.split_once("{lens}")?;
    let rest = label.strip_prefix(before)?.strip_suffix(after)?;
    (!rest.is_empty()).then_some(rest)
}

/// A name with only its letters and digits, lowercased.
fn plain(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut last = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (row[j + 1] + 1)
                .min(row[j] + 1)
                .min(last + usize::from(ca != *cb));
            last = here;
        }
    }
    row[b.len()]
}

/// Whether two names are spelt near enough to be one concern: the same
/// once plain, one inside the other, or a couple of letters apart.
fn near(a: &str, b: &str) -> bool {
    let (a, b) = (plain(a), plain(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b
        || (a.len().min(b.len()) >= 4 && (a.contains(&b) || b.contains(&a)))
        || (a.len().min(b.len()) >= 5 && distance(&a, &b) <= 2)
}

/// The repository's labels spelt close to `lens`, by the lens each names
/// under the config's patterns, or else by the whole label.
pub fn closest(lens: &str, names: Option<&Labels>, repo: &[RepoLabel]) -> Vec<String> {
    let patterns: Vec<&str> = names
        .map(|n| {
            [&n.waiting, &n.confirmed]
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();
    repo.iter()
        .filter(|l| {
            let named = patterns
                .iter()
                .find_map(|p| lens_in(p, &l.name))
                .unwrap_or(&l.name);
            named != lens && near(named, lens)
        })
        .map(|l| l.name.clone())
        .take(6)
        .collect()
}

/// What a calibration's config does with the repository's labels, for the
/// person who reads it before accepting.
pub fn summary(cfg: &Config, repo: &[RepoLabel], classes: &[Class]) -> String {
    let names = cfg.integrations.labels.as_ref();
    let mut out = String::from("## The repository's labels\n\nChecked by acrbus.\n\n");
    match names {
        Some(names) => out.push_str(&creates(cfg, names, repo)),
        None => out.push_str(
            "- The config sets no `integrations.labels`, so reviews won't label changes.\n",
        ),
    }
    out.push_str(&classified(names, classes));
    out
}

/// The lenses whose labels the repository has, and the labels accepting
/// would create, each with what it's spelt close to.
fn creates(cfg: &Config, names: &Labels, repo: &[RepoLabel]) -> String {
    let have: BTreeSet<&str> = repo.iter().map(|l| l.name.as_str()).collect();
    let quoted = |names: &[&String]| {
        names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let per_lens = Labels {
        ready: None,
        not_ready: None,
        ..names.clone()
    };
    let (mut kept, mut new) = (Vec::new(), String::new());
    for lens in &cfg.lenses {
        let wants = wanted(&per_lens, &[lens.id.as_str()]);
        let missing: Vec<&String> = wants
            .iter()
            .filter(|w| !have.contains(w.as_str()))
            .collect();
        if missing.is_empty() {
            kept.push(lens.id.as_str());
            continue;
        }
        new.push_str(&format!(
            "- New: lens {}. Accepting creates {}.",
            lens.id,
            quoted(&missing)
        ));
        let close = closest(&lens.id, Some(names), repo);
        if !close.is_empty() {
            new.push_str(&format!(
                " Spelt close to {}: if it's the same concern, rename the lens to match before \
                 you accept.",
                quoted(&close.iter().collect::<Vec<_>>())
            ));
        }
        new.push('\n');
    }
    let mut out = String::new();
    if !kept.is_empty() {
        out.push_str(&format!(
            "- Lenses whose labels the repository has: {}.\n",
            kept.join(", ")
        ));
    }
    out.push_str(&new);
    for overall in names.ready.iter().chain(&names.not_ready) {
        if !have.contains(overall.as_str()) {
            out.push_str(&format!("- Accepting creates `{overall}`.\n"));
        }
    }
    out
}

/// What the calibration said of the labels: concerns it left out, labels
/// that give a lens another name, and how many aren't review concerns.
fn classified(names: Option<&Labels>, classes: &[Class]) -> String {
    let patterns: Vec<&str> = names
        .map(|n| {
            [&n.waiting, &n.confirmed]
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();
    let mut out = String::new();
    let mut other = 0;
    for c in classes {
        match (c.kind, &c.lens, c.relevant) {
            (Kind::Other, ..) => other += 1,
            (Kind::Dimension, Some(lens), Some(true)) => {
                let named = patterns.iter().find_map(|p| lens_in(p, &c.label));
                if named.is_some_and(|n| n != lens) {
                    out.push_str(&format!(
                        "- `{}` names lens {lens} by another name. Two names for one concern \
                         split its history: keep one, and relabel by hand.\n",
                        c.label
                    ));
                }
            }
            (Kind::Dimension, Some(lens), Some(false)) => out.push_str(&format!(
                "- Not relevant here now, so left out (the label stays): `{}`, lens {lens}. {}\n",
                c.label,
                c.why.trim()
            )),
            _ => {}
        }
    }
    if other > 0 {
        out.push_str(&format!("- Not review concerns: {other} label(s).\n"));
    }
    out
}

/// Creates the labels an accepted config could put on a change that the
/// repository lacks, and gives their names.
pub fn create_missing(repo: &Path, cfg: &Config) -> Result<Vec<String>> {
    let Some(names) = &cfg.integrations.labels else {
        return Ok(Vec::new());
    };
    let have: BTreeSet<String> = read(repo)?.into_iter().map(|l| l.name).collect();
    let lenses: Vec<&str> = cfg.lenses.iter().map(|l| l.id.as_str()).collect();
    let mut made = Vec::new();
    for (name, description, color) in to_create(names, &lenses, &have) {
        let out = Command::new("gh")
            .args(["label", "create", &name, "--description", &description])
            .args(["--color", color])
            .current_dir(repo)
            .output()
            .context("running gh")?;
        if !out.status.success() {
            bail!(
                "gh label create {name}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        made.push(name);
    }
    Ok(made)
}

/// The labels to create, each with its description and colour, the way
/// acrbus has always described them.
fn to_create(
    names: &Labels,
    lenses: &[&str],
    have: &BTreeSet<String>,
) -> Vec<(String, String, &'static str)> {
    let mut out = Vec::new();
    let mut add = |name: String, description: String, color: &'static str| {
        if !have.contains(&name) && !out.iter().any(|(n, _, _)| n == &name) {
            let description: String = description.chars().take(100).collect();
            out.push((name, description, color));
        }
    };
    if let Some(n) = &names.ready {
        add(
            n.clone(),
            "ACR: every lens confirmed or waived".into(),
            "0e8a16",
        );
    }
    if let Some(n) = &names.not_ready {
        add(
            n.clone(),
            "ACR: a review ended with lenses open".into(),
            "d93f0b",
        );
    }
    for lens in lenses {
        if let Some(p) = &names.waiting {
            add(
                p.replace("{lens}", lens),
                format!("ACR: the {lens} lens is still open"),
                "fbca04",
            );
        }
        if let Some(p) = &names.confirmed {
            add(
                p.replace("{lens}", lens),
                format!("ACR: the {lens} lens's latest round found no blocker"),
                "0e8a16",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acr::config;

    fn label(name: &str) -> RepoLabel {
        RepoLabel {
            name: name.to_string(),
            description: String::new(),
        }
    }

    fn cfg(lenses: &[&str]) -> Config {
        let mut text = String::from(
            "version: 1\n\
             tiers:\n  - {id: all, match: {paths: [\"**\"], description: \"any change\"}, fan_out: 1}\n\
             integrations:\n  labels: {not_ready: not-ready, ready: ready, waiting: \"wait:{lens}\", confirmed: \"confirmed:{lens}\"}\n\
             lenses:\n",
        );
        for l in lenses {
            text.push_str(&format!(
                "  - {{id: {l}, criteria: \"wrong\", blocker: \"wrong; not style\", triggers: {{paths: [\"**\"]}}}}\n"
            ));
        }
        let user = config::parse_user(
            "version: 1\nallowed_bindings:\n  strong: {agent: \"a@here\", model: strong, tools: [Read]}\n\
             defaults:\n  bindings: {sizer: strong, orchestrator: strong, reviewer: strong, judge: strong}\n",
        )
        .unwrap();
        config::resolve(
            &user,
            config::parse_repo(&text).unwrap(),
            &config::Flags::default(),
        )
        .unwrap()
    }

    fn dim(label: &str, lens: &str, relevant: bool) -> Class {
        Class {
            label: label.to_string(),
            kind: Kind::Dimension,
            lens: Some(lens.to_string()),
            relevant: Some(relevant),
            why: "because".to_string(),
        }
    }

    fn other(label: &str) -> Class {
        Class {
            label: label.to_string(),
            kind: Kind::Other,
            lens: None,
            relevant: None,
            why: String::new(),
        }
    }

    #[test]
    fn the_brief_lists_every_label_and_asks_for_each_to_be_classified() {
        let b = brief(&[
            RepoLabel {
                name: "wait:correctness".into(),
                description: "ACR: the correctness lens is still open".into(),
            },
            label("epic"),
        ]);
        assert!(
            b.contains("- `wait:correctness`: ACR: the correctness lens"),
            "{b}"
        );
        assert!(b.contains("- `epic`\n"), "{b}");
        assert!(b.contains("```labels block"), "{b}");
        assert!(b.contains("whatever its pattern"), "{b}");
        assert!(brief(&[]).contains("It has none yet"));
    }

    #[test]
    fn a_classification_is_read_from_yaml() {
        let c = parse(
            "- {label: \"wait:perf\", kind: dimension, lens: perf, relevant: true, why: hot loops}\n\
             - {label: epic, kind: other}\n",
        )
        .unwrap();
        assert_eq!(c[0], dim("wait:perf", "perf", true).with_why("hot loops"));
        assert_eq!(c[1], other("epic"));
        assert!(parse("- {label: x, kind: maybe}").is_err());
    }

    impl Class {
        fn with_why(mut self, why: &str) -> Class {
            self.why = why.to_string();
            self
        }
    }

    #[test]
    fn every_label_must_be_classified_once_and_none_made_up() {
        let c = cfg(&["correctness"]);
        let repo = [
            label("wait:correctness"),
            label("confirmed:correctness"),
            label("epic"),
        ];
        let good = [
            dim("wait:correctness", "correctness", true),
            dim("confirmed:correctness", "correctness", true),
            other("epic"),
        ];
        check(&c, &repo, Some(&good)).unwrap();

        let e = check(&c, &repo, Some(&good[..2])).unwrap_err().to_string();
        assert!(e.contains("not classified: epic"), "{e}");

        let mut made_up = good.to_vec();
        made_up.push(other("size/L"));
        let e = check(&c, &repo, Some(&made_up)).unwrap_err().to_string();
        assert!(
            e.contains("size/L isn't one of the repository's labels"),
            "{e}"
        );

        let mut twice = good.to_vec();
        twice.push(other("epic"));
        let e = check(&c, &repo, Some(&twice)).unwrap_err().to_string();
        assert!(e.contains("epic is classified twice"), "{e}");

        let e = check(&c, &repo, None).unwrap_err().to_string();
        assert!(e.contains("no ```labels block"), "{e}");
        check(&c, &[], None).unwrap();
    }

    #[test]
    fn a_relevant_concern_keeps_its_name_as_a_lens() {
        // The labels call it correctness; a config that renames it fails.
        let c = cfg(&["logic"]);
        let repo = [label("wait:correctness")];
        let e = check(
            &c,
            &repo,
            Some(&[dim("wait:correctness", "correctness", true)]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("the config has no lens correctness"), "{e}");
        // One that isn't relevant now may be left out.
        check(
            &c,
            &repo,
            Some(&[dim("wait:correctness", "correctness", false)]),
        )
        .unwrap();
        // A dimension without its lens or relevance doesn't hold.
        let mut bare = dim("wait:correctness", "correctness", true);
        bare.relevant = None;
        let e = check(&c, &repo, Some(&[bare])).unwrap_err().to_string();
        assert!(e.contains("needs `lens` and `relevant`"), "{e}");
    }

    #[test]
    fn spellings_close_to_a_new_lens_are_found() {
        let names = cfg(&["correctness"]).integrations.labels.unwrap();
        let repo = [
            label("wait:performance"),
            label("confirmed:correctness"),
            label("review:security"),
            label("epic"),
        ];
        assert_eq!(
            closest("perf", Some(&names), &repo),
            vec!["wait:performance"]
        );
        assert_eq!(
            closest("correctnes", Some(&names), &repo),
            vec!["confirmed:correctness"]
        );
        // Another pattern is compared whole.
        assert_eq!(
            closest("security", Some(&names), &repo),
            vec!["review:security"]
        );
        assert!(closest("delivery", Some(&names), &repo).is_empty());
        // A lens's own labels aren't close to it: they're it.
        assert!(closest("correctness", Some(&names), &repo).is_empty());
    }

    #[test]
    fn the_summary_says_what_accepting_creates_and_what_splits_a_concern() {
        let c = cfg(&["correctness", "perf"]);
        let repo = [
            label("wait:correctness"),
            label("confirmed:correctness"),
            label("wait:performance"),
            label("ready"),
            label("epic"),
        ];
        let classes = [
            dim("wait:correctness", "correctness", true),
            dim("confirmed:correctness", "correctness", true),
            dim("wait:performance", "perf", true),
            dim("ready", "readiness", false),
            other("epic"),
        ];
        let s = summary(&c, &repo, &classes);
        assert!(
            s.contains("Lenses whose labels the repository has: correctness"),
            "{s}"
        );
        assert!(
            s.contains("- New: lens perf. Accepting creates `wait:perf`, `confirmed:perf`."),
            "{s}"
        );
        assert!(s.contains("Spelt close to `wait:performance`"), "{s}");
        assert!(
            s.contains("`wait:performance` names lens perf by another name"),
            "{s}"
        );
        assert!(s.contains("Accepting creates `not-ready`"), "{s}");
        assert!(!s.contains("Accepting creates `ready`"), "{s}");
        assert!(s.contains("Not review concerns: 1 label(s)"), "{s}");
        // elegance comes with every config, so its labels are new here too.
        assert!(s.contains("- New: lens elegance."), "{s}");
    }

    #[test]
    fn accepting_creates_only_what_the_repository_lacks() {
        let names = cfg(&["correctness"]).integrations.labels.unwrap();
        let have: BTreeSet<String> = ["ready", "wait:correctness"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let made = to_create(&names, &["correctness", "perf"], &have);
        let made: Vec<(&str, &str)> = made
            .iter()
            .map(|(n, d, _)| (n.as_str(), d.as_str()))
            .collect();
        assert_eq!(
            made,
            vec![
                ("not-ready", "ACR: a review ended with lenses open"),
                (
                    "confirmed:correctness",
                    "ACR: the correctness lens's latest round found no blocker"
                ),
                ("wait:perf", "ACR: the perf lens is still open"),
                (
                    "confirmed:perf",
                    "ACR: the perf lens's latest round found no blocker"
                ),
            ]
        );
        assert_eq!(
            wanted(&names, &["perf"]),
            vec!["ready", "not-ready", "wait:perf", "confirmed:perf"]
        );
    }
}
