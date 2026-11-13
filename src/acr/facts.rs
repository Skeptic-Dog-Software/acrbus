//! The change map: what tools found in the code about a change, built once
//! a round and shared, byte for byte, ahead of every reviewer's and Judge's
//! own text, so a role's calls read it from the model's prompt cache rather
//! than each paying to read it fresh.
//!
//! Nothing in it comes from a model: no summary, no finding, no "look here".
//! Each changed file's diff, the definitions its changed lines sit in, where
//! those names appear in the code, and the tests that name them. References
//! are found by name with `git grep`, which every repository has.
//!
//! With a pull request, its words come too, fetched with `gh` once a round:
//! its title and description and the issues it links, which a Judge ruling
//! on what the change missed would otherwise read for itself, each call
//! paying for them fresh, and its review comments.
//!
//! The order is the cache's. A prompt cache hits on an identical prefix, so
//! what stays the same comes first and what changes comes last: the pull
//! request's title, description and issues, then each round's files in the
//! order they were first mapped, then the round, the pull request's review
//! comments, which can grow during a review, and this round's gate. A later
//! round keeps every section an earlier one had, byte for byte, where it
//! was: a file a fix touched keeps its earlier section, marked superseded
//! but rendered as it was, and comes again after them all as it is now, as
//! does a file the change has just begun to touch, and one it no longer
//! touches says so there. So a round shares all of the last one's files,
//! whichever a fix touched. Nothing that varies between rounds or calls, no
//! round number, count of calls, time or path of a copy, sits before a fact
//! that doesn't.
//!
//! With a record of what this repository's reviewers open (see
//! `learned`), the files they keep opening come too, whole, after the
//! change's own files of the same round: each a file the change doesn't
//! touch, which a reviewer would otherwise spend turns to open, every turn
//! reading again all that came before. A file once given stays for the rest
//! of the review, in the place it was given, so a round's facts only grow
//! at their end; one a fix touches moves to the round it changed in.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::change::{self, Change};

/// The most definitions a file's facts name.
const MAX_SYMBOLS: usize = 20;
/// The most places a definition's references list.
const MAX_REFS: usize = 25;
/// Names this short match too much to be worth a search.
const MIN_NAME: usize = 3;
/// The most bytes of a pull request's text, or of one issue it links, the
/// facts carry: a longer one is cut, and says so.
const MAX_PR_TEXT: usize = 32 * 1024;
/// The most issues a pull request's facts carry.
const MAX_ISSUES: usize = 10;

/// The most searches for references one round's facts make, in the diff's
/// file order, so a change of hundreds of files maps in seconds: a file
/// past them names its definitions without their references.
const MAX_SEARCHES: usize = 400;

/// One changed file's facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFacts {
    pub path: String,
    /// Its content at the change's head, by hash: how a later round knows
    /// a fix left it alone. "deleted" for a file the change removes.
    pub blob: String,
    /// The round this content was first mapped. Facts are sorted by it, so
    /// a later round shares the earlier one's prefix up to the first file a
    /// fix touched.
    pub since: u32,
    /// The definitions its changed lines sit in, or that it adds.
    pub symbols: Vec<String>,
    /// Where those names appear elsewhere, as `path:line`.
    pub references: Vec<String>,
    /// Test files among those, and changed test files.
    pub tests: Vec<String>,
    /// Its part of the diff, as the diff has it.
    pub diff: String,
    /// A later round's section for the same file holds now: this one stays
    /// where it was, rendered as it was, so the facts keep their prefix.
    #[serde(default, skip_serializing_if = "is_false")]
    pub superseded: bool,
    /// The change no longer touches the file, as of `since`: said in place
    /// of a section.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gone: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A file the change doesn't touch that this repository's reviewers keep
/// opening, given whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnedFile {
    pub path: String,
    /// Its content, by hash, as [`FileFacts::blob`].
    pub blob: String,
    /// The round it was first given, or last changed.
    pub since: u32,
    pub text: String,
}

/// A round's facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub files: Vec<FileFacts>,
    /// Files reviewers keep opening, in the order they're given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learned: Vec<LearnedFile>,
}

/// What [`Facts::add_learned`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearnedAdded {
    /// Files given, the earlier rounds' included.
    pub files: usize,
    /// Their bytes.
    pub bytes: usize,
    /// Files new this round.
    pub new: usize,
    /// Files the record ranks that the budget left out.
    pub left_out: usize,
}

