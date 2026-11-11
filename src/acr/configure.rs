//! A repository's review, configured from what's in it and recalibrated from
//! what its user says of its reviews.
//!
//! `acr configure` has the configurer, a strong model in a clone of the
//! repository of its own, review the whole repository as it is now: what it's
//! built of, how it's tested and shipped, what it keeps and what it talks to,
//! and which kinds of defect its history keeps fixing. From that it writes
//! the repository's review config: the dimensions that matter here, what
//! blocks in each and how to attack it, and how to work and test in it. A
//! person reads the proposal and accepts it, and that's the repository's
//! calibration.
//!
//! `acr feedback` records what the person says of a finished review's
//! findings, and what it missed. `acr calibrate` gives the configurer the
//! config, that feedback and the repository again, and it proposes a revised
//! config the same way.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::config::{self, Binding, Role, UserConfig};
use super::flow::{Call, Caller, Run};
use crate::config::APP_NAME;

/// How to set a repository up, which the configurer follows.
const GUIDE: &str = include_str!("../../docs/acr-repo-setup.md");
/// A config to go by for its form.
const EXAMPLE: &str = include_str!("../../examples/acr/rust.yaml");

/// How long a finished review's findings are kept, for feedback on them.
const FINDINGS_DAYS: u64 = 90;
/// The most feedback kept for a repository: its latest.
const FEEDBACK_KEPT: usize = 2000;
/// The backups of a repository's config that accepting a proposal keeps.
const BACKUPS_KEPT: usize = 3;

// ------------------------------------------------------------- the configurer

/// The configurer's binding: its own, or else the Judge's, which is also a
/// strong model.
pub fn binding(user: &UserConfig) -> Result<(String, Binding)> {
    let bindings = user.defaults.as_ref().map(|d| &d.bindings);
    let name = bindings
        .and_then(|b| b.get(&Role::Configurer).or_else(|| b.get(&Role::Judge)))
        .with_context(|| {
            format!(
                "no binding for the configurer: give one in your acr.yaml's defaults.bindings, \
                 as `configurer`, or run `{APP_NAME} setup acr`"
            )
        })?;
    let binding = user
        .allowed_bindings
        .get(name)
        .with_context(|| format!("binding {name} isn't in your allowed_bindings"))?;
    Ok((name.clone(), binding.clone()))
}

/// What the configurer proposes: the config, decided non-issues if the
/// feedback shows some, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    pub config: String,
    pub not_findings: Option<String>,
    pub why: String,
}

