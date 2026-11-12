//! What's under review, from git. The repository's config and its list of
//! decided non-issues always come from the base, never the change, so a
//! change can't switch off its own review.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;

use super::config::{Generated, ReviewScope};
use super::generated::CONVENTIONS;

#[derive(Debug, Clone, Default)]
pub struct Change {
    /// Every changed file, generated ones too.
    pub files: Vec<String>,
    /// What reviewers get: the diff without generated files, which are
    /// listed at its top.
    pub diff: String,
    /// The whole diff, when files were left out, for a lens that reads them.
    pub full: Option<String>,
    pub generated: Vec<LeftOut>,
    /// Its commits, oldest first, without merges: what it says, commit by
    /// commit, it's for.
    pub commits: Vec<Commit>,
}

/// A commit in a change, with its whole message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub message: String,
}

/// The commits from `base` to `tip`, oldest first, leaving out merges,
/// whose messages say nothing about the change.
pub fn commits(repo: &Path, base: &str, tip: &str) -> Result<Vec<Commit>> {
    let range = format!("{base}..{tip}");
    let out = git(
        repo,
        &[
            "log",
            "--reverse",
            "--no-merges",
            "--format=%h%x1f%B%x1e",
            &range,
        ],
    )?;
    Ok(out
        .split('\x1e')
        .filter_map(|record| {
            let (id, message) = record.trim_start().split_once('\x1f')?;
            Some(Commit {
                id: id.trim().to_string(),
                message: message.trim().to_string(),
            })
        })
        .collect())
}

/// How every role is told what the attached change is: what's said about
/// it, then its diff.
pub const ATTACHED: &str = "The change is the attached file: first what's said about it, \
     claims to check and not to take on trust, then its diff.";

/// The head of what's said about a change, which every role gets ahead of
/// the diff: its description and commit messages, here, then its builder's
/// statement and what earlier commits and the docs say, all under it.
/// Knowing why a change was made saves a reviewer from rediscovering it, so
/// all of it is given, but as claims, never as facts.
pub const SAID: &str = "# What's said about this change: claims to check, not to take on \
     trust\n\n\
     Everything before the diff is someone's account of the change: its own description and \
     commit messages, what its builder says it does, and what earlier commits and the \
     repository's docs say about the files it touches. The comments and docs the change adds \
     are claims too. Use them to learn what the change is for, but none of it is a fact until \
     the code and tests show it. A claim they don't bear out is a finding, and so is a change \
     in behaviour that none of them mention.\n\n";

/// What a change says it's for, in its own words: its description, such as
/// its pull request's, and its commit messages, under [`SAID`].
pub fn intent(description: Option<&str>, commits: &[Commit]) -> String {
    let mut out = String::from(SAID);
    out.push_str("## Its description\n\n");
    match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(text) => {
            out.push_str(text);
            out.push_str("\n\n");
        }
        None => out.push_str("None was given.\n\n"),
    }
    out.push_str("## Its commits, oldest first\n\n");
    if commits.is_empty() {
        out.push_str("None.\n\n");
    }
    for c in commits {
        let message = if c.message.is_empty() {
            "(no message)"
        } else {
            &c.message
        };
        out.push_str(&format!("### {}\n\n{message}\n\n", c.id));
    }
    out
}

/// A generated file left out of the diff a review sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftOut {
    pub path: String,
    pub added: u64,
    pub removed: u64,
    /// What says it's generated.
    pub why: String,
}