impl Facts {
    /// Builds the facts for `change` at round `round`, in the checkout at
    /// `repo`, reusing `prior`'s for every file whose content hasn't
    /// changed: their bytes then stay the same from round to round.
    pub fn build(repo: &Path, change: &Change, round: u32, prior: Option<&Facts>) -> Facts {
        let left_out: BTreeSet<&str> = change.generated.iter().map(|g| g.path.as_str()).collect();
        let parts: Vec<(String, String)> = change::per_file(&change.diff)
            .into_iter()
            .filter(|(path, _)| !path.is_empty() && !left_out.contains(path.as_str()))
            .collect();
        let mut searches = MAX_SEARCHES;
        let Some(prior) = prior else {
            let mut files: Vec<FileFacts> = parts
                .iter()
                .map(|(path, part)| {
                    let blob = blob_of(repo, path);
                    map_file(repo, path, part, &blob, round, &mut searches)
                })
                .collect();
            files.sort_by(|a, b| (a.since, &a.path).cmp(&(b.since, &b.path)));
            return Facts {
                files,
                learned: Vec::new(),
            };
        };
        // Every earlier section stays, in place and as it was.
        let mut files = prior.files.clone();
        let mut added = Vec::new();
        for (path, part) in &parts {
            let blob = blob_of(repo, path);
            let live = files
                .iter()
                .rposition(|f| &f.path == path && !f.superseded && !f.gone);
            match live {
                Some(at) if files[at].blob == blob && &files[at].diff == part => {}
                Some(at) => {
                    files[at].superseded = true;
                    added.push(map_file(repo, path, part, &blob, round, &mut searches));
                }
                None => added.push(map_file(repo, path, part, &blob, round, &mut searches)),
            }
        }
        // A file the change no longer touches says so, after the rest.
        let touched: BTreeSet<&str> = parts.iter().map(|(p, _)| p.as_str()).collect();
        for f in files.iter_mut() {
            if !f.superseded && !f.gone && !touched.contains(f.path.as_str()) {
                f.superseded = true;
                added.push(FileFacts {
                    path: f.path.clone(),
                    blob: "gone".into(),
                    since: round,
                    symbols: Vec::new(),
                    references: Vec::new(),
                    tests: Vec::new(),
                    diff: String::new(),
                    superseded: false,
                    gone: true,
                });
            }
        }
        added.sort_by(|a, b| a.path.cmp(&b.path));
        files.extend(added);
        Facts {
            files,
            learned: Vec::new(),
        }
    }

    /// Each file's section that holds now: its latest, for a file the
    /// change still touches.
    pub fn current(&self) -> impl Iterator<Item = &FileFacts> {
        self.files.iter().filter(|f| !f.superseded && !f.gone)
    }

    /// Adds the files `ranked` names, most opened first, to what `prior`
    /// gave, within `budget` bytes of their text, reading each from the
    /// checkout at `repo`. A file `prior` gave stays where it was, with its
    /// round, unless its content changed, which moves it to `round`; one the
    /// change now touches, or that's gone, drops out. Each new file comes
    /// after them, at `round`. A file the change touches, one that isn't
    /// text, or one that doesn't fit what's left of the budget, isn't added.
    pub fn add_learned(
        &mut self,
        repo: &Path,
        round: u32,
        prior: Option<&Facts>,
        ranked: &[(String, u32)],
        budget: usize,
    ) -> LearnedAdded {
        let changed: BTreeSet<&str> = self.current().map(|f| f.path.as_str()).collect();
        let mut learned: Vec<LearnedFile> = Vec::new();
        let mut bytes = 0usize;
        let read = |path: &str| -> Option<String> {
            let full = repo.join(path);
            let meta = fs::symlink_metadata(&full).ok()?;
            if !meta.file_type().is_file() {
                return None;
            }
            let text = fs::read_to_string(&full).ok()?;
            (!text.contains('\0')).then_some(text)
        };
        for old in prior.map(|p| p.learned.as_slice()).unwrap_or_default() {
            if changed.contains(old.path.as_str()) {
                continue;
            }
            let blob = blob_of(repo, &old.path);
            if blob == DELETED || blob.is_empty() {
                continue;
            }
            let kept = if blob == old.blob {
                old.clone()
            } else {
                let Some(text) = read(&old.path) else {
                    continue;
                };
                LearnedFile {
                    path: old.path.clone(),
                    blob,
                    since: round,
                    text,
                }
            };
            bytes += kept.text.len();
            learned.push(kept);
        }
        let mut added = LearnedAdded::default();
        for (path, _) in ranked {
            if changed.contains(path.as_str()) || learned.iter().any(|l| &l.path == path) {
                continue;
            }
            let Some(text) = read(path) else {
                continue;
            };
            if bytes + text.len() > budget {
                added.left_out += 1;
                continue;
            }
            bytes += text.len();
            added.new += 1;
            learned.push(LearnedFile {
                path: path.clone(),
                blob: blob_of(repo, path),
                since: round,
                text,
            });
        }
        // Earlier rounds' first, each where it was; then this round's.
        learned.sort_by_key(|l| l.since);
        added.files = learned.len();
        added.bytes = bytes;
        self.learned = learned;
        added
    }

    /// The facts as every role gets them: the same bytes for every call in a
    /// round, and the same as the last round's up to the first file a fix
    /// touched. `gate` is this round's test gate, if it ran, and goes last,
    /// after the only line that names the round.
    #[cfg(test)]
    pub fn render(&self, round: u32, gate: Option<&str>) -> String {
        self.render_with(round, gate, None)
    }

    /// [`Facts::render`], with the pull request's words: its title,
    /// description and issues after the head, ahead of every file, and its
    /// review comments after the round, before the gate.
    pub fn render_with(&self, round: u32, gate: Option<&str>, pr: Option<&PrText>) -> String {
        let mut out = String::from(HEAD);
        if let Some(pr) = pr {
            out.push_str(&pr.said());
        }
        // Each round's files: the change's, then those reviewers keep
        // opening, so what a later round adds comes after all of an
        // earlier one's.
        let mut learned = self.learned.iter().peekable();
        for f in &self.files {
            while let Some(l) = learned.next_if(|l| l.since < f.since) {
                out.push_str(&render_learned(l));
            }
            out.push_str(&render_file(f));
        }
        for l in learned {
            out.push_str(&render_learned(l));
        }
        out.push_str(&format!("\n# This round\n\nRound {round}.\n"));
        if let Some(c) = pr.map(|p| p.comments.trim()).filter(|c| !c.is_empty()) {
            out.push_str(&format!(
                "\n## The pull request's review comments\n\n{}\n",
                normalize(c)
            ));
        }
        if let Some(g) = gate.map(str::trim).filter(|g| !g.is_empty()) {
            out.push_str(&format!("\n## The test gate\n\n{}\n", normalize(g)));
        }
        out
    }