/// The configurer's brief. With `current`, it's a recalibration: the config
/// as it is, and what the user said of its reviews since.
pub fn prompt(name: &str, current: Option<&str>, feedback: &[Feedback]) -> String {
    let mut out = format!(
        "You configure adversarial convergence review (ACR) for the repository {name}. Your \
         working directory is a clone of it of your own, as it is now: read all of it you \
         need, run what you need to there, and look at its history with git. Nothing you do \
         in it reaches the repository. Never copy a secret's value into what you write.\n\n\
         ACR reviews each change to this repository with fresh reviewers, one dimension (a \
         lens) each, and a Judge who rules what blocks. The config you write decides what \
         they look for and what stops a change. Make it right for this repository as it is, \
         not for repositories in general.\n\n\
         # Review the repository, exhaustively\n\n\
         - What it is and what it's for: its README and docs, and what it serves.\n\
         - What it's built of: its languages, frameworks and dependencies, generated code, \
         and how it's laid out.\n\
         - How it's tested: its suites, how they run, how long they take, and what they need. \
         Find the command that runs the tests for a given set of files, and run it in your \
         clone to be sure it works there, offline.\n\
         - How it ships: its CI, release and deploy configuration, and what it versions.\n\
         - What it keeps and what it talks to: data stores and migrations, files, external \
         services and APIs, authentication, and how secrets reach the code.\n\
         - Its history: `git log` for the kinds of defect it keeps fixing, its reverts, and \
         what its commit messages say went wrong.\n\n\
         # Decide what review needs here\n\n\
         - The dimensions that matter here, each a lens: `criteria`, what's wrong in it, said \
         for this repository; `attack`, how a reviewer finds that here, by what to run, break \
         or compare; `blocker`, the defects in it that must not merge, ending with what \
         doesn't block, such as style; and `triggers`, the paths and content it applies to. \
         A few sharp lenses beat many vague ones. {APP_NAME} gives every repository an \
         `elegance` lens itself: leave it out unless you need to word it for this one.\n\
         - `tiers`: which changes are small here and which are critical, and how many \
         reviewers each gets.\n\
         - `runbook`: how a reviewer works in a fresh copy on this machine: setup, commands, \
         and what never to touch.\n\
         - `tests`: `setup`, `command` (with `{{tests}}` for the test files), and `coverage` \
         and `report` where the tools allow, with `stages` to match.\n\
         - `always_escalate`: changes a person must see whatever the review says.\n\n"
    );
    if let Some(current) = current {
        out.push_str(&format!(
            "# Its config now\n\n```yaml\n{}\n```\n\n",
            current.trim()
        ));
    }
    if !feedback.is_empty() {
        out.push_str(
            "# What its user said of its reviews\n\n\
             Revise the config so its reviews get these right next time. Where the user says \
             something isn't a defect, or shouldn't block, tighten that lens's criteria or \
             blocker, or record it as a decided non-issue. Where a real defect was missed, or \
             should have blocked, widen the criteria, the blocker or the attack. Add, merge or \
             retire lenses where that's what it takes, and keep what works.\n\n",
        );
        for f in feedback {
            out.push_str(&format!("- {}\n", f.described()));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "# How to set a repository up\n\n{}\n\n# The schema\n\n```json\n{}\n```\n\n\
         # A config, for its form\n\n```yaml\n{}\n```\n\n\
         # Answer\n\n\
         First the whole config, in one ```yaml block. If you record decided non-issues, \
         give them in one ```not-findings block, a line each, `- <what> — <why>`. Then \
         `## Why`: for each lens, why it matters here and why its blocker is drawn where it \
         is; what you ran to check the test commands, and what it showed; and what you're \
         unsure of.",
        GUIDE.trim(),
        config::SCHEMA.trim(),
        EXAMPLE.trim()
    ));
    out
}

/// The last fenced block of `kind` in `text`.
fn block(text: &str, kind: &str) -> Option<String> {
    let open = format!("```{kind}\n");
    let start = text.rfind(&open)? + open.len();
    let end = text[start..].find("\n```")?;
    Some(text[start..start + end].to_string())
}

pub fn read_proposal(answer: &str) -> Result<Proposal> {
    let config = block(answer, "yaml")
        .or_else(|| block(answer, "yml"))
        .context("the configurer's answer has no ```yaml block")?;
    let not_findings = block(answer, "not-findings").filter(|t| !t.trim().is_empty());
    let why = answer
        .rfind("## Why")
        .map(|i| answer[i..].trim().to_string())
        .unwrap_or_default();
    Ok(Proposal {
        config,
        not_findings,
        why,
    })
}

/// Holds a proposed config to the rules a repository's config is held to.
pub fn check(user: &UserConfig, text: &str) -> Result<()> {
    let policy = config::parse_repo(text).context("reading it")?;
    config::resolve(user, policy, &config::Flags::default())?;
    Ok(())
}

