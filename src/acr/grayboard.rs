//! A review on a Grayboard team server: its questions reach you, through
//! your `human` handle and Grayboard's Slack app, and your reply answers
//! them; and a claim on its change keeps a second review of it from
//! starting, on any machine, while this one runs or waits.
//!
//! A review that pauses or ends leaves a note in the outbox. The daemon,
//! which holds the one hosted session (`crate::grayboard`), sends each
//! note, reads your replies, resumes the review with your answers in the
//! background, and renews or releases its claims. Nothing here is needed
//! for a review to run: without `[grayboard]` in config.toml, none of it
//! happens, and when the server is away, reviews go on without it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::carousel::{self, Question};
use super::config::{Config, Share};
use super::flow::{FindingState, Gate, Run, Status};
use crate::config::{APP_NAME, Paths, app};
use crate::grayboard::{self as gb, Session, Trouble, write_private};
use crate::util::{now_ms, stamped};

/// How long a claim lasts each time it's taken or renewed.
const CLAIM_MINUTES: i64 = 60;
/// A claim this close to its end is renewed.
const RENEW_WITHIN_MS: i64 = 20 * 60_000;
/// Set on a resume the daemon starts, so the pause it ends at says so.
const RESUMED: &str = "GRAYBOARD_RESUMED";

fn outbox(paths: &Paths) -> PathBuf {
    gb::dir(paths).join("outbox")
}

fn sent(paths: &Paths) -> PathBuf {
    gb::dir(paths).join("sent")
}

fn pending_dir(paths: &Paths) -> PathBuf {
    gb::dir(paths).join("pending")
}

fn claims_path(paths: &Paths) -> PathBuf {
    gb::dir(paths).join("claims.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// Questions for you.
    Gate,
    /// Waiting for its builder session's fixes.
    Fix,
    /// Finished.
    End,
}

/// What a review left for the daemon to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    run: String,
    kind: Kind,
    repo: String,
    text: String,
    /// The questions as sent, to read a reply against.
    #[serde(default)]
    questions: Vec<Question>,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    nudge: bool,
    #[serde(default)]
    starter: Option<String>,
    /// It paused on a resume the daemon started from your answers.
    #[serde(default)]
    resumed: bool,
    at: i64,
}

/// A note sent, waiting for your reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    note: Note,
    message_id: i64,
    sent_at: i64,
    /// The reply it was answered with.
    #[serde(default)]
    answered: Option<i64>,
}

/// Whether config.toml has a `[grayboard]` section.
pub fn configured(paths: &Paths) -> bool {
    gb::settings(paths).is_some()
}

/// The questions' fingerprint: a reply to older ones is stale.
fn fingerprint(run: &Run, ask: &carousel::Ask) -> String {
    let mut h = Sha256::new();
    h.update(run.id.as_bytes());
    h.update(run.round.to_le_bytes());
    h.update(ask.gate.unwrap_or("").as_bytes());
    h.update(serde_json::to_vec(&ask.carousels).unwrap_or_default());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn questions(ask: &carousel::Ask) -> Vec<Question> {
    ask.carousels
        .iter()
        .flat_map(|c| c.questions.iter().cloned())
        .collect()
}

/// The questions as a message to read on a phone, and how to answer.
fn gate_text(run: &Run, repo: &str, ask: &carousel::Ask) -> String {
    let qs = questions(ask);
    let mut out = format!(
        "Review {} of {repo} is waiting for you at its {} gate, round {}: {} question(s).\n",
        run.id,
        ask.gate.unwrap_or("next"),
        run.round,
        qs.len()
    );
    for (n, q) in qs.iter().enumerate() {
        out.push_str(&format!("\n{}. [{}] {}\n", n + 1, q.header, q.question));
        for (i, o) in q.options.iter().enumerate() {
            out.push_str(&format!("   {}) {}: {}\n", i + 1, o.label, o.description));
        }
        if q.multi_select {
            out.push_str("   Choose any, with commas.\n");
        }
    }
    out.push_str(
        "\nReply in this thread, a line per question: its number, then the option's number \
         or name, as \"1: 2\" or \"2: Fix it\". Where a question takes your own words, write \
         them after its number.",
    );
    out
}

/// What the end of a review says: how it ended, and in brief what it did.
fn end_text(run: &Run, repo: &str, full: bool) -> String {
    let how = if run.status == Status::Confirmed {
        "confirmed"
    } else {
        "stopped"
    };
    let mut out = format!(
        "Review {} of {repo} is over: {how}, after {} round(s).",
        run.id, run.round
    );
    if full {
        let mut last: BTreeMap<(String, String), FindingState> = BTreeMap::new();
        for lens in run.lenses.values() {
            for round in &lens.rounds {
                for f in &round.findings {
                    last.insert((lens.id.clone(), f.id.clone()), f.state);
                }
            }
        }
        let count = |s: FindingState| last.values().filter(|v| **v == s).count();
        out.push_str(&format!(
            " {} finding(s) fixed, {} waived, {} logged, {} still open.",
            count(FindingState::Folded),
            count(FindingState::Waived),
            count(FindingState::Logged),
            count(FindingState::Open),
        ));
    }
    out
}

/// Leaves a note for the daemon as a review pauses or ends, when Grayboard
/// is configured. Never fails the review: a note that can't be written is
/// said, and the review goes on.
pub fn on_finish(paths: &Paths, run: &Run, cfg: &Config) {
    if !configured(paths) {
        return;
    }
    if let Err(e) = note(paths, run, cfg) {
        eprintln!(
            "{APP_NAME}: couldn't leave review {}'s note for Grayboard: {e:#}",
            run.id
        );
    }
}

fn note(paths: &Paths, run: &Run, cfg: &Config) -> Result<()> {
    let repo = super::change::name(&run.repo)
        .ok()
        .or_else(|| {
            run.repo
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "the repository".into());
    let nudge = cfg.integrations.grayboard == Some(Share::Nudge);
    let resumed = crate::config::env_var(RESUMED).is_some();
    let base = |kind: Kind, text: String| Note {
        run: run.id.clone(),
        kind,
        repo: repo.clone(),
        text,
        questions: Vec::new(),
        fingerprint: None,
        nudge,
        starter: run.starter.clone(),
        resumed,
        at: now_ms(),
    };
    let note = match (run.status, run.waiting) {
        (Status::Waiting, Some(Gate::Fix)) => base(
            Kind::Fix,
            format!(
                "Review {} of {repo} waits for its builder session{} to fold in the fixes \
                 chosen, then `{APP_NAME} acr resume {} --fixed`.",
                run.id,
                run.starter
                    .as_deref()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default(),
                run.id
            ),
        ),
        (Status::Waiting, _) => {
            let ask = carousel::ask(run, cfg, &APP_NAME.to_string());
            if ask.carousels.is_empty() {
                return Ok(());
            }
            if nudge {
                base(
                    Kind::Gate,
                    format!(
                        "Review {} of {repo} is waiting for you at its {} gate: {} \
                         question(s). They stay on its machine: answer there with \
                         `{APP_NAME} acr ask {}`.",
                        run.id,
                        ask.gate.unwrap_or("next"),
                        questions(&ask).len(),
                        run.id
                    ),
                )
            } else {
                Note {
                    questions: questions(&ask),
                    fingerprint: Some(fingerprint(run, &ask)),
                    ..base(Kind::Gate, gate_text(run, &repo, &ask))
                }
            }
        }
        (Status::Confirmed | Status::Stopped, _) => base(Kind::End, end_text(run, &repo, !nudge)),
        _ => return Ok(()),
    };
    let kind = match note.kind {
        Kind::Gate => "gate",
        Kind::Fix => "fix",
        Kind::End => "end",
    };
    let path = outbox(paths).join(stamped(&format!("{}-{kind}.json", run.id)));
    write_private(&path, &serde_json::to_vec_pretty(&note)?)
}

// The daemon's side.

/// One pass: send what's waiting, read your replies, keep the claims.
pub fn tick(paths: &Paths, s: &mut Session, log: &dyn Fn(String)) -> Result<()> {
    send_notes(paths, s, log)?;
    read_replies(paths, s, log)?;
    keep_claims(paths, s, log)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(value)?)
}

/// Each `.json` in `dir`, oldest first by name.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    out.sort();
    out
}