    /// The key a round's facts are kept under: a hash of what they say.
    pub fn key(rendered: &str) -> String {
        hex(&Sha256::digest(rendered.as_bytes()))[..16].to_string()
    }
}

const HEAD: &str = "# The change's facts\n\n\
    Found in the code by tools, with nothing any model wrote: each changed file's diff, the \
    definitions its changed lines sit in, where those names appear, and the tests that name \
    them. Names are matched by text, so a reference may be a namesake. A file's references are \
    as of the round it was mapped. A file a fix touched keeps its earlier section where it was, \
    and comes again after the rest as it is now: the later section is the one that holds. A \
    file the change no longer touches says so, after the rest. The change and its claims are \
    also in the attached file.\n";

/// A pull request's words, as `gh` gave them: what it says, the issues it
/// links, and its review comments, the last apart since they can grow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrText {
    pub number: u64,
    pub title: String,
    pub body: String,
    /// Each linked issue: its number, title and body.
    pub issues: Vec<(u64, String, String)>,
    /// Its conversation, its reviews and their comments on lines, oldest
    /// first, each with who wrote it.
    pub comments: String,
}

impl PrText {
    /// Its title, description and issues, as the facts carry them.
    fn said(&self) -> String {
        let mut out = format!(
            "\n# The pull request, #{}\n\nWhat its author and its issues say, as `gh` gave \
             them: claims to check, like the change's own words.\n\n## {}\n\n{}\n",
            self.number,
            normalize(self.title.trim()).trim_end(),
            normalize(&cut(self.body.trim(), MAX_PR_TEXT))
        );
        for (n, title, body) in self.issues.iter().take(MAX_ISSUES) {
            out.push_str(&format!(
                "\n## Issue #{n}: {}\n\n{}\n",
                normalize(title.trim()).trim_end(),
                normalize(&cut(body.trim(), MAX_PR_TEXT))
            ));
        }
        out
    }
}

/// `text`, cut to `max` bytes at a character's edge, saying so if it was.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n(cut here: {} bytes in all)",
        &text[..end],
        text.len()
    )
}