/// One configuring: the configurer reviews the clone and proposes a config.
/// A config that doesn't hold gets one more call, to fix just that, not to
/// review again.
pub fn propose(
    caller: &dyn Caller,
    user: &UserConfig,
    (name, binding): (&str, &Binding),
    clone: &Path,
    brief: &str,
) -> Result<Proposal> {
    let call = |prompt: &str| {
        caller.call(&Call {
            role: Role::Configurer,
            binding_name: name,
            binding,
            prompt,
            artifact: None,
            // It fixes nothing: what it writes, running the tests, stays in
            // its own clone, which goes when it's done.
            writes: false,
            repo: clone,
        })
    };
    let answer = call(brief)?;
    let first = read_proposal(&answer.text).and_then(|p| check(user, &p.config).map(|_| p));
    match first {
        Ok(p) => Ok(p),
        Err(e) => {
            let fix = format!(
                "You were configuring ACR for this repository, and answered this:\n\n{}\n\n\
                 It doesn't hold: {e:#}\n\nFix only that. Don't review the repository again. \
                 Answer the same way: the whole config in one ```yaml block, any decided \
                 non-issues in a ```not-findings block, then `## Why`.\n\n\
                 # The schema\n\n```json\n{}\n```",
                answer.text.trim(),
                config::SCHEMA.trim()
            );
            let answer = call(&fix)?;
            let p = read_proposal(&answer.text)?;
            check(user, &p.config)
                .context("the configurer's config doesn't hold, even after a try to fix it")?;
            Ok(p)
        }
    }
}

// --------------------------------------------------------------- proposals

/// Where a repository's proposed config, its decided non-issues and its
/// reasons wait for a person, beside its config.
pub struct Pending {
    pub config: PathBuf,
    pub not_findings: PathBuf,
    pub why: PathBuf,
    /// When the latest feedback a recalibration folded in was given.
    pub folds: PathBuf,
}

pub fn pending(config: &Path) -> Pending {
    let with = |ext: &str| config.with_extension(ext);
    Pending {
        config: with("proposed.yaml"),
        not_findings: with("proposed.not-findings.md"),
        why: with("proposed.md"),
        folds: with("proposed.folds"),
    }
}

/// Saves a proposal to wait for a person, replacing any earlier one.
/// `folds` is the time of the latest feedback it folds in, from a
/// recalibration.
pub fn save(config: &Path, p: &Proposal, folds: Option<i64>) -> Result<Pending> {
    let at = pending(config);
    if let Some(dir) = config.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&at.config, format!("{}\n", p.config.trim_end()))?;
    fs::write(&at.why, format!("{}\n", p.why.trim_end()))?;
    match &p.not_findings {
        Some(text) => fs::write(&at.not_findings, format!("{}\n", text.trim_end()))?,
        None => {
            let _ = fs::remove_file(&at.not_findings);
        }
    }
    match folds {
        Some(t) => fs::write(&at.folds, t.to_string())?,
        None => {
            let _ = fs::remove_file(&at.folds);
        }
    }
    Ok(at)
}

/// What putting a proposal in place did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The latest feedback it folded in, from a recalibration.
    pub folded: Option<i64>,
}

/// Puts a waiting proposal in place of the config, keeping the config it
/// replaces as a backup, and adding its decided non-issues to the
/// repository's. None if nothing was waiting.
pub fn apply(config: &Path, at: i64) -> Result<Option<Applied>> {
    let p = pending(config);
    if !p.config.exists() {
        return Ok(None);
    }
    let folded = fs::read_to_string(&p.folds)
        .ok()
        .and_then(|t| t.trim().parse().ok());
    let mut text = fs::read_to_string(&p.config)?;
    if let Ok(added) = fs::read_to_string(&p.not_findings) {
        let file = config.with_extension("not-findings.md");
        let mut out = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .with_context(|| format!("adding to {}", file.display()))?;
        writeln!(out, "{}", added.trim_end())?;
        let named = config::parse_repo(&text)?.not_findings.is_some();
        if !named {
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            text = format!("{}\nnot_findings: {name}\n", text.trim_end());
        }
    }
    if config.exists() {
        let backup = config.with_extension(format!("yaml.{at}.bak"));
        fs::copy(config, &backup)?;
        prune_backups(config)?;
    }
    fs::write(config, text)?;
    for path in [&p.config, &p.not_findings, &p.why, &p.folds] {
        let _ = fs::remove_file(path);
    }
    Ok(Some(Applied { folded }))
}