fn dm(s: &mut Session, body: &str, parent: Option<i64>) -> Result<i64> {
    let mut args = json!({ "to": "human", "body": body });
    if let Some(p) = parent {
        args["parent_id"] = json!(p);
    }
    let r = s.call("dm_send", args)?;
    r["message_id"]
        .as_i64()
        .context("the server didn't say which message it sent")
}

/// Tells a review's builder session something, over this machine's bus.
fn tell(paths: &Paths, to: &str, body: String) -> Result<()> {
    let _: Value = crate::local::call(
        &paths.socket(),
        &crate::local::Request::Send {
            agent: super::SENDER.into(),
            to: to.into(),
            body,
            reply_to: None,
            expects_reply: Some(false),
            pipe: None,
            ext: None,
        },
    )?;
    Ok(())
}

fn send_notes(paths: &Paths, s: &mut Session, log: &dyn Fn(String)) -> Result<()> {
    for path in files(&outbox(paths)) {
        let Some(note) = read_json::<Note>(&path) else {
            continue;
        };
        let id = dm(s, &note.text, None)?;
        match note.kind {
            Kind::Gate => {
                let pending = Pending {
                    note: note.clone(),
                    message_id: id,
                    sent_at: now_ms(),
                    answered: None,
                };
                save_json(
                    &pending_dir(paths).join(format!("{}.json", note.run)),
                    &pending,
                )?;
            }
            Kind::Fix => {
                if note.resumed
                    && let Some(starter) = &note.starter
                    && let Err(e) = tell(
                        paths,
                        starter,
                        format!(
                            "Review {} waits for you: your user's answers chose fixes for you to \
                             fold in. `{APP_NAME} acr ask {}` says what, then `{APP_NAME} acr \
                             resume {} --fixed`.",
                            note.run, note.run, note.run
                        ),
                    )
                {
                    log(format!(
                        "grayboard: couldn't tell {starter} about review {}: {e:#}",
                        note.run
                    ));
                }
                answered_out(paths, &note.run);
            }
            Kind::End => answered_out(paths, &note.run),
        }
        // Kept, to be looked at, until the daemon's cleanup.
        let to = sent(paths).join(path.file_name().unwrap_or_default());
        fs::create_dir_all(sent(paths))?;
        fs::rename(&path, &to)?;
        let kind = format!("{:?}", note.kind).to_lowercase();
        log(format!("grayboard: sent review {}'s {kind} note", note.run));
    }
    Ok(())
}

/// A review that's moved past its questions has none pending.
fn answered_out(paths: &Paths, run: &str) {
    let path = pending_dir(paths).join(format!("{run}.json"));
    if path.exists() {
        let done = pending_dir(paths).join("done");
        let _ = fs::create_dir_all(&done);
        let _ = fs::rename(&path, done.join(stamped(&format!("{run}.json"))));
    }
}

/// One message in `dm_inbox`'s `unanswered`.
#[derive(Debug, Deserialize)]
struct Incoming {
    id: i64,
    from: String,
    #[serde(default)]
    parent_id: Option<i64>,
    #[serde(default)]
    body: String,
}