/// Pull request `pr`'s words, with `gh` in the checkout at `repo`: its
/// title, description and review comments, and the issues it links.
pub fn fetch_pr(repo: &Path, pr: u64) -> Result<PrText> {
    let gh = |args: &[&str]| -> Result<String> {
        let out = Command::new("gh")
            .args(args)
            .current_dir(repo)
            .stdin(Stdio::null())
            .output()
            .context("running gh")?;
        if !out.status.success() {
            bail!(
                "gh {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let number = pr.to_string();
    let view = gh(&[
        "pr",
        "view",
        &number,
        "--json",
        "number,title,body,closingIssuesReferences,comments,reviews",
    ])?;
    let inline = gh(&[
        "api",
        "--paginate",
        &format!("repos/{{owner}}/{{repo}}/pulls/{pr}/comments"),
    ])
    .unwrap_or_default();
    let mut text = pr_text(&view, &inline)?;
    for (n, title, body) in &mut text.issues {
        if let Ok(issue) = gh(&["issue", "view", &n.to_string(), "--json", "title,body"]) {
            let v: serde_json::Value = serde_json::from_str(&issue).unwrap_or_default();
            *title = v["title"].as_str().unwrap_or(title).to_string();
            *body = v["body"].as_str().unwrap_or_default().to_string();
        }
    }
    Ok(text)
}

/// A pull request's words from what `gh pr view --json` and the API's
/// review comments gave: the issues by number, their text fetched apart.
pub fn pr_text(view: &str, inline: &str) -> Result<PrText> {
    let v: serde_json::Value = serde_json::from_str(view).context("reading gh's pull request")?;
    let s = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    let issues = v["closingIssuesReferences"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| Some((i["number"].as_u64()?, s(&i["title"]), String::new())))
        .collect();
    // Every comment, oldest first, with who wrote it and when.
    let mut said: Vec<(String, String)> = Vec::new();
    let author = |c: &serde_json::Value| {
        c["author"]["login"]
            .as_str()
            .or_else(|| c["user"]["login"].as_str())
            .unwrap_or("someone")
            .to_string()
    };
    for c in v["comments"].as_array().into_iter().flatten() {
        let body = s(&c["body"]);
        if !body.trim().is_empty() {
            said.push((
                s(&c["createdAt"]),
                format!("{} commented:\n\n{}", author(c), body.trim()),
            ));
        }
    }
    for r in v["reviews"].as_array().into_iter().flatten() {
        let body = s(&r["body"]);
        let state = s(&r["state"]).to_lowercase().replace('_', " ");
        if !body.trim().is_empty() || state == "changes requested" {
            said.push((
                s(&r["submittedAt"]),
                format!("{} reviewed ({state}):\n\n{}", author(r), body.trim()),
            ));
        }
    }
    // The API gives one page of review comments after another; read each.
    let mut pages: Vec<serde_json::Value> = Vec::new();
    for part in serde_json::Deserializer::from_str(inline).into_iter::<serde_json::Value>() {
        match part {
            Ok(serde_json::Value::Array(items)) => pages.extend(items),
            Ok(_) | Err(_) => break,
        }
    }
    for c in &pages {
        let at = match (
            c["path"].as_str(),
            c["line"].as_u64().or_else(|| c["original_line"].as_u64()),
        ) {
            (Some(p), Some(l)) => format!(" on {p}:{l}"),
            (Some(p), None) => format!(" on {p}"),
            _ => String::new(),
        };
        let body = s(&c["body"]);
        if !body.trim().is_empty() {
            said.push((
                s(&c["created_at"]),
                format!("{} commented{at}:\n\n{}", author(c), body.trim()),
            ));
        }
    }
    said.sort_by(|a, b| a.0.cmp(&b.0));
    let comments = said
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    Ok(PrText {
        number: v["number"].as_u64().unwrap_or_default(),
        title: s(&v["title"]),
        body: s(&v["body"]),
        issues,
        comments: cut(&comments, MAX_PR_TEXT),
    })
}

fn render_learned(l: &LearnedFile) -> String {
    let text = normalize(&l.text);
    // A fence longer than any run of backticks in the file.
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!(
        "\n## Often read: {}\n\nNot touched by the change: this repository's reviewers keep \
         opening it, so it's here whole.\n\n{fence}\n{text}{fence}\n",
        l.path
    )
}

fn render_file(f: &FileFacts) -> String {
    let mut out = format!("\n## {}\n\n", f.path);
    if f.gone {
        out.push_str(&format!(
            "No longer part of the change, as of round {}.\n",
            f.since
        ));
        return out;
    }
    if f.blob == DELETED {
        out.push_str("Deleted by the change.\n");
    }
    if !f.symbols.is_empty() {
        out.push_str(&format!("Definitions changed: {}\n", f.symbols.join(", ")));
    }
    if !f.references.is_empty() {
        out.push_str(&format!("Referenced at: {}\n", f.references.join(", ")));
    }
    if !f.tests.is_empty() {
        out.push_str(&format!("Tests that name them: {}\n", f.tests.join(", ")));
    }
    out.push_str(&format!("\n```diff\n{}```\n", normalize(&f.diff)));
    out
}

/// Line endings and trailing space made the same, so the bytes don't depend
/// on a checkout's settings.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.replace("\r\n", "\n").split_inclusive('\n') {
        out.push_str(line.trim_end_matches(['\r', '\n', ' ', '\t']));
        out.push('\n');
    }
    if !text.ends_with('\n') && out.ends_with('\n') && !text.is_empty() {
        // Keep a missing final newline missing.
        out.pop();
        out.push('\n');
    }
    out
}

const DELETED: &str = "deleted";

fn blob_of(repo: &Path, path: &str) -> String {
    if !repo.join(path).is_file() {
        return DELETED.to_string();
    }
    change::git(repo, &["hash-object", "--", path])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn map_file(
    repo: &Path,
    path: &str,
    part: &str,
    blob: &str,
    round: u32,
    searches: &mut usize,
) -> FileFacts {
    let mut facts = FileFacts {
        path: path.to_string(),
        blob: blob.to_string(),
        since: round,
        symbols: Vec::new(),
        references: Vec::new(),
        tests: Vec::new(),
        diff: part.to_string(),
        superseded: false,
        gone: false,
    };
    if blob == DELETED {
        return facts;
    }
    let text = fs::read_to_string(repo.join(path)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let defs = definitions(path, &lines);
    let changed = changed_lines(part);
    let mut names: BTreeSet<String> = BTreeSet::new();
    for line in &changed {
        // The definition a changed line sits in: the nearest above it.
        if let Some((_, name)) = defs.iter().rev().find(|(at, _)| *at <= *line) {
            names.insert(name.clone());
        }
    }
    let mut symbols: Vec<String> = names.into_iter().take(MAX_SYMBOLS).collect();
    symbols.sort();
    let own: BTreeSet<(String, u64)> = defs.iter().map(|(at, _)| (path.to_string(), *at)).collect();
    let mut refs: BTreeSet<String> = BTreeSet::new();
    let mut tests: BTreeSet<String> = BTreeSet::new();
    for name in symbols.iter().filter(|n| n.len() >= MIN_NAME) {
        if *searches == 0 {
            break;
        }
        *searches -= 1;
        let mut found: Vec<(String, u64)> = grep(repo, name)
            .into_iter()
            .filter(|hit| !own.contains(hit))
            .collect();
        found.sort();
        for (file, line) in found.iter().take(MAX_REFS) {
            refs.insert(format!("{file}:{line}"));
        }
        for (file, _) in &found {
            if is_test(file) {
                tests.insert(file.clone());
            }
        }
    }
    if is_test(path) {
        tests.insert(path.to_string());
    }
    facts.symbols = symbols;
    facts.references = sort_refs(refs);
    facts.tests = tests.into_iter().collect();
    facts
}

/// `path:line` in path order, then line order.
fn sort_refs(refs: BTreeSet<String>) -> Vec<String> {
    let mut v: Vec<(String, u64)> = refs
        .into_iter()
        .filter_map(|r| {
            let (p, l) = r.rsplit_once(':')?;
            Some((p.to_string(), l.parse().ok()?))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(p, l)| format!("{p}:{l}")).collect()
}

/// The new file's line numbers the diff adds or changes, and the line a
/// pure removal sits at.
fn changed_lines(part: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for (start, _, text) in change::hunks(part) {
        let mut line = start;
        let mut any = false;
        for l in text.lines().skip(1) {
            if l.starts_with('+') && !l.starts_with("+++") {
                out.push(line);
                line += 1;
                any = true;
            } else if l.starts_with('-') && !l.starts_with("---") {
                any = true;
            } else {
                line += 1;
            }
        }
        if any && out.last() != Some(&line.saturating_sub(1)) {
            out.push(start.max(1));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Definitions in a file, by line, for the languages a pattern can find
/// them in reliably. Other files get none, and their diff still goes in.
fn definitions(path: &str, lines: &[&str]) -> Vec<(u64, String)> {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let patterns: &[&str] = match ext {
        "go" => &[
            r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)",
            r"^type\s+([A-Za-z_]\w*)",
        ],
        "rs" => &[
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\S+\s+)?fn\s+([A-Za-z_]\w*)",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|union|type|mod)\s+([A-Za-z_]\w*)",
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+([A-Z_][A-Z0-9_]*)\s*:",
        ],
        "py" => &[
            r"^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)",
            r"^\s*class\s+([A-Za-z_]\w*)",
        ],
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => &[
            r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)",
            r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)",
            r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
            r"^\s*(?:export\s+)?(?:interface|type|enum)\s+([A-Za-z_$][\w$]*)",
        ],
        "java" | "kt" | "swift" | "cs" => &[
            r"^\s*(?:(?:public|private|protected|internal|static|final|abstract|open|override|sealed|data)\s+)*(?:class|interface|enum|struct|object|record)\s+([A-Za-z_]\w*)",
            r"^\s*(?:(?:public|private|protected|internal|static|final|override|open)\s+)*func\s+([A-Za-z_]\w*)",
            r"^\s*(?:(?:public|private|protected|internal|static|final|override|open|suspend)\s+)*fun\s+([A-Za-z_]\w*)",
        ],
        _ => &[],
    };
    let res: Vec<Regex> = patterns.iter().filter_map(|p| Regex::new(p).ok()).collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        for re in &res {
            if let Some(name) = re.captures(line).and_then(|c| c.get(1)) {
                out.push((i as u64 + 1, name.as_str().to_string()));
                break;
            }
        }
    }
    out
}

/// Every place `name` appears as a whole word in the checkout's tracked
/// files, as (path, line).
fn grep(repo: &Path, name: &str) -> Vec<(String, u64)> {
    let out = change::git(
        repo,
        &["grep", "-n", "-w", "-F", "-I", "--no-color", "-e", name],
    )
    .unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let (path, rest) = l.split_once(':')?;
            let (line, _) = rest.split_once(':')?;
            Some((path.to_string(), line.parse().ok()?))
        })
        .collect()
}