fn prune_backups(config: &Path) -> Result<()> {
    let (Some(dir), Some(stem)) = (config.parent(), config.file_name()) else {
        return Ok(());
    };
    let prefix = format!("{}.", stem.to_string_lossy());
    let mut backups: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            n.starts_with(&prefix) && n.ends_with(".bak")
        })
        .collect();
    backups.sort();
    let over = backups.len().saturating_sub(BACKUPS_KEPT);
    for old in &backups[..over] {
        let _ = fs::remove_file(old);
    }
    Ok(())
}

// ---------------------------------------------------------------- findings

/// A finding of a finished review, kept for feedback on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub id: String,
    pub lens: String,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    pub severity: String,
    /// It blocked the change.
    pub blocked: bool,
    /// Where it ended up: open, fixed, waived, dropped and so on.
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeptRun {
    pub run: String,
    /// The repository, by the name its config goes by.
    pub repo: String,
    pub at: i64,
    pub findings: Vec<Kept>,
}

fn findings_dir(state: &Path) -> PathBuf {
    state.join("acr-findings")
}

/// Keeps a finished review's findings for feedback, the latest of each, and
/// clears ones older than `FINDINGS_DAYS`.
pub fn keep_findings(state: &Path, run: &Run, at: i64) -> Result<()> {
    keep(
        state,
        &run.id,
        &super::change::name(&run.repo)?,
        &run.lenses,
        at,
    )
}

fn keep(
    state: &Path,
    run: &str,
    repo: &str,
    lenses: &std::collections::BTreeMap<String, super::flow::LensResult>,
    at: i64,
) -> Result<()> {
    let mut findings: Vec<Kept> = Vec::new();
    for (lens, result) in lenses {
        for round in &result.rounds {
            for f in &round.findings {
                let kept = Kept {
                    id: f.id.clone(),
                    lens: lens.clone(),
                    title: f.title.clone(),
                    detail: f.detail.clone(),
                    file: f.file.clone(),
                    line: f.line,
                    severity: format!("{:?}", f.severity),
                    blocked: f.blocks(),
                    state: format!("{:?}", f.state).to_lowercase(),
                };
                match findings.iter_mut().find(|k| k.id == kept.id) {
                    Some(old) => *old = kept,
                    None => findings.push(kept),
                }
            }
        }
    }
    let dir = findings_dir(state);
    fs::create_dir_all(&dir)?;
    prune(&dir, Duration::from_secs(FINDINGS_DAYS * 86_400));
    let record = KeptRun {
        run: run.to_string(),
        repo: repo.to_string(),
        at,
        findings,
    };
    fs::write(
        dir.join(format!("{run}.json")),
        serde_json::to_string_pretty(&record)?,
    )?;
    Ok(())
}

pub fn kept(state: &Path, run: &str) -> Result<KeptRun> {
    let path = findings_dir(state).join(format!("{run}.json"));
    let text = fs::read_to_string(&path).with_context(|| {
        format!(
            "no findings are kept for review {run}: a finished review's are, for \
             {FINDINGS_DAYS} days"
        )
    })?;
    serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

fn prune(dir: &Path, age: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d > age);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------- feedback

/// What the user says of a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Right, and ruled right.
    Right,
    /// Not a defect at all.
    NotADefect,
    /// A real defect, but it shouldn't have blocked.
    NotABlocker,
    /// A real defect that should have blocked.
    ShouldBlock,
}

impl Verdict {
    fn said(self) -> &'static str {
        match self {
            Verdict::Right => "right",
            Verdict::NotADefect => "not a defect",
            Verdict::NotABlocker => "a real defect, but it shouldn't have blocked",
            Verdict::ShouldBlock => "a real defect that should have blocked",
        }
    }
}