fn read_replies(paths: &Paths, s: &mut Session, log: &dyn Fn(String)) -> Result<()> {
    let inbox = s.call("dm_inbox", json!({}))?;
    let unanswered: Vec<Incoming> =
        serde_json::from_value(inbox["unanswered"].clone()).unwrap_or_default();
    let Some(principal) = s.principal.clone() else {
        return Ok(());
    };
    let human = format!("{principal}/human");
    for m in unanswered {
        if m.from == human {
            let said = answer(paths, &m);
            let parent = m.parent_id.unwrap_or(m.id);
            dm(s, &said, Some(parent))?;
            log(format!(
                "grayboard: answered message {} from your human handle",
                m.id
            ));
        } else {
            log(format!(
                "grayboard: ignored a message from {}: only your own human handle answers reviews",
                m.from
            ));
        }
        s.call("dm_ack", json!({ "message_id": m.id }))?;
    }
    Ok(())
}

/// The review a reply is about: the one whose note it replies to, the one it
/// names, or the only one waiting.
fn which(paths: &Paths, m: &Incoming) -> Result<Pending, String> {
    let all: Vec<Pending> = files(&pending_dir(paths))
        .iter()
        .filter_map(|p| read_json(p))
        .collect();
    if let Some(p) = m.parent_id
        && let Some(found) = all.iter().find(|x| x.message_id == p)
    {
        return Ok(found.clone());
    }
    let first = m
        .body
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if first.len() >= 6 {
        let named: Vec<&Pending> = all
            .iter()
            .filter(|x| x.note.run.starts_with(&first))
            .collect();
        if named.len() == 1 {
            return Ok(named[0].clone());
        }
    }
    match all.len() {
        0 => Err("No review is waiting for answers here.".into()),
        1 => Ok(all[0].clone()),
        _ => Err(format!(
            "Several reviews are waiting: {}. Reply in a review's thread, or start your reply \
             with its ID.",
            all.iter()
                .map(|p| p.note.run.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What to say back to a reply, having acted on it.
fn answer(paths: &Paths, m: &Incoming) -> String {
    let pending = match which(paths, m) {
        Ok(p) => p,
        Err(why) => return why,
    };
    let run_id = pending.note.run.clone();
    if pending.note.nudge {
        return format!(
            "Review {run_id}'s questions stay on its machine, so I can't take answers to them \
             here: answer there with `{APP_NAME} acr ask {run_id}`."
        );
    }
    match apply_reply(paths, &pending, m) {
        Ok(said) => said,
        Err(e) => format!("Review {run_id}: {e:#}"),
    }
}

fn apply_reply(paths: &Paths, pending: &Pending, m: &Incoming) -> Result<String> {
    let run_id = &pending.note.run;
    if super::keep::is_finished(paths, run_id) {
        bail!("it has finished, so there's nothing to answer.");
    }
    if super::keep::active(paths).contains(run_id) {
        bail!("it's running now; I'll send its questions if it waits again.");
    }
    let dir = super::run_dir(paths, run_id);
    let (Ok(run), Ok(cfg)) = (
        fs::read(dir.join("run.json")),
        fs::read(dir.join("config.json")),
    ) else {
        bail!("there's no record of it on this machine.");
    };
    let run: Run = serde_json::from_slice(&run)?;
    let cfg = super::stored_config(&cfg)?;
    let ask = carousel::ask(&run, &cfg, &APP_NAME.to_string());
    if run.status != Status::Waiting || Some(fingerprint(&run, &ask)) != pending.note.fingerprint {
        bail!(
            "it has moved on since those questions, so these answers are stale. I'll send its new questions if it waits again."
        );
    }
    if pending.answered.is_some() {
        bail!("those questions were answered already.");
    }
    let body = strip_id(&m.body, run_id);
    let answers = parse(&pending.note.questions, &body).map_err(|e| anyhow::anyhow!("{e}"))?;
    // Tried on a copy first: a mistake is said, and nothing changes.
    let mut trial = run.clone();
    carousel::apply(&mut trial, &cfg, &answers, now_ms())?;
    let file = gb::dir(paths)
        .join("answers")
        .join(stamped(&format!("{run_id}-{}.json", m.id)));
    write_private(&file, &serde_json::to_vec_pretty(&answers)?)?;
    let mut p = pending.clone();
    p.answered = Some(m.id);
    save_json(&pending_dir(paths).join(format!("{run_id}.json")), &p)?;
    let command = format!(
        "{APP_NAME} acr resume {run_id} --answers @{}",
        file.display()
    );
    if super::flow::session_fixes(&trial) {
        let Some(starter) = &run.starter else {
            return Ok(format!(
                "Your answers choose fixes for its builder session, which I don't know. From \
                 that session: `{command}`."
            ));
        };
        tell(
            paths,
            starter,
            format!(
                "Your user answered review {run_id}'s questions on their phone, choosing fixes \
                 for you to fold in. Apply them with `{command}`, then fix, then `{APP_NAME} acr \
                 resume {run_id} --fixed`."
            ),
        )?;
        return Ok(format!(
            "Your answers choose fixes for its builder session, so I've passed them to \
             {starter}, which applies them and fixes."
        ));
    }
    resume(paths, run_id, &file)?;
    Ok("Resuming it with your answers. I'll write when it waits again or ends.".to_string())
}

/// A reply may start with the review's ID, or a unique start of it.
fn strip_id(body: &str, run: &str) -> String {
    let mut lines: Vec<&str> = body.lines().collect();
    if let Some(first) = lines.first() {
        let word = first.split_whitespace().next().unwrap_or("");
        if word.len() >= 6 && run.starts_with(&word.to_ascii_uppercase()) {
            let rest = first[word.len()..]
                .trim_start_matches([' ', ':', ',', '-'])
                .trim();
            if rest.is_empty() {
                lines.remove(0);
            } else {
                lines[0] = rest;
                return lines.join("\n");
            }
        }
    }
    lines.join("\n")
}

/// Answers, keyed by question text as `acr resume --answers` takes them,
/// from a reply such as "1: 2\n2: Fix it\n3: 1, 3". One question takes a
/// bare answer. A number is an option; text naming an option, or the start
/// of one, is it; other text is the reply's own words.
pub fn parse(qs: &[Question], body: &str) -> Result<BTreeMap<String, Value>, String> {
    let mut out = BTreeMap::new();
    let mut unread = Vec::new();
    let line_re = regex::Regex::new(r"^\s*(\d{1,2})\s*[:.)\]-]\s*(.*)$").expect("regex");
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut pairs: Vec<(usize, String)> = Vec::new();
    for line in &lines {
        match line_re.captures(line) {
            Some(c) => pairs.push((c[1].parse().unwrap_or(0), c[2].trim().to_string())),
            None if qs.len() == 1 && lines.len() == 1 => pairs.push((1, line.to_string())),
            None => match pairs.last_mut() {
                // A line of its own words that goes on from the last.
                Some((_, text)) => {
                    text.push('\n');
                    text.push_str(line);
                }
                None => unread.push(format!(
                    "\"{line}\": it doesn't start with a question's number"
                )),
            },
        }
    }
    for (n, text) in pairs {
        let Some(q) = n.checked_sub(1).and_then(|i| qs.get(i)) else {
            unread.push(format!("{n}: there's no question {n}"));
            continue;
        };
        out.insert(q.question.clone(), pick(q, &text));
    }
    if out.is_empty() && unread.is_empty() {
        unread.push("nothing in it answers a question".into());
    }
    if unread.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "I couldn't read {}. Reply a line per question, as \"1: 2\".",
            unread.join("; ")
        ))
    }
}

fn pick(q: &Question, text: &str) -> Value {
    let one = |part: &str| -> Option<String> {
        let part = part.trim();
        if let Ok(i) = part.parse::<usize>() {
            return i
                .checked_sub(1)
                .and_then(|i| q.options.get(i))
                .map(|o| o.label.clone());
        }
        let lower = part.to_lowercase();
        let exact = q.options.iter().find(|o| o.label.to_lowercase() == lower);
        if let Some(o) = exact {
            return Some(o.label.clone());
        }
        let starts: Vec<_> = q
            .options
            .iter()
            .filter(|o| !lower.is_empty() && o.label.to_lowercase().starts_with(&lower))
            .collect();
        (starts.len() == 1).then(|| starts[0].label.clone())
    };
    if q.multi_select {
        let parts: Vec<Option<String>> = text.split(',').map(one).collect();
        if !parts.is_empty() && parts.iter().all(Option::is_some) {
            return Value::Array(parts.into_iter().flatten().map(Value::String).collect());
        }
    } else if let Some(label) = one(text) {
        return Value::String(label);
    }
    Value::String(text.to_string())
}

/// Resumes a review with answers, in the background, as its own process:
/// it logs to the review's progress.log, and its output to a file kept
/// beside the answers.
fn resume(paths: &Paths, run: &str, answers: &Path) -> Result<()> {
    #[cfg(test)]
    {
        let _ = paths;
        tests::RESUMED
            .lock()
            .unwrap()
            .push((run.to_string(), answers.to_path_buf()));
        return Ok(());
    }
    #[allow(unreachable_code)]
    let exe = std::env::current_exe().context("finding this program")?;
    let logs = gb::dir(paths).join("resumes");
    fs::create_dir_all(&logs)?;
    let log = fs::File::create(logs.join(stamped(&format!("{run}.log"))))?;
    let prefix = app().env.first().copied().unwrap_or("ACRBUS");
    let mut cmd = Command::new(exe);
    cmd.args(["acr", "resume", run, "--answers"])
        .arg(format!("@{}", answers.display()))
        .env(format!("{prefix}_{RESUMED}"), "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().context("starting the resume")?;
    // Reaped when it ends, so it never lingers.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Clears what was sent, answered or resumed past `cleanup::KEEP`, by the
/// time in its name. The daemon's hourly cleanup calls it. Returns how
/// many went.
pub fn prune(paths: &Paths) -> usize {
    let base = gb::dir(paths);
    let mut gone = 0;
    for dir in [
        base.join("sent"),
        base.join("answers"),
        base.join("resumes"),
        base.join("pending").join("done"),
    ] {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if crate::util::older_than(&path, crate::cleanup::KEEP)
                && fs::remove_file(&path).is_ok()
            {
                gone += 1;
            }
        }
    }
    gone
}

// Claims.

/// What `acr run` hears back about its claim.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaimAnswer {
    pub taken: bool,
    #[serde(default)]
    pub held_by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    /// Why it couldn't be asked, such as a server that's away.
    #[serde(default)]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Held {
    run: String,
    expires_at: i64,
}

fn read_claims(paths: &Paths) -> BTreeMap<String, Held> {
    read_json(&claims_path(paths)).unwrap_or_default()
}

/// The claim for a change: its pull request, or its head.
pub fn claim_key(repo: &str, pr: Option<u64>, head: Option<&str>) -> String {
    match (pr, head) {
        (Some(pr), _) => format!("review:{repo}#{pr}"),
        (None, Some(head)) => format!(
            "review:{repo}@{}",
            head.chars().take(12).collect::<String>()
        ),
        (None, None) => format!("review:{repo}@worktree"),
    }
}

/// Takes `key` for review `run`, in the daemon.
pub fn take_claim(
    paths: &Paths,
    s: &mut Session,
    key: &str,
    run: &str,
    note: &str,
) -> Result<ClaimAnswer> {
    let mut held = read_claims(paths);
    if let Some(h) = held.get(key)
        && h.run != run
        && !done(paths, &h.run)
    {
        return Ok(ClaimAnswer {
            taken: false,
            held_by: s.principal.as_ref().map(|p| format!("{p}/{}", name(paths))),
            note: Some(format!("{APP_NAME} review {} on this machine", h.run)),
            ..ClaimAnswer::default()
        });
    }
    let r = s.call(
        "claim_take",
        json!({ "key": key, "ttl_minutes": CLAIM_MINUTES, "note": note }),
    )?;
    if r["ok"].as_bool() == Some(true) {
        held.insert(
            key.to_string(),
            Held {
                run: run.to_string(),
                expires_at: r["claim"]["expires_at"]
                    .as_i64()
                    .unwrap_or(now_ms() + CLAIM_MINUTES * 60_000),
            },
        );
        save_json(&claims_path(paths), &held)?;
        return Ok(ClaimAnswer {
            taken: true,
            until: r["claim"]["expires_at"].as_i64(),
            ..ClaimAnswer::default()
        });
    }
    let since = s
        .call("claim_list", json!({ "prefix": key }))
        .ok()
        .and_then(|list| {
            list.as_array()?
                .iter()
                .find(|c| c["key"].as_str() == Some(key))
                .and_then(|c| c["acquired_at"].as_i64())
        });
    Ok(ClaimAnswer {
        taken: false,
        held_by: r["held_by"].as_str().map(str::to_string),
        note: r["note"].as_str().map(str::to_string),
        since,
        until: r["expires_at"].as_i64(),
        problem: None,
    })
}

fn name(paths: &Paths) -> String {
    gb::settings(paths)
        .map(|c| c.name)
        .unwrap_or_else(|| "acr".into())
}

/// Whether review `run` is over: confirmed, stopped, or gone.
fn done(paths: &Paths, run: &str) -> bool {
    if super::keep::is_finished(paths, run) {
        return true;
    }
    let path = super::run_dir(paths, run).join("run.json");
    match fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Run>(&b).ok())
    {
        Some(r) => matches!(r.status, Status::Confirmed | Status::Stopped),
        None => true,
    }
}

/// Renews the claims of reviews still running or waiting, and releases the
/// claims of reviews that ended.
fn keep_claims(paths: &Paths, s: &mut Session, log: &dyn Fn(String)) -> Result<()> {
    let mut held = read_claims(paths);
    if held.is_empty() {
        return Ok(());
    }
    let mut changed = false;
    for (key, h) in held.clone() {
        if done(paths, &h.run) {
            s.call(
                "claim_release",
                json!({ "key": key, "note": format!("review {} ended", h.run) }),
            )?;
            held.remove(&key);
            changed = true;
            log(format!("grayboard: released {key}"));
        } else if h.expires_at - now_ms() < RENEW_WITHIN_MS {
            let r = s.call(
                "claim_take",
                json!({ "key": key, "ttl_minutes": CLAIM_MINUTES }),
            )?;
            if let Some(until) = r["claim"]["expires_at"].as_i64() {
                held.insert(
                    key.clone(),
                    Held {
                        run: h.run.clone(),
                        expires_at: until,
                    },
                );
                changed = true;
            } else {
                log(format!(
                    "grayboard: couldn't renew {key}: {}",
                    r["held_by"].as_str().unwrap_or("refused")
                ));
            }
        }
    }
    if changed {
        save_json(&claims_path(paths), &held)?;
    }
    Ok(())
}

/// The daemon's answer to a claim request, said plainly when it can't ask.
pub fn claim_request(key: &str, run: &str, note: &str) -> ClaimAnswer {
    let Some(svc) = gb::service() else {
        return ClaimAnswer {
            problem: Some(
                "this daemon has no Grayboard: restart it after adding [grayboard]".into(),
            ),
            ..ClaimAnswer::default()
        };
    };
    let paths = svc.paths.clone();
    match svc.with(|s| take_claim(&paths, s, key, run, note)) {
        Ok(a) => a,
        Err(e) => ClaimAnswer {
            problem: Some(match gb::trouble(&e) {
                Some(Trouble::Unreachable(_)) => {
                    format!("{e:#}; the review goes on without its claim")
                }
                _ => format!("{e:#}"),
            }),
            ..ClaimAnswer::default()
        },
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::acr::carousel::Choice;
    use std::sync::{Arc, Mutex};

    /// The resumes `resume` would have started: (review, answers file).
    pub static RESUMED: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());

    /// A Grayboard server, as much of one as acrbus uses: discovery, OAuth
    /// with PKCE and rotating refresh tokens, and hosted MCP over
    /// server-sent events.
    #[derive(Default)]
    struct Fake {
        base: String,
        challenge: Option<String>,
        redirect: Option<String>,
        access: String,
        refresh: String,
        /// The next MCP call is refused 401, as an expired token is.
        expire_next: bool,
        sessions: Vec<String>,
        held: Vec<String>,
        next_id: i64,
        /// To your human handle: (id, body, parent).
        to_human: Vec<(i64, String, Option<i64>)>,
        /// From it: (id, body, parent, acked).
        from_human: Vec<(i64, String, Option<i64>, bool)>,
        claims: BTreeMap<String, (String, String, i64)>,
        released: Vec<String>,
    }

    const ME: &str = "you@example.com";

    fn respond(req: tiny_http::Request, status: u16, body: String, headers: &[(&str, &str)]) {
        let mut r = tiny_http::Response::from_string(body).with_status_code(status);
        for (k, v) in headers {
            r = r.with_header(tiny_http::Header::from_bytes(*k, *v).unwrap());
        }
        let _ = req.respond(r);
    }

    fn serve(fake: Arc<Mutex<Fake>>, server: tiny_http::Server) {
        for mut req in server.incoming_requests() {
            let url = req.url().to_string();
            let (path, q) = url.split_once('?').unwrap_or((&url, ""));
            let mut body = String::new();
            let _ = req.as_reader().read_to_string(&mut body);
            let auth = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Authorization"))
                .map(|h| h.value.to_string());
            let session = req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Mcp-Session-Id"))
                .map(|h| h.value.to_string());
            let mut f = fake.lock().unwrap();
            let base = f.base.clone();
            match path {
                "/.well-known/oauth-protected-resource/mcp" => respond(req, 200, json!({ "resource": format!("{base}/mcp"), "authorization_servers": [base] }).to_string(), &[]),
                "/.well-known/oauth-authorization-server" => respond(req, 200, json!({ "issuer": base, "authorization_endpoint": format!("{base}/oauth/authorize"), "token_endpoint": format!("{base}/oauth/token"), "revocation_endpoint": format!("{base}/oauth/revoke") }).to_string(), &[]),
                "/oauth/authorize" => {
                    let p: BTreeMap<String, String> = gb::query(q).into_iter().collect();
                    assert_eq!(p["client_id"], "acrbus");
                    assert_eq!(p["code_challenge_method"], "S256");
                    assert_eq!(p["scope"], "grayboard:tools grayboard:delivery");
                    f.challenge = Some(p["code_challenge"].clone());
                    f.redirect = Some(p["redirect_uri"].clone());
                    let to = format!("{}?code=c1&state={}", p["redirect_uri"], gb::enc(&p["state"]));
                    respond(req, 302, String::new(), &[("Location", &to)]);
                }
                "/oauth/token" => {
                    let p: BTreeMap<String, String> = gb::query(&body).into_iter().collect();
                    let ok = match p["grant_type"].as_str() {
                        "authorization_code" => {
                            f.challenge.as_deref() == Some(&gb::challenge(&p["code_verifier"]))
                                && p["code"] == "c1"
                                && f.redirect.as_ref() == p.get("redirect_uri")
                        }
                        _ => p["refresh_token"] == f.refresh && !f.refresh.is_empty(),
                    };
                    if !ok {
                        respond(req, 400, json!({ "error": "invalid_grant" }).to_string(), &[]);
                        continue;
                    }
                    f.next_id += 1;
                    f.access = format!("a{}", f.next_id);
                    f.refresh = format!("r{}", f.next_id);
                    respond(req, 200, json!({ "access_token": f.access, "token_type": "Bearer", "expires_in": 3600, "refresh_token": f.refresh, "scope": "grayboard:tools grayboard:delivery" }).to_string(), &[]);
                }
                "/mcp" => {
                    if f.expire_next || auth.as_deref() != Some(&format!("Bearer {}", f.access)) {
                        f.expire_next = false;
                        respond(req, 401, "{}".into(), &[]);
                        continue;
                    }
                    let msg: Value = serde_json::from_str(&body).unwrap();
                    let id = msg["id"].clone();
                    let sse = |result: Value| format!("event: message\ndata: {}\n\n", json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                    match msg["method"].as_str().unwrap() {
                        "initialize" => {
                            let s = format!("s{}", f.sessions.len() + 1);
                            f.sessions.push(s.clone());
                            respond(req, 200, sse(json!({ "protocolVersion": "2025-06-18" })), &[("Content-Type", "text/event-stream"), ("mcp-session-id", &s)]);
                        }
                        "notifications/initialized" => respond(req, 202, String::new(), &[]),
                        "tools/call" => {
                            if session.as_ref().is_none_or(|s| !f.sessions.contains(s)) {
                                respond(req, 404, "{\"message\":\"session not found\"}".into(), &[]);
                                continue;
                            }
                            assert!(msg["params"]["_meta"][gb::SESSION_META].as_str().is_some());
                            let a = &msg["params"]["arguments"];
                            let text = tool(&mut f, msg["params"]["name"].as_str().unwrap(), a);
                            let result = json!({ "content": [{ "type": "text", "text": text.to_string() }] });
                            respond(req, 200, sse(result), &[("Content-Type", "text/event-stream")]);
                        }
                        other => panic!("unexpected {other}"),
                    }
                }
                _ => respond(req, 404, String::new(), &[]),
            }
        }
    }

    fn tool(f: &mut Fake, name: &str, a: &Value) -> Value {
        let now = now_ms();
        match name {
            "whoami" => json!({ "principal": ME, "held": f.held }),
            "bind" => {
                f.held.push(format!("{ME}/{}", a["name"].as_str().unwrap()));
                json!("Bound")
            }
            "dm_send" => {
                assert_eq!(a["to"], "human");
                f.next_id += 1;
                let id = f.next_id;
                f.to_human.push((id, a["body"].as_str().unwrap().into(), a["parent_id"].as_i64()));
                json!({ "message_id": id, "delivered_now": true })
            }
            "dm_inbox" => json!({ "unanswered": f.from_human.iter().filter(|m| !m.3).map(|m| json!({ "id": m.0, "from": format!("{ME}/human"), "parent_id": m.2, "age": "0m", "body": m.1 })).collect::<Vec<_>>() }),
            "dm_ack" => {
                let id = a["message_id"].as_i64().unwrap();
                for m in &mut f.from_human {
                    if m.0 == id {
                        m.3 = true;
                    }
                }
                json!("ok")
            }
            "claim_take" => {
                let key = a["key"].as_str().unwrap().to_string();
                let me = format!("{ME}/acr");
                match f.claims.get(&key) {
                    Some((holder, note, _)) if *holder != me => json!({ "ok": false, "held_by": holder, "note": note, "expires_at": now + 600_000 }),
                    _ => {
                        let since = f.claims.get(&key).map_or(now, |c| c.2);
                        f.claims.insert(key.clone(), (me.clone(), a["note"].as_str().unwrap_or("").into(), since));
                        json!({ "ok": true, "claim": { "key": key, "holder": me, "acquired_at": since, "expires_at": now + 3_600_000 }, "renewed": false })
                    }
                }
            }
            "claim_list" => json!(f.claims.iter().map(|(k, c)| json!({ "key": k, "holder": c.0, "note": c.1, "acquired_at": c.2, "expires_at": now + 600_000 })).collect::<Vec<_>>()),
            "claim_release" => {
                let key = a["key"].as_str().unwrap().to_string();
                f.claims.remove(&key);
                f.released.push(key);
                json!("ok")
            }
            other => panic!("unexpected tool {other}"),
        }
    }

    fn fake() -> (Arc<Mutex<Fake>>, String) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!(
            "http://127.0.0.1:{}",
            server.server_addr().to_ip().unwrap().port()
        );
        let fake = Arc::new(Mutex::new(Fake {
            base: base.clone(),
            ..Fake::default()
        }));
        let f = fake.clone();
        std::thread::spawn(move || serve(f, server));
        (fake, base)
    }

    fn paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("tb-gb-{}", crate::util::new_id()));
        fs::create_dir_all(&dir).unwrap();
        Paths {
            state: dir.join("state"),
            config: dir.join("config.toml"),
        }
    }

    fn cfg(share: Option<&str>) -> Config {
        use crate::acr::config;
        let extra = share
            .map(|s| format!("integrations: {{grayboard: {s}}}\n"))
            .unwrap_or_default();
        config::resolve(
            &config::parse_user(include_str!("../../examples/acr/user.yaml")).unwrap(),
            config::parse_repo(&format!(
                "version: 1\ntiers: [{{id: a, match: {{paths: ['**']}}, fan_out: 1}}]\n\
                 lenses: [{{id: l, criteria: c, triggers: {{paths: ['**']}}}}]\n{extra}"
            ))
            .unwrap(),
            &config::Flags::default(),
        )
        .unwrap()
    }

    fn waiting(id: &str) -> Run {
        serde_json::from_value(json!({
            "id": id, "repo": "/r/app", "base": "main", "head": null,
            "tier": "a", "tier_why": "the Sizer", "fan_out": 1,
            "round": 1, "stage": "scope", "status": "waiting", "waiting": "scope",
            "lenses": {}, "escalations": [], "disagreements": [], "usage": [],
            "starter": "app"
        }))
        .unwrap()
    }

    fn signed_in(base: &str) -> (Paths, Session) {
        let paths = paths();
        fs::write(&paths.config, format!("[grayboard]\nserver = \"{base}\"\n")).unwrap();
        let gcfg = gb::settings(&paths).unwrap();
        // The browser: follows the sign-in to the loopback, where login waits.
        let browser = |url: &str| {
            let url = url.to_string();
            std::thread::spawn(move || {
                let _ = ureq::get(&url).call();
            });
        };
        gb::login(&paths, &gcfg, &browser).unwrap();
        let mut s = Session::open(&paths).unwrap();
        gb::bind(&mut s, "acr").unwrap();
        (paths, s)
    }

    fn reply(fake: &Arc<Mutex<Fake>>, body: &str, parent: Option<i64>) {
        let mut f = fake.lock().unwrap();
        f.next_id += 1;
        let id = f.next_id;
        f.from_human.push((id, body.into(), parent, false));
    }

    fn last_to_human(fake: &Arc<Mutex<Fake>>) -> (i64, String) {
        let f = fake.lock().unwrap();
        let (id, body, _) = f.to_human.last().cloned().unwrap();
        (id, body)
    }

    #[test]
    fn a_reviews_questions_reach_you_and_your_reply_resumes_it() {
        let (fake, base) = fake();
        let (paths, mut s) = signed_in(&base);
        // Signed in: the grant is yours alone, and the name is held.
        let grant = gb::dir(&paths).join("grant.json");
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&grant).unwrap().permissions())
                & 0o777,
            0o600
        );
        assert_eq!(fake.lock().unwrap().held, [format!("{ME}/acr")]);
        assert_eq!(s.principal.as_deref(), Some(ME));

        let cfg = cfg(None);
        let run = waiting("01RUNAAAAAAAAA");
        crate::acr::save(&paths, &run, &cfg).unwrap();
        on_finish(&paths, &run, &cfg);
        let log = |_: String| {};
        tick(&paths, &mut s, &log).unwrap();
        let (sent_id, text) = last_to_human(&fake);
        assert!(
            text.contains("Review 01RUNAAAAAAAAA of app is waiting for you at its scope gate"),
            "{text}"
        );
        assert!(
            text.contains("1. [")
                && text.contains("1) Approve")
                && text.contains("Reply in this thread"),
            "{text}"
        );
        assert!(files(&outbox(&paths)).is_empty() && files(&sent(&paths)).len() == 1);

        // What it can't read, it says, in the same thread.
        reply(&fake, "sounds good", Some(sent_id));
        tick(&paths, &mut s, &log).unwrap();
        let (_, said) = last_to_human(&fake);
        assert!(
            said.contains("couldn't read") && said.contains("\"1: 2\""),
            "{said}"
        );
        assert!(
            RESUMED
                .lock()
                .unwrap()
                .iter()
                .all(|(r, _)| r != "01RUNAAAAAAAAA")
        );

        // A real answer resumes it, once: the scope, and who fixes.
        reply(&fake, "1: 1\n2: The Orchestrator", Some(sent_id));
        tick(&paths, &mut s, &log).unwrap();
        let (_, said) = last_to_human(&fake);
        assert!(said.starts_with("Resuming it with your answers"), "{said}");
        let resumed = RESUMED
            .lock()
            .unwrap()
            .iter()
            .find(|(r, _)| r == "01RUNAAAAAAAAA")
            .cloned()
            .unwrap();
        let answers: BTreeMap<String, Value> =
            serde_json::from_slice(&fs::read(&resumed.1).unwrap()).unwrap();
        assert!(answers.values().any(|v| v == "Approve"), "{answers:?}");
        assert!(
            answers.values().any(|v| v == "The Orchestrator"),
            "{answers:?}"
        );
        reply(&fake, "1: 3", Some(sent_id));
        tick(&paths, &mut s, &log).unwrap();
        assert!(last_to_human(&fake).1.contains("answered already"));
        // Every reply was acknowledged.
        assert!(fake.lock().unwrap().from_human.iter().all(|m| m.3));

        // Questions it has moved past are stale.
        let mut on = run.clone();
        on.round = 2;
        crate::acr::save(&paths, &on, &cfg).unwrap();
        let mut p: Pending = read_json(&pending_dir(&paths).join("01RUNAAAAAAAAA.json")).unwrap();
        p.answered = None;
        save_json(&pending_dir(&paths).join("01RUNAAAAAAAAA.json"), &p).unwrap();
        reply(&fake, "1: 1\n2: 2", Some(sent_id));
        tick(&paths, &mut s, &log).unwrap();
        assert!(
            last_to_human(&fake).1.contains("moved on"),
            "{}",
            last_to_human(&fake).1
        );

        // An expired token is refreshed, and rotated on disk; a dropped
        // session is opened again.
        fake.lock().unwrap().expire_next = true;
        fake.lock().unwrap().sessions.clear();
        s.call("whoami", json!({})).unwrap();
        let on_disk: Value = serde_json::from_slice(&fs::read(&grant).unwrap()).unwrap();
        assert_eq!(on_disk["refresh"], json!(fake.lock().unwrap().refresh));
    }

    #[test]
    fn a_nudge_says_only_that_a_review_waits() {
        let (fake, base) = fake();
        let (paths, mut s) = signed_in(&base);
        let cfg = cfg(Some("nudge"));
        let run = waiting("01RUNBBBBBBBBB");
        crate::acr::save(&paths, &run, &cfg).unwrap();
        on_finish(&paths, &run, &cfg);
        tick(&paths, &mut s, &|_| {}).unwrap();
        let (id, text) = last_to_human(&fake);
        assert!(
            text.contains("They stay on its machine") && !text.contains("1) "),
            "{text}"
        );
        reply(&fake, "1: 1", Some(id));
        tick(&paths, &mut s, &|_| {}).unwrap();
        assert!(last_to_human(&fake).1.contains("stay on its machine"));
    }

    #[test]
    fn a_change_is_claimed_while_its_review_lasts() {
        let (fake, base) = fake();
        let (paths, mut s) = signed_in(&base);
        let cfg = cfg(None);
        let run = waiting("01RUNCCCCCCCCC");
        crate::acr::save(&paths, &run, &cfg).unwrap();
        let a = take_claim(
            &paths,
            &mut s,
            "review:app#12",
            "01RUNCCCCCCCCC",
            "acrbus review 01RUNCCCCCCCCC",
        )
        .unwrap();
        assert!(a.taken, "{a:?}");
        // A second review of it here is refused while the first waits.
        let b = take_claim(&paths, &mut s, "review:app#12", "01RUNDDDDDDDDD", "n").unwrap();
        assert!(!b.taken && b.note.unwrap().contains("01RUNCCCCCCCCC"));
        // And so is one someone else holds, saying who and since when.
        fake.lock().unwrap().claims.insert(
            "review:app#13".into(),
            (
                "bob@example.com/acr".into(),
                "bob's review".into(),
                now_ms() - 120_000,
            ),
        );
        let c = take_claim(&paths, &mut s, "review:app#13", "01RUNDDDDDDDDD", "n").unwrap();
        assert!(
            !c.taken && c.held_by.as_deref() == Some("bob@example.com/acr") && c.since.is_some()
        );
        // Waiting, it stays held; once over, it's released.
        keep_claims(&paths, &mut s, &|_| {}).unwrap();
        assert!(fake.lock().unwrap().released.is_empty());
        let mut over = run.clone();
        over.status = Status::Confirmed;
        crate::acr::save(&paths, &over, &cfg).unwrap();
        keep_claims(&paths, &mut s, &|_| {}).unwrap();
        assert_eq!(fake.lock().unwrap().released, ["review:app#12"]);
    }

    fn q(text: &str, labels: &[&str], multi: bool) -> Question {
        Question {
            question: text.into(),
            header: "H".into(),
            options: labels
                .iter()
                .map(|l| Choice {
                    label: (*l).into(),
                    description: String::new(),
                })
                .collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn a_reply_from_a_phone_reads_as_answers() {
        let qs = [
            q(
                "Approve the scope?",
                &["Approve", "One tier higher", "Stop the review"],
                false,
            ),
            q(
                "This blocker?",
                &["Fix it", "Waive it", "Not a blocker", "Not a defect"],
                false,
            ),
            q("Should these block?", &["A", "B", "None of them"], true),
        ];
        let got = parse(&qs, "1: 1\n2. fix\n3) 1, 2").unwrap();
        assert_eq!(got["Approve the scope?"], json!("Approve"));
        assert_eq!(got["This blocker?"], json!("Fix it"));
        assert_eq!(got["Should these block?"], json!(["A", "B"]));
        // Words of your own go on, over lines, and stay as written.
        let got = parse(&qs, "2: guard the nil\nand test it").unwrap();
        assert_eq!(got["This blocker?"], json!("guard the nil\nand test it"));
        // One question takes a bare answer.
        let one = parse(&qs[..1], "approve").unwrap();
        assert_eq!(one["Approve the scope?"], json!("Approve"));
        // What it can't read, it says.
        let e = parse(&qs, "yes please").unwrap_err();
        assert!(e.contains("doesn't start with a question's number"), "{e}");
        let e = parse(&qs, "7: 1").unwrap_err();
        assert!(e.contains("there's no question 7"), "{e}");
    }

    #[test]
    fn a_reply_can_name_its_review() {
        assert_eq!(strip_id("01M78ABC 1: 2", "01M78ABCDEF"), "1: 2");
        assert_eq!(strip_id("01m78abc\n1: 2", "01M78ABCDEF"), "1: 2");
        assert_eq!(strip_id("1: 2", "01M78ABCDEF"), "1: 2");
    }

    #[test]
    fn a_change_is_claimed_by_its_pull_request_or_its_head() {
        assert_eq!(claim_key("app", Some(12), Some("abc")), "review:app#12");
        assert_eq!(
            claim_key("app", None, Some("0123456789abcdef")),
            "review:app@0123456789ab"
        );
        assert_eq!(claim_key("app", None, None), "review:app@worktree");
    }
}