/// A test file, by the names test tools use.
pub fn is_test(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/")
        || path.starts_with("tests/")
        || path.contains("/test/")
        || path.starts_with("test/")
        || path.contains("__tests__/")
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
        || file.starts_with("test_")
        || file.contains(".test.")
        || file.contains(".spec.")
        || file.ends_with("Test.java")
        || file.ends_with("Tests.swift")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// How many leading bytes two texts share, cut back to a character
/// boundary: the prefix a cache could serve to both.
pub fn shared_prefix(a: &str, b: &str) -> usize {
    let mut n = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    while n > 0 && !a.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// What a fan-out's calls, or two rounds, share at the head of what they
/// send: bytes, and tokens at about four bytes each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefix {
    pub round: u32,
    /// "reviewers", "judges", or "facts since round N".
    pub what: String,
    pub bytes: u64,
    pub tokens: u64,
}

impl Prefix {
    pub fn new(round: u32, what: impl Into<String>, bytes: usize) -> Prefix {
        Prefix {
            round,
            what: what.into(),
            bytes: bytes as u64,
            tokens: bytes.div_ceil(4) as u64,
        }
    }

    pub fn line(&self) -> String {
        format!(
            "shared prefix, {}: {} bytes (about {} tokens)",
            self.what,
            thousands(self.bytes),
            thousands(self.tokens)
        )
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The shared prefix of a fan-out's prompts after the facts: what their own
/// texts share before they differ.
pub fn common_prefix(texts: &[&str]) -> usize {
    match texts.split_first() {
        None => 0,
        Some((first, rest)) => rest
            .iter()
            .map(|t| shared_prefix(first, t))
            .min()
            .unwrap_or(first.len()),
    }
}

/// Facts kept by round, in the run's folder.
pub fn kept(dir: &Path, round: u32) -> Option<(Facts, String)> {
    let facts: Facts = serde_json::from_str(
        &fs::read_to_string(dir.join(format!("round-{round}.facts.json"))).ok()?,
    )
    .ok()?;
    let text = fs::read_to_string(dir.join(format!("round-{round}.facts.md"))).ok()?;
    Some((facts, text))
}

pub fn keep(dir: &Path, round: u32, facts: &Facts, text: &str) -> Result<std::path::PathBuf> {
    fs::create_dir_all(dir)?;
    fs::write(
        dir.join(format!("round-{round}.facts.json")),
        serde_json::to_vec_pretty(facts)?,
    )?;
    let path = dir.join(format!("round-{round}.facts.md"));
    fs::write(&path, text)?;
    Ok(path)
}

/// Facts' files by path, each at its section that holds now, for a test or
/// a report.
#[cfg(test)]
pub fn by_path(facts: &Facts) -> std::collections::BTreeMap<&str, &FileFacts> {
    facts.current().map(|f| (f.path.as_str(), f)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acr::change::tests::{branch, commit, repo_with};

    fn change_of(repo: &Path, base: &str) -> Change {
        let head = change::git(repo, &["rev-parse", "HEAD"]).unwrap();
        change::load(
            repo,
            base,
            Some(head.trim()),
            crate::acr::config::ReviewScope::WholePr,
            &crate::acr::config::Generated::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_pull_requests_words_are_read_from_gh_with_every_comment_in_order() {
        let view = r#"{
            "number": 7, "title": "Retry the upload", "body": "Fixes #12.\r\nRetries twice.",
            "closingIssuesReferences": [{"number": 12, "title": "Uploads drop"}],
            "comments": [
                {"author": {"login": "ann"}, "body": "Why twice?", "createdAt": "2026-01-02T00:00:00Z"},
                {"author": {"login": "bot"}, "body": "  ", "createdAt": "2026-01-01T00:00:00Z"}
            ],
            "reviews": [
                {"author": {"login": "bo"}, "body": "", "state": "CHANGES_REQUESTED", "submittedAt": "2026-01-03T00:00:00Z"},
                {"author": {"login": "cy"}, "body": "", "state": "APPROVED", "submittedAt": "2026-01-04T00:00:00Z"}
            ]
        }"#;
        // Two pages, as `gh api --paginate` prints them.
        let inline = r#"[{"user": {"login": "bo"}, "path": "up.rs", "line": 9, "body": "Off by one", "created_at": "2026-01-03T00:00:01Z"}]
[{"user": {"login": "ann"}, "path": "up.rs", "original_line": 4, "body": "And here", "created_at": "2026-01-01T12:00:00Z"}]"#;
        let pr = pr_text(view, inline).unwrap();
        assert_eq!((pr.number, pr.title.as_str()), (7, "Retry the upload"));
        assert_eq!(
            pr.issues,
            vec![(12, "Uploads drop".to_string(), String::new())]
        );
        // Oldest first; an empty comment and a bare approval say nothing.
        let order: Vec<usize> = [
            "ann commented on up.rs:4",
            "ann commented:",
            "bo reviewed (changes requested)",
            "bo commented on up.rs:9",
        ]
        .iter()
        .map(|n| {
            pr.comments
                .find(n)
                .unwrap_or_else(|| panic!("{n} in {}", pr.comments))
        })
        .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "{order:?} in {}",
            pr.comments
        );
        assert!(
            !pr.comments.contains("cy") && !pr.comments.contains("bot"),
            "{}",
            pr.comments
        );
        // A pull request with none of them still reads.
        let bare = pr_text(r#"{"number": 3, "title": "t", "body": null}"#, "").unwrap();
        assert!(bare.issues.is_empty() && bare.comments.is_empty() && bare.body.is_empty());
    }

    /// A repository of plain files, committed, so their blobs hash.
    fn learned_repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-learnedfacts-{}", crate::util::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        for (path, text) in files {
            let full = dir.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        dir
    }

    fn changed(path: &str, since: u32) -> FileFacts {
        FileFacts {
            path: path.into(),
            blob: "b".into(),
            since,
            symbols: Vec::new(),
            references: Vec::new(),
            tests: Vec::new(),
            diff: "+x\n".into(),
            superseded: false,
            gone: false,
        }
    }

    fn ranked(paths: &[&str]) -> Vec<(String, u32)> {
        paths
            .iter()
            .rev()
            .enumerate()
            .map(|(i, p)| (p.to_string(), i as u32 + 1))
            .rev()
            .collect()
    }

    #[test]
    fn often_read_files_come_after_the_change_s_own_and_only_grow_at_the_end() {
        let repo = learned_repo(&[
            ("src/changed.rs", "changed"),
            ("src/types.rs", "pub struct T;\n"),
            ("src/util.rs", "pub fn u() {}\n"),
            ("docs/guide.md", "how it works\n"),
        ]);
        let mut round1 = Facts {
            files: vec![changed("src/changed.rs", 1)],
            learned: Vec::new(),
        };
        let added = round1.add_learned(
            &repo,
            1,
            None,
            &ranked(&["src/changed.rs", "src/types.rs", "src/util.rs"]),
            1024,
        );
        assert_eq!((added.files, added.new, added.left_out), (2, 2, 0));
        let paths: Vec<&str> = round1.learned.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(
            paths,
            ["src/types.rs", "src/util.rs"],
            "the change's own file isn't repeated"
        );
        let text1 = round1.render(1, None);
        let at = |t: &str, what: &str| t.find(what).unwrap_or_else(|| panic!("{what} in {t}"));
        assert!(at(&text1, "## src/changed.rs") < at(&text1, "## Often read: src/types.rs"));
        assert!(at(&text1, "## Often read: src/util.rs") < at(&text1, "# This round"));
        assert!(text1.contains("pub struct T;"), "given whole");
        // Round 2: a fix touched another file, and the record ranks a new
        // file first. What round 1 gave stays where it was, the new comes
        // after, and round 2 shares round 1's bytes up to its first change.
        let mut round2 = Facts {
            files: vec![changed("src/changed.rs", 1), changed("src/fixed.rs", 2)],
            learned: Vec::new(),
        };
        round2.add_learned(
            &repo,
            2,
            Some(&round1),
            &ranked(&["docs/guide.md", "src/util.rs", "src/types.rs"]),
            1024,
        );
        let paths: Vec<&str> = round2.learned.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, ["src/types.rs", "src/util.rs", "docs/guide.md"]);
        let text2 = round2.render(2, None);
        let shared = text1.find("\n# This round").unwrap();
        assert_eq!(
            &text2[..shared],
            &text1[..shared],
            "round 1's facts are round 2's prefix"
        );
        assert!(at(&text2, "## src/fixed.rs") < at(&text2, "## Often read: docs/guide.md"));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn often_read_files_keep_to_the_budget_and_follow_their_content() {
        let repo = learned_repo(&[
            ("a.rs", "aaaa"),
            ("big.rs", &"b".repeat(100)),
            ("c.rs", "cc"),
            ("bin.dat", "x\0y"),
        ]);
        let mut f = Facts::default();
        let added = f.add_learned(
            &repo,
            1,
            None,
            &ranked(&["big.rs", "a.rs", "bin.dat", "gone.rs", "c.rs"]),
            10,
        );
        let paths: Vec<&str> = f.learned.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(
            paths,
            ["a.rs", "c.rs"],
            "what fits, no binary, nothing missing"
        );
        assert_eq!((added.bytes, added.left_out), (6, 1));
        // A file whose content changed moves to the round it changed in.
        fs::write(repo.join("a.rs"), "a2").unwrap();
        let mut next = Facts::default();
        next.add_learned(&repo, 2, Some(&f), &[], 10);
        let a = next.learned.iter().find(|l| l.path == "a.rs").unwrap();
        assert_eq!((a.since, a.text.as_str()), (2, "a2"));
        assert_eq!(
            next.learned.last().unwrap().path,
            "a.rs",
            "after the unchanged"
        );
        // One the change now touches drops out.
        let mut touched = Facts {
            files: vec![changed("c.rs", 2)],
            learned: Vec::new(),
        };
        touched.add_learned(&repo, 2, Some(&f), &[], 10);
        assert!(touched.learned.iter().all(|l| l.path != "c.rs"));
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn an_often_read_file_s_fence_outlasts_its_backticks() {
        let l = LearnedFile {
            path: "doc.md".into(),
            blob: "b".into(),
            since: 1,
            text: "```rust\nfn x() {}\n```\n".into(),
        };
        let r = render_learned(&l);
        assert!(r.contains("````\n```rust"), "{r}");
        assert!(r.trim_end().ends_with("````"), "{r}");
    }

    #[test]
    fn a_pull_requests_words_sit_where_the_cache_wants_them() {
        let facts = Facts {
            files: vec![FileFacts {
                path: "up.rs".into(),
                blob: "b".into(),
                since: 1,
                symbols: vec!["upload".into()],
                references: Vec::new(),
                tests: Vec::new(),
                diff: "+retry\n".into(),
                superseded: false,
                gone: false,
            }],
            learned: Vec::new(),
        };
        let pr = PrText {
            number: 7,
            title: "Retry the upload".into(),
            body: "Retries twice.".into(),
            issues: vec![(
                12,
                "Uploads drop".into(),
                "They drop on a bad network.".into(),
            )],
            comments: "ann commented:\n\nWhy twice?".into(),
        };
        let text = facts.render_with(2, Some("ok: 3 passed"), Some(&pr));
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {text}"))
        };
        // What stays the same comes first: the head, the pull request and
        // its issues, then the files; what changes comes last: the round,
        // the comments, the gate.
        assert!(at("# The change's facts") < at("# The pull request, #7"));
        assert!(at("# The pull request, #7") < at("## Issue #12: Uploads drop"));
        assert!(at("They drop on a bad network.") < at("up.rs"));
        assert!(at("up.rs") < at("Round 2."));
        assert!(at("Round 2.") < at("## The pull request's review comments"));
        assert!(at("Why twice?") < at("## The test gate"));
        // A comment added later changes nothing before the round.
        let mut later = pr.clone();
        later.comments.push_str("\n\n---\n\nbo commented:\n\nFine.");
        let after = facts.render_with(2, Some("ok: 3 passed"), Some(&later));
        assert_eq!(&text[..at("Round 2.")], &after[..at("Round 2.")]);
        // Without a pull request, the facts are as they were.
        assert_eq!(
            facts.render_with(2, Some("ok"), None),
            facts.render(2, Some("ok"))
        );
        // A long description is cut, and says so.
        let mut long = pr.clone();
        long.body = "x".repeat(MAX_PR_TEXT + 10);
        let cut_text = facts.render_with(1, None, Some(&long));
        assert!(cut_text.contains(&format!("(cut here: {} bytes in all)", MAX_PR_TEXT + 10)));
    }

    #[test]
    fn a_fix_to_the_first_file_keeps_every_earlier_section_and_adds_its_new_one_last() {
        let repo = repo_with(&[
            ("a.go", "package p\n\nfunc A() int {\n\treturn 0\n}\n"),
            ("b.go", "package p\n\nfunc B() int {\n\treturn 0\n}\n"),
            ("c.go", "package p\n\nfunc C() int {\n\treturn 0\n}\n"),
        ]);
        branch(&repo, "work");
        commit(
            &repo,
            &[
                ("a.go", "package p\n\nfunc A() int {\n\treturn 1\n}\n"),
                ("b.go", "package p\n\nfunc B() int {\n\treturn 1\n}\n"),
                ("c.go", "package p\n\nfunc C() int {\n\treturn 1\n}\n"),
            ],
            "the change",
        );
        let one = Facts::build(&repo, &change_of(&repo, "main"), 1, None);
        let text1 = one.render(1, None);
        // A fix touches a.go, the first file in round 1's facts.
        commit(
            &repo,
            &[("a.go", "package p\n\nfunc A() int {\n\treturn 2\n}\n")],
            "a fix",
        );
        let two = Facts::build(&repo, &change_of(&repo, "main"), 2, Some(&one));
        let text2 = two.render(2, None);
        // Round 2 shares all of round 1's file sections: everything before
        // round 1's own round line.
        let end_of_files = text1.find("\n# This round").unwrap();
        assert!(
            facts_shared(&text1, &text2) >= end_of_files,
            "round 2 shares {} bytes of round 1's {end_of_files} before its round line",
            facts_shared(&text1, &text2)
        );
        // a.go's earlier section stays where it was, and its new one comes
        // after every file round 1 had, as the facts' head says.
        let paths: Vec<&str> = two.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.go", "b.go", "c.go", "a.go"]);
        assert!(two.files[0].superseded && !two.files[3].superseded);
        assert_eq!(two.files[3].since, 2);
        assert!(
            text2.contains("comes again after the rest as it is now"),
            "{text2}"
        );
        // What reads a file's facts reads its latest.
        let now: Vec<&str> = two.current().map(|f| f.path.as_str()).collect();
        assert_eq!(now, ["b.go", "c.go", "a.go"]);
        assert!(two.current().all(|f| !f.superseded));
        // Round 3, with no further fix, shares all of round 2's.
        let three = Facts::build(&repo, &change_of(&repo, "main"), 3, Some(&two));
        let text3 = three.render(3, None);
        assert!(facts_shared(&text2, &text3) >= text2.find("\n# This round").unwrap());
        assert_eq!(three.files, two.files, "nothing new: the same sections");
        // A file the change no longer touches says so, last.
        commit(
            &repo,
            &[("c.go", "package p\n\nfunc C() int {\n\treturn 0\n}\n")],
            "c undone",
        );
        let four = Facts::build(&repo, &change_of(&repo, "main"), 4, Some(&three));
        let text4 = four.render(4, None);
        assert!(facts_shared(&text3, &text4) >= text3.find("\n# This round").unwrap());
        assert!(
            text4.contains("\n## c.go\n\nNo longer part of the change, as of round 4.\n"),
            "{text4}"
        );
        let now: Vec<&str> = four.current().map(|f| f.path.as_str()).collect();
        assert_eq!(now, ["b.go", "a.go"]);
        let _ = fs::remove_dir_all(&repo);
    }

    /// How many bytes two renders share, as the cache would serve them.
    fn facts_shared(a: &str, b: &str) -> usize {
        shared_prefix(a, b)
    }

    #[test]
    fn facts_are_tools_only_deterministic_and_ordered_for_the_cache() {
        let repo = repo_with(&[
            (
                "src/lib.go",
                "package p\n\nfunc Alpha() int {\n\treturn 1\n}\n\nfunc Beta() int {\n\treturn 2\n}\n",
            ),
            (
                "src/use.go",
                "package p\n\nfunc Use() int {\n\treturn Alpha() + Beta()\n}\n",
            ),
            (
                "src/lib_test.go",
                "package p\n\nfunc TestAlpha(t *testing.T) { Alpha() }\n",
            ),
            ("z.go", "package p\n\nfunc Zed() int {\n\treturn 0\n}\n"),
        ]);
        branch(&repo, "work");
        commit(
            &repo,
            &[
                (
                    "src/lib.go",
                    "package p\n\nfunc Alpha() int {\n\treturn 10\n}\n\nfunc Beta() int {\n\treturn 2\n}\n",
                ),
                ("z.go", "package p\n\nfunc Zed() int {\n\treturn 9\n}\n"),
            ],
            "the change",
        );
        let change = change_of(&repo, "main");
        let one = Facts::build(&repo, &change, 1, None);
        let again = Facts::build(&repo, &change, 1, None);
        assert_eq!(one, again, "the same change maps the same");
        let text = one.render(1, Some("ok: 3 passed"));
        assert_eq!(text, again.render(1, Some("ok: 3 passed")), "byte for byte");
        let lib = by_path(&one)["src/lib.go"];
        assert_eq!(lib.symbols, ["Alpha"]);
        assert!(
            lib.references.contains(&"src/use.go:4".to_string()),
            "{:?}",
            lib.references
        );
        assert_eq!(lib.tests, ["src/lib_test.go"]);
        // Nothing that varies sits before the facts: the round and the gate
        // are the tail.
        let tail = text.find("# This round").unwrap();
        assert!(!text[..tail].contains("Round 1") && !text[..tail].contains("3 passed"));
        assert!(
            !text.contains(&repo.display().to_string()),
            "no absolute paths"
        );

        // A fix touches z.go: round 2 shares round 1's facts up to it, and
        // a fact that changed never sits before one that didn't.
        commit(
            &repo,
            &[("z.go", "package p\n\nfunc Zed() int {\n\treturn 8\n}\n")],
            "a fix",
        );
        let fixed = change_of(&repo, "main");
        let two = Facts::build(&repo, &fixed, 2, Some(&one));
        let paths: Vec<&str> = two.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["src/lib.go", "z.go", "z.go"],
            "every earlier section, then z.go again as it is now"
        );
        assert_eq!(two.files[0], one.files[0], "reused as it was");
        assert!(two.files[1].superseded);
        assert_eq!((two.files[0].since, two.files[2].since), (1, 2));
        let text2 = two.render(2, None);
        let shared = shared_prefix(&text, &text2);
        let files_end = text.find("\n# This round").unwrap();
        assert!(
            shared >= files_end,
            "round 2 shares all of round 1's file sections"
        );
        let mut sinces: Vec<u32> = two.files.iter().map(|f| f.since).collect();
        let sorted = {
            let mut s = sinces.clone();
            s.sort_unstable();
            s
        };
        assert_eq!(sinces, sorted);
        sinces.dedup();
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_prefix_is_counted_in_bytes_and_about_tokens() {
        assert_eq!(shared_prefix("abcdef", "abcxyz"), 3);
        assert_eq!(common_prefix(&["abcd", "abce", "abzz"]), 2);
        let p = Prefix::new(1, "reviewers", 12_001);
        assert_eq!(p.tokens, 3001);
        assert_eq!(
            p.line(),
            "shared prefix, reviewers: 12,001 bytes (about 3,001 tokens)"
        );
    }
}