/// One thing the user said of a review: of a finding, or of what it missed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub at: i64,
    pub run: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finding: Option<Kept>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// A defect the review missed, in the user's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed: Option<String>,
    /// The lens it belongs to, for a missed defect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Feedback {
    /// As the configurer reads it.
    pub fn described(&self) -> String {
        let note = self
            .note
            .as_deref()
            .map(|n| format!(" They added: {n}"))
            .unwrap_or_default();
        match (&self.finding, self.verdict, &self.missed) {
            (Some(f), Some(v), _) => format!(
                "Review {}, lens {}: \"{}\" ({}, {}{}) — the user says: {}.{note}",
                self.run,
                f.lens,
                f.title,
                f.severity,
                if f.blocked { "blocked" } else { "logged" },
                f.file
                    .as_deref()
                    .map(|p| format!(", {p}"))
                    .unwrap_or_default(),
                v.said()
            ),
            (_, _, Some(m)) => format!(
                "Review {}{}: it missed this, the user says: {m}.{note}",
                self.run,
                self.lens
                    .as_deref()
                    .map(|l| format!(", lens {l}"))
                    .unwrap_or_default()
            ),
            _ => format!(
                "Review {}: {}",
                self.run,
                self.note.as_deref().unwrap_or("")
            ),
        }
    }
}

fn feedback_path(state: &Path, repo: &str) -> PathBuf {
    state.join("acr-feedback").join(format!("{repo}.jsonl"))
}

/// Records feedback on a repository's review, keeping its latest
/// `FEEDBACK_KEPT`.
pub fn add_feedback(state: &Path, repo: &str, f: &Feedback) -> Result<()> {
    let path = feedback_path(state, repo);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut lines: Vec<String> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.push(serde_json::to_string(f)?);
    let over = lines.len().saturating_sub(FEEDBACK_KEPT);
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, lines[over..].join("\n") + "\n")?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// A repository's feedback, after `since` if given: what a recalibration
/// hasn't folded in yet.
pub fn feedback(state: &Path, repo: &str, since: Option<i64>) -> Result<Vec<Feedback>> {
    let text = match fs::read_to_string(feedback_path(state, repo)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Feedback = serde_json::from_str(line).context("reading feedback")?;
        if since.is_none_or(|s| f.at > s) {
            out.push(f);
        }
    }
    Ok(out)
}

/// A finding of a kept review, by its place in the list (from 1) or its ID.
pub fn finding<'a>(run: &'a KeptRun, which: &str) -> Result<&'a Kept> {
    if let Ok(n) = which.parse::<usize>()
        && (1..=run.findings.len()).contains(&n)
    {
        return Ok(&run.findings[n - 1]);
    }
    match run.findings.iter().find(|f| f.id == which) {
        Some(f) => Ok(f),
        None => bail!(
            "review {} has no finding {which}: give its number in the list, or its ID",
            run.run
        ),
    }
}

