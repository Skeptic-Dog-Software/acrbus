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
//! The order is the cache's. A prompt cache hits on an identical prefix, so
//! what stays the same comes first and what changes comes last: files a fix
//! hasn't touched since they were first mapped, in path order, then files
//! touched in each later round, oldest touch first, then this round's gate.
//! Nothing that varies between rounds or calls, no round number, count of
//! calls, time or path of a copy, sits before a fact that doesn't.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::Result;
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
}

/// A round's facts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub files: Vec<FileFacts>,
}

impl Facts {
    /// Builds the facts for `change` at round `round`, in the checkout at
    /// `repo`, reusing `prior`'s for every file whose content hasn't
    /// changed: their bytes then stay the same from round to round.
    pub fn build(repo: &Path, change: &Change, round: u32, prior: Option<&Facts>) -> Facts {
        let left_out: BTreeSet<&str> = change.generated.iter().map(|g| g.path.as_str()).collect();
        let mut files = Vec::new();
        let mut searches = MAX_SEARCHES;
        for (path, part) in change::per_file(&change.diff) {
            if path.is_empty() || left_out.contains(path.as_str()) {
                continue;
            }
            let blob = blob_of(repo, &path);
            if let Some(old) = prior
                .and_then(|p| p.files.iter().find(|f| f.path == path))
                .filter(|f| f.blob == blob && f.diff == part)
            {
                files.push(old.clone());
                continue;
            }
            files.push(map_file(repo, &path, &part, &blob, round, &mut searches));
        }
        files.sort_by(|a, b| (a.since, &a.path).cmp(&(b.since, &b.path)));
        Facts { files }
    }

    /// The facts as every role gets them: the same bytes for every call in a
    /// round, and the same as the last round's up to the first file a fix
    /// touched. `gate` is this round's test gate, if it ran, and goes last,
    /// after the only line that names the round.
    pub fn render(&self, round: u32, gate: Option<&str>) -> String {
        let mut out = String::from(HEAD);
        for f in &self.files {
            out.push_str(&render_file(f));
        }
        out.push_str(&format!("\n# This round\n\nRound {round}.\n"));
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
    as of the round it was first mapped; files a fix touched come after the rest, with their \
    own. The change and its claims are also in the attached file.\n";

fn render_file(f: &FileFacts) -> String {
    let mut out = format!("\n## {}\n\n", f.path);
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

/// Facts' files by path, for a test or a report.
#[cfg(test)]
pub fn by_path(facts: &Facts) -> std::collections::BTreeMap<&str, &FileFacts> {
    facts.files.iter().map(|f| (f.path.as_str(), f)).collect()
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
        assert_eq!(paths, ["src/lib.go", "z.go"], "untouched first");
        assert_eq!(two.files[0], one.files[0], "reused as it was");
        assert_eq!((two.files[0].since, two.files[1].since), (1, 2));
        let text2 = two.render(2, None);
        let shared = shared_prefix(&text, &text2);
        let z_at = text.find("\n## z.go").unwrap();
        assert!(
            shared >= z_at,
            "round 2 shares round 1's prefix up to the first touched file"
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