pub(super) fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A file as it is on `base`, if it's there.
pub fn at_base(repo: &Path, base: &str, path: &str) -> Result<Option<String>> {
    git(
        repo,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .with_context(|| format!("there's no {base} to read the config from"))?;
    Ok(git(repo, &["show", &format!("{base}:{path}")]).ok())
}

/// The repository's name: its top directory's.
/// The repository's name: its main checkout's folder, so a worktree of it
/// finds the same config.
pub fn name(repo: &Path) -> Result<String> {
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = Path::new(common.trim());
    let main = match common.file_name() {
        Some(n) if n == ".git" => common.parent().unwrap_or(common).to_path_buf(),
        _ => common.to_path_buf(),
    };
    main.file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
        .filter(|n| !n.is_empty())
        .context("the repository has no name")
}

/// What the checkout is now: its commit, and every change and untracked file
/// in it. Only an Orchestrator may change it during a review.
pub fn fingerprint(repo: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for args in [
        &["rev-parse", "HEAD"][..],
        // Untracked folders count once, not by what's in them, so an
        // agent's worktrees under one don't look like a change.
        &["status", "--porcelain", "--untracked-files=normal"],
        &["diff", "HEAD", "--binary"],
    ] {
        hash.update(git(repo, args)?.as_bytes());
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A reviewer's own copy of the checkout at `to`, as it is now, changes and
/// untracked files too, to run and break things in. Its repository is its
/// own: it reads the checkout's history through git's alternates, and has
/// its own index and refs, so nothing done in it reaches the checkout. That
/// holds for a linked worktree too, whose `.git` a plain copy would share.
pub fn copy_of(repo: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    let entries: Vec<_> = fs::read_dir(repo)
        .with_context(|| format!("reading {}", repo.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name() != ".git")
        .map(|e| e.path())
        .collect();
    if !entries.is_empty() {
        // A clone of each file where the file system can make one, which
        // costs next to nothing; a plain copy where it can't.
        let clone: &[&str] = if cfg!(target_os = "macos") {
            &["-c", "-pR"]
        } else {
            &["-a", "--reflink=auto"]
        };
        let copied = |flags: &[&str]| {
            Command::new("cp")
                .args(flags)
                .args(&entries)
                .arg(to)
                .stdin(Stdio::null())
                .output()
        };
        let mut out = copied(clone).context("running cp")?;
        if !out.status.success() && cfg!(target_os = "macos") {
            out = copied(&["-pR"]).context("running cp")?;
        }
        if !out.status.success() {
            bail!(
                "copying {} to {}: {}",
                repo.display(),
                to.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = Path::new(common.trim());
    let head = git(repo, &["rev-parse", "HEAD"])?;
    git(to, &["init", "--quiet"])?;
    let objects = to.join(".git/objects/info");
    fs::create_dir_all(&objects)?;
    fs::write(
        objects.join("alternates"),
        format!("{}\n", common.join("objects").display()),
    )?;
    if common.join("shallow").is_file() {
        fs::copy(common.join("shallow"), to.join(".git/shallow"))?;
    }
    git(to, &["update-ref", "--no-deref", "HEAD", head.trim()])?;
    git(to, &["read-tree", "HEAD"])?;
    // Fills in what the index knows of each file, so its first status is
    // quick. It exits 1 when files differ from HEAD, which they may.
    let _ = Command::new("git")
        .arg("-C")
        .arg(to)
        .args(["update-index", "-q", "--refresh"])
        .stdin(Stdio::null())
        .output();
    Ok(())
}

/// The checkout as it is now, changes and new files too, as a git tree, made
/// with an index of its own so the checkout's index is untouched. Comparing
/// two of these says what changed in between.
pub fn worktree_tree(repo: &Path) -> Result<String> {
    let index = std::env::temp_dir().join(format!("tb-acr-index-{}", crate::util::new_id()));
    let run = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .stdin(Stdio::null())
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let tree = run(&["read-tree", "HEAD"])
        .and_then(|_| run(&["add", "--all"]))
        .and_then(|_| run(&["write-tree"]));
    let _ = fs::remove_file(&index);
    tree
}

/// The files that differ between two trees, and the diff.
pub fn tree_diff(repo: &Path, from: &str, to: &str) -> Result<(Vec<String>, String)> {
    let files = git(repo, &["diff", "--name-only", from, to])?;
    let diff = git(repo, &["diff", from, to])?;
    Ok((files.lines().map(str::to_string).collect(), diff))
}

/// Reviewers read the checkout itself, so the commit a review is of must be
/// the one checked out, with nothing changed on top. That's `head`, or with
/// `bottom-of-stack`, the first commit after the base on the way to it.
/// Without `head`, the review is of the working tree, which is the checkout
/// already.
pub fn checked_out(repo: &Path, base: &str, head: Option<&str>, scope: ReviewScope) -> Result<()> {
    let Some(given) = head else {
        return Ok(());
    };
    let reviewed = reviewed_head(repo, base, head, scope)?.unwrap_or_else(|| given.to_string());
    let commit = |r: &str| -> Result<String> {
        Ok(git(repo, &["rev-parse", &format!("{r}^{{commit}}")])?
            .trim()
            .to_string())
    };
    let short = |s: &str| s[..12.min(s.len())].to_string();
    let want = commit(&reviewed)?;
    let have = git(repo, &["rev-parse", "HEAD"])?.trim().to_string();
    if want != have {
        if want != commit(given)? {
            bail!(
                "review_scope is bottom-of-stack, so the review is of {}, the first commit \
                 after {base} on the way to {given}, and reviewers read the checkout, which is \
                 {}. Check {} out, or pass --review-scope whole-pr to review all of \
                 {base}..{given}.",
                short(&want),
                short(&have),
                short(&want)
            );
        }
        bail!(
            "reviewers read the checkout, so {given} has to be what's checked out, and it's \
             {}. Check {given} out, or leave --head out to review the working tree.",
            short(&have)
        );
    }
    let changed = git(repo, &["status", "--porcelain", "--untracked-files=no"])?;
    if !changed.trim().is_empty() {
        bail!(
            "reviewers read the checkout, which has changes on top of {given}. Commit or \
             stash them, or leave --head out to review the working tree."
        );
    }
    Ok(())
}

/// The commit a review is of: `head`, or with `bottom-of-stack`, the first
/// commit after the base on the way to it. None for the working tree.
pub fn reviewed_head(
    repo: &Path,
    base: &str,
    head: Option<&str>,
    scope: ReviewScope,
) -> Result<Option<String>> {
    Ok(match (scope, head) {
        (ReviewScope::BottomOfStack, Some(head)) => {
            bottom_of_stack(repo, base, head)?.or(Some(head.to_string()))
        }
        (_, head) => head.map(str::to_string),
    })
}

/// The commit a stack's review stops at: the first after the base.
fn bottom_of_stack(repo: &Path, base: &str, head: &str) -> Result<Option<String>> {
    let range = format!("{base}..{head}");
    let commits = git(repo, &["rev-list", "--reverse", &range])?;
    Ok(commits.lines().next().map(str::to_string))
}

/// The change from `base` to `head`, or to the working tree with no `head`.
pub fn load(
    repo: &Path,
    base: &str,
    head: Option<&str>,
    scope: ReviewScope,
    generated: &Generated,
) -> Result<Change> {
    let head = reviewed_head(repo, base, head, scope)?;
    let range = match &head {
        Some(head) => format!("{base}...{head}"),
        None => base.to_string(),
    };
    let names = git(repo, &["diff", "--name-only", &range])?;
    let change = Change {
        files: names.lines().map(str::to_string).collect(),
        diff: git(repo, &["diff", &range])?,
        commits: commits(repo, base, head.as_deref().unwrap_or("HEAD"))?,
        ..Change::default()
    };
    leave_out_generated(repo, base, change, generated)
}

fn globs(paths: &[String]) -> Result<GlobSet> {
    let mut set = GlobSetBuilder::new();
    for path in paths {
        set.add(Glob::new(path)?);
    }
    Ok(set.build()?)
}

/// Files the base's `.gitattributes` marks `linguist-generated`: the
/// base's, so a change can't mark its own files generated.
fn linguist_generated(repo: &Path, base: &str, files: &[String]) -> Vec<String> {
    if files.is_empty() {
        return Vec::new();
    }
    let source = format!("--source={base}");
    let mut args = vec!["check-attr", source.as_str()];
    args.extend(["linguist-generated", "--"]);
    args.extend(files.iter().map(String::as_str));
    let Ok(out) = git(repo, &args) else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| l.rsplit_once(": linguist-generated: "))
        .filter(|(_, v)| matches!(*v, "set" | "true"))
        .map(|(p, _)| p.to_string())
        .collect()
}

/// Each changed file, with the lines it adds and removes, generated ones
/// too: the Sizer's first look at a change.
pub fn diffstat(change: &Change) -> Vec<(String, u64, u64)> {
    per_file(change.full.as_deref().unwrap_or(&change.diff))
        .into_iter()
        .map(|(path, part)| {
            let (added, removed) = counts(&part);
            (path, added, removed)
        })
        .collect()
}

/// How far from a hunk a finding's line may be and still point at it.
const NEAR: u64 = 10;
/// The most hunks one finding brings in, when it names no line.
const HUNKS_PER_FINDING: usize = 6;
/// The most lines of hunks a Judge's brief carries.
const HUNK_LINES: usize = 1500;

/// A path as a finding or a diff may write it, to compare.
fn bare_path(p: &str) -> &str {
    let p = p.trim().trim_start_matches("./");
    let p = p
        .strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p);
    // `src/x.rs:12` names a line too.
    match p.rsplit_once(':') {
        Some((path, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => path,
        _ => p,
    }
}

fn same_file(a: &str, b: &str) -> bool {
    let (a, b) = (bare_path(a), bare_path(b));
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// A hunk: where it starts in the new file, how many lines it spans there,
/// and its text.
type Hunk = (u64, u64, String);

/// A file's hunks.
fn hunks(part: &str) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for line in part.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("@@ ") {
            // `@@ -a,b +c,d @@`: c is the new start, d its length.
            let new = rest.split_whitespace().find(|w| w.starts_with('+'));
            let (start, len) = new
                .map(|w| {
                    let mut it = w[1..].split(',');
                    let start = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let len = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                    (start, len)
                })
                .unwrap_or((0, 0));
            out.push((start, len, line.to_string()));
        } else if let Some((_, _, text)) = out.last_mut() {
            text.push_str(line);
        }
    }
    out
}

/// The diff hunks `findings` point at, for a Judge to start from: for a
/// finding with a file and line, the hunks of that file near it; with a file
/// only, that file's first hunks; with neither, those of the changed files
/// it names. Each hunk keeps the diff's own lines of context. None if they
/// point at nothing in it.
pub fn hunks_for<'a>(
    diff: &str,
    findings: impl IntoIterator<Item = &'a super::roles::Raw>,
) -> Option<String> {
    let files = per_file(diff);
    let parsed: Vec<(String, Vec<Hunk>)> = files
        .iter()
        .map(|(path, part)| (path.clone(), hunks(part)))
        .collect();
    let mut chosen: Vec<(usize, usize)> = Vec::new();
    let mut take = |f: usize, h: usize| {
        if !chosen.contains(&(f, h)) {
            chosen.push((f, h));
        }
    };
    for finding in findings {
        let named: Vec<usize> = match finding.file.as_deref().filter(|f| !f.trim().is_empty()) {
            Some(file) => parsed
                .iter()
                .enumerate()
                .filter(|(_, (path, _))| same_file(path, file))
                .map(|(i, _)| i)
                .collect(),
            None => {
                let text = format!("{} {}", finding.title, finding.detail);
                parsed
                    .iter()
                    .enumerate()
                    .filter(|(_, (path, _))| {
                        let name = path.rsplit('/').next().unwrap_or(path);
                        text.contains(path.as_str()) || (name.contains('.') && text.contains(name))
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        };
        for f in named {
            let near: Vec<usize> = match finding.line {
                Some(line) => parsed[f]
                    .1
                    .iter()
                    .enumerate()
                    .filter(|(_, (start, len, _))| {
                        line + NEAR >= *start && line <= start + len + NEAR
                    })
                    .map(|(h, _)| h)
                    .collect(),
                None => Vec::new(),
            };
            let picked = if near.is_empty() {
                (0..parsed[f].1.len().min(HUNKS_PER_FINDING)).collect()
            } else {
                near
            };
            for h in picked {
                take(f, h);
            }
        }
    }
    if chosen.is_empty() {
        return None;
    }
    chosen.sort();
    let mut out = String::new();
    let mut lines = 0;
    let mut current = None;
    let mut cut = false;
    for (f, h) in chosen {
        let text = &parsed[f].1[h].2;
        let n = text.lines().count();
        if lines + n > HUNK_LINES {
            cut = true;
            break;
        }
        if current != Some(f) {
            out.push_str(&format!("--- {}\n", parsed[f].0));
            current = Some(f);
        }
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
        lines += n;
    }
    if cut {
        out.push_str("(more hunks are in the attached diff)\n");
    }
    Some(out)
}

/// Splits a diff into each file's part, by its `diff --git` line.
fn per_file(diff: &str) -> Vec<(String, String)> {
    let mut parts: Vec<(String, String)> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if let Some(header) = line.strip_prefix("diff --git ") {
            let path = header
                .trim_end()
                .rsplit_once(" b/")
                .map_or(header.trim_end(), |(_, b)| b)
                .to_string();
            parts.push((path, String::new()));
        }
        match parts.last_mut() {
            Some((_, text)) => text.push_str(line),
            None => parts.push((String::new(), line.to_string())),
        }
    }
    parts
}

fn counts(part: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in part.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

/// Takes generated files out of the diff, listing each at its top. Never
/// silently: a hand edit hidden in a generated file still shows up there.
/// Generated means by path: a generator's naming convention, the config's
/// `generated.paths`, or the base's `.gitattributes`. Never by what's in a
/// file, nor anything else the change can set.
fn leave_out_generated(
    repo: &Path,
    base: &str,
    mut change: Change,
    rules: &Generated,
) -> Result<Change> {
    if !rules.exclude || change.files.is_empty() {
        return Ok(change);
    }
    let conventions: Vec<String> = CONVENTIONS.iter().map(|p| p.to_string()).collect();
    let (named, paths, keep) = (
        globs(&conventions)?,
        globs(&rules.paths)?,
        globs(&rules.keep)?,
    );
    let attributed = linguist_generated(repo, base, &change.files);
    let why = |path: &str| -> Option<String> {
        if keep.is_match(path) {
            return None;
        }
        if paths.is_match(path) {
            return Some("generated.paths".into());
        }
        if named.is_match(path) {
            return Some("named like generated code".into());
        }
        attributed
            .iter()
            .any(|p| p == path)
            .then(|| "linguist-generated on the base".into())
    };
    let mut kept = String::new();
    for (path, part) in per_file(&change.diff) {
        match (!path.is_empty()).then(|| why(&path)).flatten() {
            Some(why) => {
                let (added, removed) = counts(&part);
                change.generated.push(LeftOut {
                    path,
                    added,
                    removed,
                    why,
                });
            }
            None => kept.push_str(&part),
        }
    }
    if change.generated.is_empty() {
        return Ok(change);
    }
    let mut diff = String::from(
        "# Generated files are left out of this diff, with their lines added and removed.\n\
         # Each is in the repository, to read if a finding needs it.\n",
    );
    for g in &change.generated {
        diff.push_str(&format!(
            "#   {} (+{} -{}): {}\n",
            g.path, g.added, g.removed, g.why
        ));
    }
    diff.push('\n');
    diff.push_str(&kept);
    change.full = Some(std::mem::replace(&mut change.diff, diff));
    Ok(change)
}

impl Change {
    /// Lines the change adds or removes, without the file headers or the
    /// generated files left out.
    pub fn changed_lines(&self) -> impl Iterator<Item = &str> {
        self.diff.lines().filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
    }
}

#[cfg(test)]
pub mod tests {
    use std::fs;
    use std::path::PathBuf;

    use crate::acr::config::REPO_CONFIG;

    use super::*;

    #[test]
    fn a_judge_gets_the_hunks_its_findings_point_at() {
        use super::super::roles::Raw;
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n\
@@ -1,3 +1,4 @@\n ctx a1\n+added near the top\n ctx a2\n ctx a3\n\
@@ -100,3 +101,4 @@\n ctx a100\n+added far down\n ctx a101\n ctx a102\n\
diff --git a/docs/b.md b/docs/b.md\n--- a/docs/b.md\n+++ b/docs/b.md\n\
@@ -5,2 +5,3 @@\n ctx b5\n+a doc line\n ctx b6\n";
        let raw = |file: Option<&str>, line: Option<u64>, detail: &str| Raw {
            title: "t".into(),
            detail: detail.into(),
            file: file.map(str::to_string),
            line,
        };
        // A file and a line: only the hunk near it, with its context.
        let near = hunks_for(diff, &[raw(Some("src/a.rs"), Some(102), "")]).unwrap();
        assert!(
            near.contains("--- src/a.rs") && near.contains("+added far down"),
            "{near}"
        );
        assert!(
            near.contains(" ctx a100") && near.contains(" ctx a102"),
            "{near}"
        );
        assert!(
            !near.contains("added near the top") && !near.contains("a doc line"),
            "{near}"
        );
        // A file, however written, with no line: its hunks.
        let file = hunks_for(diff, &[raw(Some("./src/a.rs:7"), None, "")]).unwrap();
        assert!(
            file.contains("near the top") && file.contains("far down"),
            "{file}"
        );
        // Neither: the changed files it names.
        let named = hunks_for(diff, &[raw(None, None, "the wording in b.md is wrong")]).unwrap();
        assert!(
            named.contains("--- docs/b.md") && named.contains("+a doc line"),
            "{named}"
        );
        assert!(!named.contains("src/a.rs"), "{named}");
        // Several findings, each hunk once.
        let both = hunks_for(
            diff,
            &[
                raw(Some("src/a.rs"), Some(2), ""),
                raw(Some("src/a.rs"), Some(1), ""),
            ],
        )
        .unwrap();
        assert_eq!(both.matches("+added near the top").count(), 1, "{both}");
        // Nothing it can place.
        assert!(hunks_for(diff, &[raw(None, None, "a general worry")]).is_none());
    }
    use crate::util::new_id;

    /// A git repository with one commit of `files` on `main`.
    #[test]
    fn two_worktree_trees_say_what_changed_between_them() {
        let repo = repo_with(&[("a.txt", "one\n"), ("b.txt", "b\n")]);
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        let before = fingerprint(&repo).unwrap();
        let first = worktree_tree(&repo).unwrap();
        fs::write(repo.join("a.txt"), "three\n").unwrap();
        fs::write(repo.join("c.txt"), "new\n").unwrap();
        let second = worktree_tree(&repo).unwrap();
        let (files, diff) = tree_diff(&repo, &first, &second).unwrap();
        assert_eq!(files, ["a.txt", "c.txt"]);
        assert!(diff.contains("-two") && diff.contains("+three"));
        // Its own index: the checkout's is as it was, but for the new file.
        fs::remove_file(repo.join("c.txt")).unwrap();
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        assert_eq!(fingerprint(&repo).unwrap(), before);
    }

    #[test]
    fn a_reviewers_copy_is_the_checkout_as_it_is_and_reaches_nothing_back() {
        let repo = repo_with(&[("a.txt", "one\n"), ("dir/b.txt", "b\n")]);
        let base = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        commit(&repo, &[("a.txt", "two\n")], "the change");
        let head = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        // A fix on top, not committed, and a new file.
        fs::write(repo.join("a.txt"), "three\n").unwrap();
        fs::write(repo.join("new.txt"), "new\n").unwrap();
        let before = fingerprint(&repo).unwrap();
        let to = std::env::temp_dir()
            .join(format!("tb-acr-copy-{}", new_id()))
            .join("repo");
        copy_of(&repo, &to).unwrap();
        assert_eq!(git(&to, &["rev-parse", "HEAD"]).unwrap(), head);
        assert_eq!(fs::read_to_string(to.join("a.txt")).unwrap(), "three\n");
        assert_eq!(fs::read_to_string(to.join("dir/b.txt")).unwrap(), "b\n");
        assert!(to.join("new.txt").is_file());
        // Its history is there, through the checkout's objects.
        let range = format!("{}..HEAD", base.trim());
        let diff = git(&to, &["diff", "--stat", &range]).unwrap();
        assert!(diff.contains("a.txt"), "{diff}");
        assert!(git(&to, &["diff", "HEAD"]).unwrap().contains("+three"));
        // What's done in it stays in it.
        fs::write(to.join("a.txt"), "broken\n").unwrap();
        git(&to, &["add", "--all"]).unwrap();
        git(
            &to,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "mutation",
            ],
        )
        .unwrap();
        assert_eq!(fingerprint(&repo).unwrap(), before);
        assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "three\n");
        let _ = fs::remove_dir_all(to.parent().unwrap());
    }

    pub fn repo_with(files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-acr-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        commit(&dir, files, "first");
        dir
    }

    pub fn commit(dir: &Path, files: &[(&str, &str)], message: &str) {
        for (path, text) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", message]] {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
        }
    }

    #[test]
    fn a_change_carries_its_own_commit_messages_oldest_first() {
        let dir = repo_with(&[("a.txt", "one\n")]);
        commit(
            &dir,
            &[("a.txt", "two\n")],
            "Fix B-1: keep the edit\n\nThe save path dropped it.",
        );
        commit(&dir, &[("b.txt", "new\n")], "Test the edit");
        let change = load(
            &dir,
            "main~2",
            Some("main"),
            ReviewScope::WholePr,
            &Generated::default(),
        )
        .unwrap();
        let messages: Vec<&str> = change.commits.iter().map(|c| c.message.as_str()).collect();
        // The base's own commit isn't the change's.
        assert_eq!(
            messages,
            [
                "Fix B-1: keep the edit\n\nThe save path dropped it.",
                "Test the edit"
            ]
        );
        assert!(change.commits.iter().all(|c| c.id.len() >= 7));

        let text = intent(Some("Fixes B-1.\n"), &change.commits);
        let fix = text.find("Fix B-1: keep the edit").unwrap();
        assert!(
            text.contains("## Its description\n\nFixes B-1.\n"),
            "{text}"
        );
        assert!(fix < text.find("Test the edit").unwrap(), "{text}");
        assert!(
            text.starts_with(SAID) && SAID.contains("claims to check, not to take on trust"),
            "{text}"
        );
        let bare = intent(None, &[]);
        assert!(
            bare.contains("None was given.") && bare.contains("None.\n"),
            "{bare}"
        );
    }

    #[test]
    fn generated_files_are_left_out_and_listed() {
        // The base says vendor/ is generated; the change can't add to that.
        let dir = repo_with(&[
            ("a.txt", "one\n"),
            (".gitattributes", "vendor/** linguist-generated\n"),
        ]);
        branch(&dir, "work");
        commit(
            &dir,
            &[
                ("a.txt", "two\n"),
                ("lib/db.g.dart", "line\nline\nline\n"),
                ("app/target/out.bin.txt", "built\n"),
                ("vendor/v.txt", "plain\n"),
                ("data/big.csv", "1,2\n"),
                // Claims to be generated, but a file's contents never decide.
                (
                    "src/sneaky.txt",
                    "// Code generated by x. DO NOT EDIT.\nhand edit\n",
                ),
                // Nor does the change's own .gitattributes.
                ("mine/m.txt", "mine\n"),
                (
                    ".gitattributes",
                    "vendor/** linguist-generated\nmine/** linguist-generated\n",
                ),
            ],
            "work",
        );
        let rules = Generated {
            paths: vec!["data/**".into()],
            ..Generated::default()
        };
        let c = load(&dir, "main", Some("work"), ReviewScope::WholePr, &rules).unwrap();
        let out: Vec<(&str, &str)> = c
            .generated
            .iter()
            .map(|g| (g.path.as_str(), g.why.as_str()))
            .collect();
        assert_eq!(
            out,
            [
                ("app/target/out.bin.txt", "named like generated code"),
                ("data/big.csv", "generated.paths"),
                ("lib/db.g.dart", "named like generated code"),
                ("vendor/v.txt", "linguist-generated on the base"),
            ]
        );
        let db = c
            .generated
            .iter()
            .find(|g| g.path == "lib/db.g.dart")
            .unwrap();
        assert_eq!((db.added, db.removed), (3, 0));
        // Listed at the top, never dropped silently, and still changed files.
        assert!(
            c.diff
                .contains("#   lib/db.g.dart (+3 -0): named like generated code")
        );
        assert!(!c.diff.contains("+line"));
        assert!(
            c.diff.contains("+two") && c.diff.contains("+hand edit") && c.diff.contains("+mine")
        );
        assert!(c.full.as_ref().unwrap().contains("+line"));
        assert_eq!(c.files.len(), 8);
        // A repository can keep one, or turn it all off.
        let keep = Generated {
            keep: vec!["lib/**".into()],
            ..Generated::default()
        };
        let kept = load(&dir, "main", Some("work"), ReviewScope::WholePr, &keep).unwrap();
        assert!(kept.generated.iter().all(|g| g.path != "lib/db.g.dart"));
        let off = Generated {
            exclude: false,
            ..Generated::default()
        };
        let all = load(&dir, "main", Some("work"), ReviewScope::WholePr, &off).unwrap();
        assert!(all.generated.is_empty() && all.full.is_none());
        // The working tree is read the same way.
        let tree = load(&dir, "main", None, ReviewScope::WholePr, &rules).unwrap();
        assert_eq!(tree.generated.len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_worktree_has_its_repositorys_name() {
        let dir = repo_with(&[("a.txt", "one\n")]);
        let tree = dir.with_file_name(format!("pr-{}", new_id()));
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["worktree", "add", "-q", "-b", "pr"])
            .arg(&tree)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(name(&tree).unwrap(), name(&dir).unwrap());
        assert!(name(&dir).unwrap().starts_with("tb-acr-"));
        fs::remove_dir_all(&tree).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reviewed_commit_must_be_what_is_checked_out() {
        let dir = repo_with(&[("a.txt", "one\n")]);
        branch(&dir, "change");
        commit(&dir, &[("a.txt", "two\n")], "two");
        // The working tree is always fine: it's the checkout.
        let whole = |head| checked_out(&dir, "main", head, ReviewScope::WholePr);
        assert!(whole(None).is_ok());
        assert!(whole(Some("change")).is_ok());
        let err = format!("{:#}", whole(Some("main")).unwrap_err());
        assert!(err.contains("checked out"), "{err}");
        fs::write(dir.join("a.txt"), "three\n").unwrap();
        let err = format!("{:#}", whole(Some("change")).unwrap_err());
        assert!(err.contains("changes on top"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stacks_review_is_of_its_bottom_commit_which_must_be_checked_out() {
        // Reviewers were handed the bottom commit's diff while they read the
        // top of the stack, so they argued with lines that weren't there.
        let dir = repo_with(&[("a.txt", "one\n")]);
        branch(&dir, "change");
        commit(&dir, &[("a.txt", "two\n")], "bottom");
        let bottom = git(&dir, &["rev-parse", "HEAD"]).unwrap();
        commit(&dir, &[("a.txt", "three\n")], "top");
        let stack = |head| checked_out(&dir, "main", Some(head), ReviewScope::BottomOfStack);
        let err = format!("{:#}", stack("change").unwrap_err());
        assert!(
            err.contains("bottom-of-stack") && err.contains(&bottom.trim()[..12]),
            "{err}"
        );
        assert!(err.contains("--review-scope whole-pr"), "{err}");
        let at = |r: &str| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["checkout", "-q", r])
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        at(bottom.trim());
        assert!(stack("change").is_ok());
        // The diff it's handed is the one checked out.
        let change = load(
            &dir,
            "main",
            Some("change"),
            ReviewScope::BottomOfStack,
            &Generated::default(),
        )
        .unwrap();
        let lines: Vec<&str> = change.changed_lines().collect();
        assert!(
            lines.contains(&"+two") && !lines.contains(&"+three"),
            "{lines:?}"
        );
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        // A stack of one is its own bottom.
        at("change");
        assert!(checked_out(&dir, "change~1", Some("change"), ReviewScope::BottomOfStack).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    pub fn branch(dir: &Path, name: &str) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["checkout", "-q", "-b", name])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn the_config_comes_from_the_base_never_the_change() {
        let repo = repo_with(&[(REPO_CONFIG, "base config"), ("a.txt", "one\n")]);
        branch(&repo, "work");
        commit(&repo, &[(REPO_CONFIG, "head config")], "switch it off");
        let config = |repo: &Path, base: &str| at_base(repo, base, REPO_CONFIG);
        assert_eq!(config(&repo, "main").unwrap().unwrap(), "base config");
        assert_eq!(config(&repo, "work").unwrap().unwrap(), "head config");
        // A config that only the change has doesn't count.
        let bare = repo_with(&[("a.txt", "one\n")]);
        branch(&bare, "work");
        commit(&bare, &[(REPO_CONFIG, "brought along")], "bring my own");
        assert_eq!(config(&bare, "main").unwrap(), None);
        assert!(config(&repo, "no-such-ref").is_err());
    }

    #[test]
    fn reads_a_change_whole_or_from_the_bottom_of_its_stack() {
        let repo = repo_with(&[("a.txt", "one\n")]);
        branch(&repo, "work");
        commit(&repo, &[("b.txt", "two\n")], "first of the stack");
        commit(&repo, &[("c.txt", "three\n")], "second of the stack");
        let whole = load(
            &repo,
            "main",
            Some("work"),
            ReviewScope::WholePr,
            &Generated::default(),
        )
        .unwrap();
        assert_eq!(whole.files, ["b.txt", "c.txt"]);
        let bottom = load(
            &repo,
            "main",
            Some("work"),
            ReviewScope::BottomOfStack,
            &Generated::default(),
        )
        .unwrap();
        assert_eq!(bottom.files, ["b.txt"]);
        assert_eq!(bottom.changed_lines().collect::<Vec<_>>(), ["+two"]);
        fs::write(repo.join("a.txt"), "changed\n").unwrap();
        let tree = load(
            &repo,
            "main",
            None,
            ReviewScope::WholePr,
            &Generated::default(),
        )
        .unwrap();
        assert!(tree.files.contains(&"a.txt".to_string()));
    }
}