/// A kept review's findings, numbered, for feedback on them.
pub fn listed(run: &KeptRun) -> String {
    let mut out = format!(
        "Review {} of {}: {} finding(s).\n",
        run.run,
        run.repo,
        run.findings.len()
    );
    for (i, f) in run.findings.iter().enumerate() {
        out.push_str(&format!(
            "{:>3}. [{}] {} ({}, {}, {}; {})\n",
            i + 1,
            f.lens,
            f.title,
            f.severity,
            if f.blocked { "blocked" } else { "logged" },
            f.state,
            f.id
        ));
    }
    out.push_str(&format!(
        "\nSay how each did: `{APP_NAME} acr feedback {} --finding <number> --verdict \
         right|not-a-defect|not-a-blocker|should-block [--note <text>]`, and what it missed: \
         `{APP_NAME} acr feedback {} --missed <text> [--lens <id>]`.",
        run.run, run.run
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use crate::acr::config::{Action, Severity};
    use crate::acr::flow::{Answer, Finding, FindingState, LensResult, Outcome, Round};

    /// A config that holds, with no non-issues file of its own.
    const GOOD: &str = "version: 1\n\
                        tiers:\n  - {id: all, match: {paths: [\"**\"], description: \"any change\"}, fan_out: 1}\n\
                        lenses:\n  - {id: correctness, criteria: \"wrong results\", blocker: \"a wrong result; not style\", triggers: {paths: [\"**\"]}}\n";

    fn user(configurer: bool) -> UserConfig {
        let extra = if configurer {
            "  deep: {agent: \"a@here\", model: deep, effort: max, tools: [Read, Grep, Glob, Bash, Write]}\n"
        } else {
            ""
        };
        let bind = if configurer { ", configurer: deep" } else { "" };
        config::parse_user(&format!(
            "version: 1\nallowed_bindings:\n  strong: {{agent: \"a@here\", model: strong, tools: [Read, Grep, Glob, Bash, Write]}}\n{extra}\
             defaults:\n  bindings: {{sizer: strong, orchestrator: strong, reviewer: strong, judge: strong{bind}}}\n"
        ))
        .unwrap()
    }

    /// Answers with each of `answers` in turn, and keeps what it was asked.
    struct Fake {
        answers: Mutex<Vec<String>>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(answers: &[&str]) -> Fake {
            Fake {
                answers: Mutex::new(answers.iter().rev().map(|a| a.to_string()).collect()),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl Caller for Fake {
        fn call(&self, call: &Call) -> Result<Answer> {
            assert_eq!(call.role, Role::Configurer);
            assert!(!call.writes, "the configurer fixes nothing");
            self.asked.lock().unwrap().push(call.prompt.to_string());
            let text = self.answers.lock().unwrap().pop().expect("an answer");
            Ok(Answer {
                text,
                run_id: "r".into(),
                usage: None,
                via: None,
            })
        }
    }

    fn answer(config: &str) -> String {
        format!(
            "I read it all.\n\n```yaml\n{}\n```\n\n```not-findings\n- logging the request id — it's not a secret here\n```\n\n## Why\n\nBecause.",
            config.trim()
        )
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-configure-{}", crate::util::new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_configurer_has_its_own_binding_or_the_judges() {
        let (name, b) = binding(&user(true)).unwrap();
        assert_eq!((name.as_str(), b.effort.as_deref()), ("deep", Some("max")));
        let (name, _) = binding(&user(false)).unwrap();
        assert_eq!(name, "strong");
        let none = config::parse_user("version: 1\n").unwrap();
        assert!(binding(&none).is_err());
    }

    #[test]
    fn its_brief_reviews_the_repository_and_a_recalibration_carries_the_feedback() {
        let fresh = prompt("shop", None, &[]);
        assert!(fresh.contains("for the repository shop"));
        assert!(fresh.contains("Review the repository, exhaustively"));
        assert!(fresh.contains("`git log`") && fresh.contains("`{tests}`"));
        assert!(fresh.contains("`elegance` lens itself"));
        assert!(fresh.contains("# How to set a repository up") && fresh.contains("\"$schema\""));
        assert!(!fresh.contains("# Its config now") && !fresh.contains("What its user said"));
        let said = Feedback {
            at: 5,
            run: "R1".into(),
            finding: Some(kept_finding("F1", true)),
            verdict: Some(Verdict::NotABlocker),
            missed: None,
            lens: None,
            note: Some("house style".into()),
        };
        let again = prompt("shop", Some("version: 1"), &[said]);
        assert!(again.contains("# Its config now\n\n```yaml\nversion: 1\n```"));
        assert!(
            again.contains("- Review R1, lens correctness: \"F1 title\" (High, blocked, src/a.rs) — the user says: a real defect, but it shouldn't have blocked. They added: house style"),
            "{again}"
        );
    }

    #[test]
    fn a_proposal_is_read_from_its_blocks() {
        let p = read_proposal(&answer("version: 1")).unwrap();
        assert_eq!(p.config, "version: 1");
        assert_eq!(
            p.not_findings.as_deref(),
            Some("- logging the request id — it's not a secret here")
        );
        assert!(p.why.starts_with("## Why"));
        assert!(read_proposal("no config here").is_err());
    }

    #[test]
    fn a_config_that_doesnt_hold_gets_one_call_to_fix_it() {
        let u = user(false);
        let (name, b) = binding(&u).unwrap();
        let bad = answer(
            "version: 1\nlenses: [{id: Not An Id, criteria: x, triggers: {paths: [\"**\"]}}]",
        );
        let good = answer(GOOD);
        let fake = Fake::new(&[&bad, &good]);
        let p = propose(&fake, &u, (&name, &b), Path::new("/clone"), "review it").unwrap();
        assert_eq!(p.config.trim(), GOOD.trim());
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert!(asked[1].contains("It doesn't hold") && asked[1].contains("Don't review"));
        // Twice wrong is an error, not a bad config.
        let fake = Fake::new(&[&bad, &bad]);
        let e = propose(&fake, &u, (&name, &b), Path::new("/clone"), "review it").unwrap_err();
        assert!(format!("{e:#}").contains("even after a try"), "{e:#}");
        // A good first answer needs no second call.
        let fake = Fake::new(&[&good]);
        propose(&fake, &u, (&name, &b), Path::new("/clone"), "review it").unwrap();
        assert_eq!(fake.asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_proposal_waits_then_goes_in_place_keeping_backups_and_non_issues() {
        let dir = scratch();
        let config = dir.join("shop.yaml");
        fs::write(&config, "version: 1\n# the old one\n").unwrap();
        let p = read_proposal(&answer(GOOD)).unwrap();
        let waiting = save(&config, &p, Some(77)).unwrap();
        assert!(waiting.config.exists() && waiting.why.exists() && waiting.not_findings.exists());
        let applied = apply(&config, 1000).unwrap().unwrap();
        assert_eq!(applied.folded, Some(77));
        let now = fs::read_to_string(&config).unwrap();
        assert!(now.starts_with(GOOD.trim_end()));
        assert!(
            now.trim_end()
                .ends_with("not_findings: shop.not-findings.md"),
            "{now}"
        );
        let decided = fs::read_to_string(dir.join("shop.not-findings.md")).unwrap();
        assert!(decided.contains("logging the request id"));
        assert!(dir.join("shop.yaml.1000.bak").exists());
        for p in [
            &waiting.config,
            &waiting.why,
            &waiting.not_findings,
            &waiting.folds,
        ] {
            assert!(!p.exists(), "{}", p.display());
        }
        assert_eq!(apply(&config, 1001).unwrap(), None, "nothing waits");
        // Only the latest backups are kept.
        let plain = Proposal {
            config: GOOD.into(),
            not_findings: None,
            why: String::new(),
        };
        for at in 1002..1008 {
            save(&config, &plain, None).unwrap();
            assert_eq!(apply(&config, at).unwrap().unwrap().folded, None);
        }
        let backups = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
            .count();
        assert_eq!(backups, BACKUPS_KEPT);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn kept_finding(id: &str, blocked: bool) -> Kept {
        Kept {
            id: id.into(),
            lens: "correctness".into(),
            title: format!("{id} title"),
            detail: "detail".into(),
            file: Some("src/a.rs".into()),
            line: Some(3),
            severity: "High".into(),
            blocked,
            state: "open".into(),
        }
    }

    fn finding(id: &str, action: Action, state: FindingState) -> Finding {
        Finding {
            id: id.into(),
            title: format!("{id} title"),
            detail: "detail".into(),
            file: Some("src/a.rs".into()),
            line: Some(3),
            severity: Severity::High,
            action,
            reason: String::new(),
            state,
        }
    }

    #[test]
    fn a_finished_reviews_findings_are_kept_the_latest_of_each() {
        let state = scratch();
        let round = |n: u32, findings: Vec<Finding>| Round {
            round: n,
            findings,
            ..Round::default()
        };
        let mut lenses = BTreeMap::new();
        lenses.insert(
            "correctness".to_string(),
            LensResult {
                id: "correctness".into(),
                criteria: "c".into(),
                rounds: vec![
                    round(1, vec![finding("F1", Action::Blocks, FindingState::Open)]),
                    round(2, vec![finding("F1", Action::Blocks, FindingState::Folded)]),
                ],
                done: Vec::new(),
                outcome: Outcome::Confirmed,
                joined: String::new(),
            },
        );
        lenses.insert(
            "docs".to_string(),
            LensResult {
                id: "docs".into(),
                criteria: "d".into(),
                rounds: vec![round(
                    1,
                    vec![finding("F2", Action::NeverBlocks, FindingState::Logged)],
                )],
                done: Vec::new(),
                outcome: Outcome::Confirmed,
                joined: String::new(),
            },
        );
        keep(&state, "R1", "shop", &lenses, 9).unwrap();
        let run = kept(&state, "R1").unwrap();
        assert_eq!(run.repo, "shop");
        assert_eq!(run.findings.len(), 2);
        assert_eq!(run.findings[0].state, "folded", "the latest round's");
        assert!(run.findings[0].blocked && !run.findings[1].blocked);
        // By number from 1, or by ID.
        assert_eq!(finding_of(&run, "2").id, "F2");
        assert_eq!(finding_of(&run, "F1").id, "F1");
        assert!(super::finding(&run, "9").is_err());
        let shown = listed(&run);
        assert!(shown.contains("  1. [correctness] F1 title (High, blocked, folded; F1)"));
        assert!(shown.contains("--verdict right|not-a-defect|not-a-blocker|should-block"));
        assert!(kept(&state, "R9").is_err());
        fs::remove_dir_all(&state).unwrap();
    }

    fn finding_of<'a>(run: &'a KeptRun, which: &str) -> &'a Kept {
        super::finding(run, which).unwrap()
    }

    #[test]
    fn feedback_is_kept_per_repository_and_read_since_a_calibration() {
        let state = scratch();
        let said = |at: i64, verdict: Option<Verdict>, missed: Option<&str>| Feedback {
            at,
            run: "R1".into(),
            finding: verdict.map(|_| kept_finding("F1", false)),
            verdict,
            missed: missed.map(str::to_string),
            lens: missed.map(|_| "security".to_string()),
            note: None,
        };
        add_feedback(&state, "shop", &said(10, Some(Verdict::Right), None)).unwrap();
        add_feedback(&state, "shop", &said(20, Some(Verdict::ShouldBlock), None)).unwrap();
        add_feedback(&state, "shop", &said(30, None, Some("the token is logged"))).unwrap();
        assert_eq!(feedback(&state, "shop", None).unwrap().len(), 3);
        let since = feedback(&state, "shop", Some(15)).unwrap();
        assert_eq!(since.len(), 2);
        assert!(
            since[0]
                .described()
                .contains("a real defect that should have blocked")
        );
        assert_eq!(
            since[1].described(),
            "Review R1, lens security: it missed this, the user says: the token is logged."
        );
        assert!(feedback(&state, "other", None).unwrap().is_empty());
        // Only the latest are kept.
        for at in 0..FEEDBACK_KEPT as i64 {
            add_feedback(&state, "shop", &said(100 + at, Some(Verdict::Right), None)).unwrap();
        }
        let all = feedback(&state, "shop", None).unwrap();
        assert_eq!(all.len(), FEEDBACK_KEPT);
        assert_eq!(all[0].at, 100);
        fs::remove_dir_all(&state).unwrap();
    }
}
