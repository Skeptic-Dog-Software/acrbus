//! What each role is asked, and what it must answer. Answers are JSON, and
//! one that can't be read is a failure, never an empty result.

use anyhow::{Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::config::{Lens, Severity, Tier};

/// The objects in an answer that read as `T`, each once, and the first
/// error from one that didn't.
fn fits<T: DeserializeOwned>(text: &str) -> (Vec<serde_json::Value>, Option<serde_json::Error>) {
    let mut first_error = None;
    let mut answers: Vec<serde_json::Value> = Vec::new();
    let mut from = 0;
    while let Some(start) = text[from..].find('{').map(|i| from + i) {
        let mut objects =
            serde_json::Deserializer::from_str(&text[start..]).into_iter::<serde_json::Value>();
        match objects.next() {
            Some(Ok(value)) => {
                from = start + objects.byte_offset();
                match serde_json::from_value::<T>(value.clone()) {
                    Ok(_) if !answers.contains(&value) => answers.push(value),
                    Ok(_) => {}
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
            _ => from = start + 1,
        }
    }
    (answers, first_error)
}

fn none_fit(first_error: Option<serde_json::Error>) -> anyhow::Error {
    match first_error {
        Some(e) => anyhow::Error::new(e).context("reading the answer's JSON"),
        None => anyhow!("the answer has no JSON"),
    }
}

/// The JSON in an answer, which a model may have wrapped in other words,
/// braces among them: the first whole object in it that reads as `T`. If
/// none does, the error is the first object's.
pub(super) fn json_in<T: DeserializeOwned>(text: &str) -> Result<T> {
    let (mut answers, first_error) = fits::<T>(text);
    match answers.is_empty() {
        false => Ok(serde_json::from_value(answers.remove(0))?),
        true => Err(none_fit(first_error)),
    }
}

/// The one answer in a report of findings. Two lists that say different
/// things, such as an example before the real one, are an error, not a
/// guess: an empty example taken for the answer would pass a change that
/// was found wanting.
fn one_json_in<T: DeserializeOwned>(text: &str) -> Result<T> {
    let (mut answers, first_error) = fits::<T>(text);
    match answers.len() {
        0 => Err(none_fit(first_error)),
        1 => Ok(serde_json::from_value(answers.remove(0))?),
        n => bail!(
            "the answer has {n} different lists of findings in it, so which it means is unclear"
        ),
    }
}

/// Readers for what models write where a strict one would throw a whole
/// answer away: a range for a line, a severity in lower case, one side as a
/// string, a flag as a word.
mod lenient {
    use serde::Deserialize;
    use serde::de::{Deserializer, Error};
    use serde_json::Value;

    use super::Severity;

    /// A line, or the first number in text such as "169-171" or "L42".
    pub fn line<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => {
                let digits: String = s
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse().ok()
            }
            _ => None,
        })
    }

    /// A severity in any case, such as "high", or "High: a real defect".
    pub fn severity<'de, D: Deserializer<'de>>(d: D) -> Result<Severity, D::Error> {
        let text = String::deserialize(d)?;
        let word: String = text
            .trim()
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_ascii_lowercase();
        Ok(match word.as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "medium" => Severity::Medium,
            "low" => Severity::Low,
            "rejected" | "reject" => Severity::Rejected,
            _ => return Err(D::Error::custom(format!("{text:?} isn't a severity"))),
        })
    }

    /// True or false, or the words for them.
    pub fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
        match Value::deserialize(d)? {
            Value::Bool(b) => Ok(b),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" => Ok(true),
                "false" | "no" => Ok(false),
                _ => Err(D::Error::custom(format!("{s:?} isn't true or false"))),
            },
            other => Err(D::Error::custom(format!("{other} isn't true or false"))),
        }
    }

    /// A list of strings, or one string.
    pub fn strings<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::Null => Vec::new(),
            Value::String(s) => vec![s],
            Value::Array(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                })
                .collect(),
            other => vec![other.to_string()],
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct Sized {
    pub tier: String,
    #[serde(default)]
    pub sure: bool,
    #[serde(default)]
    pub reason: String,
}

pub fn sizer_prompt(tiers: &[&Tier], files: &[String]) -> String {
    let rules: String = tiers
        .iter()
        .filter_map(|t| {
            let description = t.rule.description.as_ref()?;
            Some(format!("- {}: {description}\n", t.id))
        })
        .collect();
    format!(
        "Classify the attached change into one tier. It starts with what the change says \
         it's for, then its diff. The tiers run from lowest to highest:\n\n\
         {rules}\nThe changed files are: {}.\n\n\
         Answer with only JSON: {{\"tier\": \"<id>\", \"sure\": true or false, \
         \"reason\": \"<one line>\"}}. If you're not sure, say so.",
        files.join(", ")
    )
}

/// What the Sizer answers when the file list isn't enough to tell, so the
/// fuller call follows.
pub const UNKNOWN_TIER: &str = "unknown";

/// The Sizer's first look: from the changed files, the lines each adds and
/// removes, and what the change says it's for, with no diff and no tools.
pub fn sizer_light_prompt(
    tiers: &[&Tier],
    stat: &[(String, u64, u64)],
    description: Option<&str>,
    commits: &[super::change::Commit],
) -> String {
    let rules: String = tiers
        .iter()
        .filter_map(|t| {
            let description = t.rule.description.as_ref()?;
            Some(format!("- {}: {description}\n", t.id))
        })
        .collect();
    let said: String = match description.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => d.chars().take(4000).collect(),
        None => commits
            .iter()
            .map(|c| format!("- {}\n", c.message.lines().next().unwrap_or("(no message)")))
            .collect::<String>(),
    };
    let said = if said.trim().is_empty() {
        "(nothing given)".to_string()
    } else {
        said
    };
    let files: String = stat
        .iter()
        .take(400)
        .map(|(path, added, removed)| format!("- {path} (+{added} -{removed})\n"))
        .collect();
    let more = stat.len().saturating_sub(400);
    let more = if more > 0 {
        format!("- and {more} more\n")
    } else {
        String::new()
    };
    format!(
        "Classify a change into one tier, from what's below alone: you have no tools and no \
         diff, so don't try to read anything. The tiers run from lowest to highest:\n\n\
         {rules}\n# What the change says it's for\n\n{said}\n\n\
         # The changed files, with the lines each adds and removes\n\n{files}{more}\n\
         Answer with only JSON: {{\"tier\": \"<id>\", \"sure\": true or false, \
         \"reason\": \"<one line>\"}}. If this isn't enough to tell, answer \
         {{\"tier\": \"{UNKNOWN_TIER}\", \"sure\": false, \"reason\": \"<what you'd need to see>\"}}, \
         and the diff follows."
    )
}

pub fn read_sized(text: &str) -> Result<Sized> {
    json_in(text)
}

/// What the Sizer is asked of a round's findings: which of them, found by
/// different lenses, are the same defect. `found` is (lens, id, where,
/// title, detail).
pub fn same_prompt(found: &[(String, String, String, String, String)]) -> String {
    let mut list = String::new();
    for (lens, id, at, title, detail) in found {
        let detail: String = detail.chars().take(300).collect();
        list.push_str(&format!(
            "- {id} [{lens}] {at}: {title}\n  {}\n",
            detail.trim()
        ));
    }
    format!(
        "Reviewers of different lenses each reported the findings below. Group the ones that \
         are the same defect: the same cause in the same place, which one fix would fix. \
         Findings that are only near each other, or alike in kind, are different defects. \
         Leave out a finding that's like no other.\n\n{list}\nAnswer with JSON only: \
         {{\"same\": [[\"<id>\", \"<id>\"], ...]}}, or {{\"same\": []}} if none are the same."
    )
}

#[derive(Debug, Deserialize)]
struct Same {
    same: Vec<Vec<String>>,
}

/// Each group of findings that are the same defect.
pub fn read_same(text: &str) -> Result<Vec<Vec<String>>> {
    one_json_in::<Same>(text).map(|s| s.same)
}

/// The lenses are required, so a stray object doesn't pass for "none
/// apply"; the escalations are required where there were some to check,
/// which the reader of the answer sees to.
#[derive(Debug, Default, Deserialize)]
pub struct Selected {
    /// Lenses whose descriptions apply.
    pub lenses: Vec<String>,
    /// `always_escalate` items that apply, if it said.
    #[serde(default)]
    pub escalate: Option<Vec<String>>,
}

pub fn orchestrator_prompt(lenses: &[(&str, &str)], escalate: &[String]) -> String {
    let lens_list: String = lenses
        .iter()
        .map(|(id, d)| format!("- {id}: {d}\n"))
        .collect();
    let escalate_list: String = escalate.iter().map(|e| format!("- {e}\n")).collect();
    format!(
        "Read the attached change, which starts with what's said about it, then its diff, \
         and say which of these apply to it.\n\n\
         Lenses, each with when it applies:\n{lens_list}\n\
         Things that must go to a person:\n{escalate_list}\n\
         Answer with only JSON: {{\"lenses\": [\"<id>\"], \"escalate\": [\"<item, word for word>\"]}}. \
         When in doubt, include it."
    )
}

pub fn read_selected(text: &str) -> Result<Selected> {
    json_in(text)
}

/// Puts an Orchestrator's answer on which lenses and escalations apply into
/// the form asked, from the ones it was asked about.
pub fn reformat_selected_prompt(answer: &str, lenses: &[&str], escalate: &[String]) -> String {
    let list = |items: Vec<String>| match items.is_empty() {
        true => "none".to_string(),
        false => items.join("; "),
    };
    format!(
        "An Orchestrator said which lenses apply to a change and which things about it must \
         go to a person, where its answer should have been JSON. Put it in that form: the \
         lenses it says apply, by id, from these: {}; and the things it says apply, word for \
         word, from these: {}. Add nothing it doesn't say, and when it's unclear about one, \
         include it. If it says none apply, give empty lists. If the answer is cut off, or \
         doesn't say, answer only: unreadable.\n\n\
         The answer:\n<<<\n{}\n>>>\n\n\
         Answer with only JSON: {{\"lenses\": [\"<id>\"], \"escalate\": [\"<item, word for word>\"]}}.",
        list(lenses.iter().map(|l| l.to_string()).collect()),
        list(escalate.to_vec()),
        answer.trim()
    )
}

/// One thing a reviewer found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Raw {
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default, deserialize_with = "lenient::line")]
    pub line: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Reviewed {
    findings: Vec<Raw>,
}

/// For every role that runs things. One reviewer on a dependency bump spent
/// 4.3M of its 12.3M tokens checking on a test run it had started in the
/// background, 28 times, each check rereading a context of up to 174K tokens.
pub(super) const WAIT: &str = "Run a long command in the foreground, with a time limit, and \
     wait for it. Don't start it in the background and keep checking on it: each check \
     rereads everything you've read so far, which can cost more than the run.";

/// A reviewer's whole brief: the lens, the scope, and what's already been
/// decided. The attached file is the change: what's said about it, then its
/// diff. Those are its claims; the reviewer gets nothing of the case made
/// for them, and none of the rebuttals of earlier rounds.
/// Where a part of a prompt came from: what a role's input is made of, so a
/// test can hold a reviewer to nothing a model wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// This version's own text, the same for every call of the role.
    Fixed,
    /// The repository's review config, which a person accepted.
    Config,
    /// Found by a tool: the gate's results, a scope or a path.
    Tool,
    /// Written by a model during the review: a brief.
    Model,
}

/// A part of a prompt, with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub source: Source,
    pub text: String,
}

fn part(source: Source, text: impl Into<String>) -> Part {
    Part {
        source,
        text: text.into(),
    }
}

/// A keep-alive's whole brief: the role's cached prefix is what it's for.
pub const KEEP_ALIVE: &str = "This call only refreshes the prompt cache: it isn't a task. Use no tool. Reply with the \
     single word: ok";

/// A prompt's parts, joined.
pub fn join(parts: &[Part]) -> String {
    parts.iter().map(|p| p.text.as_str()).collect()
}

/// What a reviewer's brief is made of, beyond the lens.
pub struct ReviewerCall<'a> {
    /// What's under review: the change from one commit to another.
    pub scope: &'a str,
    /// Decided non-issues, or "" for none: in facts-only mode, none.
    pub not_findings: &'a str,
    /// It works in a copy of its own, and can run things there.
    pub own_copy: bool,
    /// How to work in the repository, for a reviewer that runs things.
    pub runbook: Option<&'a str>,
    /// A brief written for this change, by its builder, the Orchestrator or
    /// a steelman: leads, not findings.
    pub brief: Option<&'a str>,
    /// The test gate's results, this round.
    pub gate: &'a str,
}

/// A reviewer's brief, in the cache's order: the role's own text, the same
/// for every reviewer, then the lens's, then this call's. One with
/// `own_copy` works in a copy of the checkout that's its alone, and is told
/// to settle doubts by running things there.
pub fn reviewer_parts(lens: &Lens, call: &ReviewerCall) -> Vec<Part> {
    let attached = super::change::ATTACHED;
    let mut parts = vec![part(
        Source::Fixed,
        format!(
            "You are a reviewer of a change. Find what is wrong: assume defects exist until \
             you've shown otherwise. {attached} What's said about it, and the comments and docs \
             the change adds, are claims to check, not to take on trust: check them against the \
             code and tests too, and report one they don't bear out. Report what's wrong in the \
             code the change touches or relies on even if it was wrong before the change, and \
             say it's pre-existing: that it isn't the change's doing doesn't make it not a \
             finding.\n\n\
             Answer with only JSON: {{\"findings\": [{{\"title\": \"<one line>\", \"detail\": \
             \"<the evidence>\", \"file\": \"<path>\", \"line\": <number>}}]}}. If you find \
             nothing, say so with an empty list.\n\n"
        ),
    )];
    if let Some(r) = call.runbook.map(str::trim).filter(|r| !r.is_empty()) {
        parts.push(part(
            Source::Config,
            format!("# How to work in this repository\n\n{r}\n\n"),
        ));
    }
    parts.push(part(
        Source::Config,
        format!(
            "# Your lens\n\nYou are the reviewer for {id}. Attack the change in this lens, and \
             nothing outside it. What wrong means here: {criteria}\n\n",
            id = lens.id,
            criteria = lens.criteria,
        ),
    ));
    if let Some(b) = lens
        .blocker
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        parts.push(part(Source::Config, format!("What blocks here: {b}\n\n")));
    }
    if let Some(a) = lens
        .attack
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        parts.push(part(
            Source::Config,
            format!("# How to attack in this lens\n\n{a}\n\n"),
        ));
    }
    parts.push(part(
        Source::Tool,
        format!("# This call\n\nScope: {}.\n\n", call.scope),
    ));
    parts.push(part(
        Source::Fixed,
        if call.own_copy {
            format!(
                "Your working directory is your own copy of the repository, as the change leaves \
                 it, and nothing you do there reaches the change. Read any of it you need. Where \
                 reading leaves a doubt, settle it by running something: the tests that cover the \
                 change, a probe of what it claims, or a mutation, breaking the code a test should \
                 guard to see whether the test fails. Report a test that still passes against \
                 broken code. Nobody is here to grant a permission, so a shell command that would \
                 need one is refused: name paths in full, not through variables or `$(...)`, and \
                 put a probe of more than one step in a script, written with the Write tool and \
                 run by its path. {WAIT}\n\n"
            )
        } else {
            "Your working directory is the repository, as the change leaves it: read any of it \
             you need, but change nothing.\n\n"
                .to_string()
        },
    ));
    if !call.gate.trim().is_empty() {
        parts.push(part(Source::Tool, call.gate.to_string()));
    }
    if let Some(b) = call.brief.map(str::trim).filter(|b| !b.is_empty()) {
        parts.push(part(
            Source::Model,
            format!(
                "# Where to attack in this change\n\nFrom a brief written for this change: \
                 leads, not findings, and what it says about the change is a claim to check, not \
                 to take on trust.\n\n{b}\n\n"
            ),
        ));
    }
    let decided = call.not_findings.trim();
    if !decided.is_empty() {
        parts.push(part(
            Source::Config,
            format!("Not findings, which are already decided, so leave them out:\n{decided}\n\n"),
        ));
    }
    parts.push(part(
        Source::Fixed,
        "Answer with only the JSON described above.",
    ));
    parts
}

/// A reviewer's brief, as one text. See [`reviewer_parts`].
#[cfg(test)]
pub fn reviewer_prompt(
    lens: &Lens,
    scope: &str,
    not_findings: &str,
    own_copy: bool,
    guide: &str,
) -> String {
    let parts = reviewer_parts(
        lens,
        &ReviewerCall {
            scope,
            not_findings,
            own_copy,
            runbook: None,
            brief: None,
            gate: guide,
        },
    );
    join(&parts)
}

/// For a reviewer that reported in prose where it should have answered in
/// JSON: put its report in the form asked, adding nothing and dropping
/// nothing. A report that can't be is still unreadable.
pub fn reformat_prompt(report: &str) -> String {
    format!(
        "A reviewer wrote this report on a change, where its answer should have been one JSON \
         object. Put it in that form. Each defect it reports is a finding: its title, the \
         evidence it gives as the detail, and the file and line if it names them. Add nothing \
         it doesn't say, and leave out nothing it reports as wrong. If it reports nothing \
         wrong, answer with an empty list. If it holds more than one list, such as an \
         example before its own, give what it reports, not the example. If the report is cut \
         off, or isn't a reviewer's report, answer only: unreadable.\n\n\
         The report:\n<<<\n{}\n>>>\n\n\
         Answer with only JSON: {{\"findings\": [{{\"title\": \"<one line>\", \"detail\": \
         \"<the evidence>\", \"file\": \"<path>\", \"line\": <number>}}]}}.",
        report.trim()
    )
}

/// What marks a handoff's answer: acrbus's pi extension puts it first, on a
/// line of its own, once a session runs out of room.
pub const HANDOFF_MARK: &str = "ACRBUS-HANDOFF";

/// What a session that ran out of room says it has done and has left.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    #[serde(default)]
    pub checked: String,
    #[serde(default)]
    pub left: String,
}

#[derive(Debug, Deserialize)]
struct Handed {
    handoff: Handoff,
}

/// A handoff, if the answer is one: marked as one, or carrying a `handoff`
/// field. Its text without the mark, to read what it found. An answer
/// marked as a handoff that can't be read is an error, never a finished
/// review: a session that ran out of room and couldn't say what it had done
/// left its lens unchecked.
pub fn handoff_of(text: &str) -> Option<Result<(String, Handoff)>> {
    let trimmed = text.trim_start();
    let marked = trimmed.starts_with(HANDOFF_MARK);
    let body = if marked {
        trimmed[HANDOFF_MARK.len()..].trim_start()
    } else {
        text
    };
    match one_json_in::<Handed>(body) {
        Ok(h) => Some(Ok((body.to_string(), h.handoff))),
        Err(e) if marked => Some(Err(
            e.context("the session ran out of room, and its handoff couldn't be read")
        )),
        Err(_) => None,
    }
}

/// One thing an earlier session already has in its answer: what the next
/// one is told, so it doesn't report it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Earlier {
    pub title: String,
    pub file: Option<String>,
    pub line: Option<u64>,
}

/// What a handoff's answer already holds, read loosely from its findings:
/// a reviewer's or a Judge's, whichever it is.
pub fn earlier_of(text: &str) -> Vec<Earlier> {
    #[derive(Deserialize)]
    struct Item {
        #[serde(default)]
        title: String,
        #[serde(default)]
        file: Option<String>,
        #[serde(default, deserialize_with = "lenient::line")]
        line: Option<u64>,
    }
    #[derive(Deserialize)]
    struct Items {
        #[serde(default)]
        findings: Vec<Item>,
        #[serde(default)]
        suspects: Vec<Item>,
    }
    one_json_in::<Items>(text)
        .map(|i| {
            i.findings
                .into_iter()
                .chain(i.suspects)
                .filter(|f| !f.title.trim().is_empty())
                .map(|f| Earlier {
                    title: f.title.trim().to_string(),
                    file: f.file,
                    line: f.line,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What a fresh session is told, after the call's own brief, so that every
/// session shares the brief's cached prefix: what the earlier ones checked,
/// what's left, and what they already found, not to be reported again.
pub fn continuation(checked: &[String], left: &str, earlier: &[Earlier], session: u32) -> String {
    let checked: String = checked
        .iter()
        .enumerate()
        .map(|(i, c)| format!("- Session {}: {}\n", i + 1, c.trim()))
        .collect();
    let found: String = if earlier.is_empty() {
        "- Nothing yet.\n".to_string()
    } else {
        earlier
            .iter()
            .map(|e| {
                let at = match (&e.file, e.line) {
                    (Some(f), Some(l)) => format!(" ({f}:{l})"),
                    (Some(f), None) => format!(" ({f})"),
                    _ => String::new(),
                };
                format!("- {}{at}\n", e.title)
            })
            .collect()
    };
    format!(
        "\n\n# Carrying on from an earlier session\n\n\
         This call's earlier session ran out of room and handed off to you: you are session \
         {session}. What it has in its answer is kept, and reported with yours, so don't report \
         it again: answer with only what you add from here, in the same JSON.\n\n\
         What the earlier sessions checked:\n{checked}\n\
         What's left to check, and where the last one was going next: {left}\n\n\
         What they already have, not to repeat:\n{found}",
        left = left.trim(),
    )
}

/// A defect a scout suspects, to be confirmed or ruled out on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suspect {
    pub title: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default, deserialize_with = "lenient::line")]
    pub line: Option<u64>,
    /// What's suspected.
    #[serde(default)]
    pub claim: String,
    /// How to confirm or rule it out.
    #[serde(default)]
    pub check: String,
}

#[derive(Debug, Deserialize)]
struct Scouted {
    suspects: Vec<Suspect>,
}

pub fn read_suspects(text: &str) -> Result<Vec<Suspect>> {
    Ok(one_json_in::<Scouted>(text)?.suspects)
}

/// A scout's session, after a reviewer's own brief, so it shares that
/// brief's cached prefix: list what's suspect, verify nothing.
pub fn scout_part() -> &'static str {
    "\n\n# This session: scout\n\n\
     This lens's review is in two steps, and this session is the first. Don't verify anything \
     here. Explore the change for your lens, and list everything you suspect is wrong, each \
     with how to confirm or rule it out. Each suspicion is then checked in a short session of \
     its own. List what you suspect even when you're unsure: a suspicion that doesn't hold \
     costs one short check, and one left out is a defect missed.\n\n\
     Instead of the findings JSON above, answer with only JSON: {\"suspects\": [{\"title\": \
     \"<one line>\", \"file\": \"<path>\", \"line\": <number>, \"claim\": \"<what you suspect \
     is wrong>\", \"check\": \"<how to confirm or rule it out>\"}]}. If you suspect nothing, \
     say so with an empty list."
}

/// One suspicion's session, after a reviewer's own brief: confirm it or
/// rule it out, with evidence, and look at nothing else.
pub fn verify_part(s: &Suspect) -> String {
    let at = match (&s.file, s.line) {
        (Some(f), Some(l)) => format!("{f}:{l}"),
        (Some(f), None) => f.clone(),
        _ => "not given".to_string(),
    };
    format!(
        "\n\n# This session: verify one suspicion\n\n\
         This lens's review is in two steps, and this session is the second, for one suspicion a \
         scout raised. Confirm it or rule it out, with evidence: read and run what you need, but \
         look for nothing else. What the scout says is a lead, not a finding.\n\n\
         Title: {title}\nWhere: {at}\nWhat's suspected: {claim}\nHow to check: {check}\n\n\
         If it holds, report it in the findings JSON above, with your evidence. If it doesn't, \
         answer with an empty list.",
        title = s.title.trim(),
        claim = s.claim.trim(),
        check = s.check.trim(),
    )
}

pub fn read_findings(text: &str) -> Result<Vec<Raw>> {
    Ok(one_json_in::<Reviewed>(text)?.findings)
}

/// A judged finding, merged across a lens's reviewers.
#[derive(Debug, Clone, Deserialize)]
pub struct Rating {
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default, deserialize_with = "lenient::line")]
    pub line: Option<u64>,
    #[serde(deserialize_with = "lenient::severity")]
    pub severity: Severity,
    /// It's a real defect, whatever caused it: false only for something
    /// that isn't a defect at all.
    #[serde(
        default = "yes",
        rename = "real_defect",
        deserialize_with = "lenient::flag"
    )]
    pub relevant: bool,
    #[serde(default = "yes", deserialize_with = "lenient::flag")]
    pub on_task: bool,
    #[serde(default)]
    pub reason: String,
    /// The reviewers disagreed about it.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub split: bool,
    /// Rejecting it is a values call, not a fact.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub values_call: bool,
    /// Both sides, when it's split or a values call.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub sides: Vec<String>,
    /// Facts the Judge states, from which rules decide what blocks. The
    /// defect was there before the change.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub pre_existing: bool,
    /// The change says it fixes it.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub claims_fix: bool,
    /// The change makes it worse.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub worsens: bool,
    /// A test the change adds or changes that can't fail when what it's
    /// there to pin breaks.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub weak_pin: bool,
    /// A claim the change makes that would lead someone relying on it to
    /// ship a defect or skip a test.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub misleads: bool,
    /// For `weak_pin`, the test's name as the change writes it. The rules
    /// hold the fact only when the change adds that text.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub weak_pin_test: Vec<String>,
    /// For `misleads`, the claim's own words, quoted from the change. The
    /// rules hold the fact only when the change adds that text.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub misleading_claim: Vec<String>,
    /// It's a blocker by the lens's own definition of one.
    #[serde(default, deserialize_with = "lenient::flag")]
    pub meets_blocker: bool,
    /// For `meets_blocker`, the words of the definition it meets. The rules
    /// hold the fact only when the definition has them.
    #[serde(default, deserialize_with = "lenient::strings")]
    pub blocker_clause: Vec<String>,
    /// A deliberate break of the code, a mutant, that changes nothing a
    /// user or caller could see. Only ever about a mutant.
    #[serde(
        default,
        rename = "equivalent_mutant",
        deserialize_with = "lenient::flag"
    )]
    pub equivalent: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct Judged {
    findings: Vec<Rating>,
}

/// The hunks a Judge's findings point at, added to its brief to start from:
/// fewer reads to find them, and nothing it can't still do.
pub fn judge_hunks(hunks: &str) -> String {
    format!(
        "\n# Where the findings point, to start from\n\nThe diff's hunks the findings above \
         name, or the files they mention, with the diff's own lines around them. They're a \
         starting point, not a limit: the whole change is still attached, and you have every \
         tool you had. Read callers, tests or other files whenever a finding depends on them.\n\n\
         ```diff\n{}```\n",
        hunks
    )
}

/// A Judge's brief, in the cache's order: the role's own text, the same for
/// every Judge, then the lens's, then this call's findings.
pub fn judge_parts(lens: &Lens, reviews: &[Vec<Raw>], not_findings: &str) -> Result<Vec<Part>> {
    let findings = serde_json::to_string_pretty(reviews)?;
    Ok(judge_parts_for(
        lens,
        not_findings,
        format!(
            "The findings, from {n} reviewer(s), one list each:\n{findings}\n\n",
            n = reviews.len()
        ),
        true,
    ))
}

/// A Judge's brief for one finding: its claim and the lines it cites, and
/// none of the reviewer's case for it, so no finding's framing reaches the
/// ruling on another, and none of the reviewer's reasoning reaches this one.
pub fn judge_finding_parts(lens: &Lens, claim: &Raw, cited: &str, not_findings: &str) -> Vec<Part> {
    let claim = serde_json::json!({
        "title": claim.title,
        "file": claim.file,
        "line": claim.line,
    });
    let cited = cited.trim();
    let cited = if cited.is_empty() {
        String::new()
    } else {
        format!("The lines it cites, to start from:\n\n```\n{cited}\n```\n\n")
    };
    judge_parts_for(
        lens,
        not_findings,
        format!(
            "One finding, by its claim alone, with the reviewer's reasoning left out: check it \
             against the code yourself.\n{}\n\n{cited}",
            serde_json::to_string_pretty(&claim).unwrap_or_default()
        ),
        false,
    )
}

fn judge_parts_for(lens: &Lens, not_findings: &str, these: String, several: bool) -> Vec<Part> {
    let attached = super::change::ATTACHED;
    let blocker_keys = if lens
        .blocker
        .as_deref()
        .is_some_and(|b| !b.trim().is_empty())
    {
        ", \"meets_blocker\", \"blocker_clause\""
    } else {
        ""
    };
    let mut parts = vec![part(
        Source::Fixed,
        format!(
            "You are a Judge of reviewers' findings about a change. {attached} Your working \
             directory is the repository, as the change leaves it. Check each finding against \
             the code. Change nothing in the repository: you repair nothing. To test a claim, \
             you may write and run a script, but only under $TMPDIR, which is yours for this call \
             and goes when it ends.\n\n\
             Merge findings that are the same defect into one. Rate each by what it costs, given \
             what the change says it's for: a defect that defeats that purpose, or a claim about \
             meeting it that the code and tests don't bear out, costs more than the same defect \
             elsewhere. Rate each as:\n\
             - Critical: wrong result, data loss, security hole\n\
             - High: real defect in a realistic case\n\
             - Medium: real defect in a narrow case\n\
             - Low: nit or unlikely edge case\n\
             - Rejected: a decided non-issue, with the reason\n\n\
             A real defect outside the lens's criteria is rated like any other, with `on_task` \
             false: don't reject it for that.\n\n\
             For each finding, also state these facts, each true or false, from the code: \
             `real_defect`, it's a real defect, whatever caused it and whichever lens it belongs \
             to: false only for something that isn't a defect at all. A missing test, or a \
             comment, doc or ledger line that's incomplete, is a real defect: rate it Low if it's \
             minor; `pre_existing`, the defect is in code as it was before the change, which the \
             change didn't add or alter: a test or line the change adds is never pre-existing; \
             `claims_fix`, the change says it fixes it; `worsens`, the change makes a \
             pre-existing defect worse; `weak_pin`, it's a test the change adds or changes that \
             can't fail when what it's there to pin breaks; `misleads`, it's a claim the change \
             makes, in a comment, doc, ledger line or test name, that would lead someone relying \
             on it to ship a defect or skip a test; with `weak_pin` give the test's name in \
             `weak_pin_test`, and with `misleads` the claim's exact words in `misleading_claim`, \
             both copied from lines the change adds: a fact without its quote counts as false; \
             `equivalent_mutant`, only for a deliberate break of the code (a mutant) that the \
             tests passed: true if that break changes nothing a user or caller could see, and \
             false for every other finding. Fixed rules decide from these facts what blocks: rate \
             the severity for what the defect costs on its own.\n\n\
             Set split when the reviewers disagree about a finding. Set values_call when \
             rejecting it comes down to values, not fact. Give both sides in `sides` when you set \
             either.\n\n"
        ),
    )];
    parts.push(part(
        Source::Config,
        format!(
            "# The lens: {id}\n\nIts criteria: {criteria}\n\n",
            id = lens.id,
            criteria = lens.criteria,
        ),
    ));
    if let Some(b) = lens
        .blocker
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        parts.push(part(
            Source::Config,
            format!(
                "In this lens, a blocker is: {b}\nState `meets_blocker`, true if a finding is a \
                 blocker by that definition, and copy into `blocker_clause` the words of the \
                 definition it meets: a fact without its quote counts as false. A defect that was \
                 there before the change, which the change neither claims to fix nor makes worse, \
                 doesn't block whatever it meets.\n\n"
            ),
        ));
    }
    parts.push(part(
        Source::Config,
        format!("# This call\n\nDecided non-issues:\n{not_findings}\n\n"),
    ));
    parts.push(part(
        if several { Source::Model } else { Source::Tool },
        these,
    ));
    parts.push(part(
        Source::Fixed,
        format!(
            "Answer with only JSON: {{\"findings\": [{{\"title\", \"detail\", \"file\", \"line\", \
             \"severity\", \"real_defect\", \"on_task\", \"reason\", \"split\", \"values_call\", \
             \"sides\", \"pre_existing\", \"claims_fix\", \"worsens\", \"weak_pin\", \
             \"weak_pin_test\", \"misleads\", \"misleading_claim\", \"equivalent_mutant\"{blocker_keys}}}]}}."
        ),
    ));
    parts
}

pub fn judge_prompt(lens: &Lens, reviews: &[Vec<Raw>], not_findings: &str) -> Result<String> {
    Ok(join(&judge_parts(lens, reviews, not_findings)?))
}

pub fn read_ratings(text: &str) -> Result<Vec<Rating>> {
    Ok(one_json_in::<Judged>(text)?.findings)
}

pub fn fix_prompt(blockers: &str) -> String {
    format!(
        "Fix these blockers in the working directory. Change only what fixing them needs, and \
         don't commit.\n\n{blockers}\n\n\
         Answer with only JSON: {{\"done\": \"<what you changed, in a line or two>\"}}."
    )
}

/// The synthesis between rounds of a review to convergence. The
/// Orchestrator holds the change's whole thread, which is why it synthesizes
/// and never attacks.
/// `writer_follows`: a test writer pins the fixes next, so the steelman
/// leaves that to it.
pub fn steelman_prompt(
    intent: &str,
    earlier: &str,
    blockers: &str,
    writer_follows: bool,
    lenses: &str,
) -> String {
    let pins = if writer_follows {
        "Leave the tests that pin these fixes to the test writer who follows you. Run only the \
         existing tests that cover what you change: no mutation checks or whole-suite runs, \
         which the review runs itself."
    } else {
        "Where a blocker comes with a test or scenario that shows it, keep that test as its \
         pin; where a blocker is a gap in the tests, write the test that closes it. Run only \
         the tests that cover what you change: no mutation checks or whole-suite runs."
    };
    let earlier = if earlier.is_empty() {
        "Nothing yet: this is the first.\n"
    } else {
        earlier
    };
    format!(
        "You synthesize this change between rounds of an adversarial review. Fresh reviewers, \
         each with a clean context, attacked it one dimension at a time, and a Judge upheld the \
         blockers below. Make the work the strongest version of itself: the version its most \
         capable advocate would stand behind. You never review it yourself; new reviewers will \
         attack what you leave, on every dimension.\n\n\
         {intent}\n\
         # What earlier rounds changed\n\n{earlier}\n\
         # Blockers\n\n\
         Fold each into the work in the working directory, so that a fresh reviewer finds \
         nothing in it to block, with the smallest change that does it: a fix that reaches \
         code the change didn't touch gives the next round more to attack. If you're sure one \
         is wrong, decline it and say why.\n\n\
         {blockers}\n\
         # How\n\n\
         Fold in these and nothing else: what the Judge upheld without blocking goes to a \
         person, not into the work. Keep the change to what it's for. Where the work claims \
         something about itself, in a comment, a test name, a doc or a ledger row, make the \
         claim true or take it out. {pins} Don't commit.\n\n\
         A declined finding is recorded as a disagreement, not a resolution.\n\n\
         # Brief the next round\n\n\
         You know what you just changed better than anyone the next round sends. For each \
         lens below, brief its reviewer: where your fixes could be wrong, or could have \
         broken something they touch, and how to test it here. Questions to settle, not \
         reassurance. Leave out a lens your fixes can't affect.\n\n{lenses}\n\
         Answer with only JSON: {{\"done\": \"<what you changed, in a few lines>\", \
         \"declined\": [{{\"finding\": \"<its id>\", \"reason\": \"\"}}], \
         \"briefs\": {{\"<lens id>\": \"<the brief>\"}}}}."
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Declined {
    pub finding: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct Fixed {
    pub done: String,
    /// A steelman's: what it left out, and why.
    #[serde(default)]
    pub declined: Vec<Declined>,
    /// A steelman's briefs for the next round's reviewers, by lens.
    #[serde(default)]
    pub briefs: std::collections::BTreeMap<String, String>,
}

pub fn read_fixed(text: &str) -> Result<Fixed> {
    json_in(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_json_among_other_words() {
        let text = "Here you go:\n```json\n{\"findings\": []}\n```\nDone.";
        assert!(read_findings(text).unwrap().is_empty());
    }

    #[test]
    fn a_judge_holds_a_lens_blocking_condition_and_false_claims() {
        let lens = Lens {
            id: "mutation".into(),
            criteria: "It blocks when the tests claim the behaviour is covered.".into(),
            attack: None,
            blocker: None,
            triggers: Default::default(),
            binding: None,
            severity: Default::default(),
            include_generated: false,
        };
        let prompt = judge_prompt(&lens, &[Vec::new()], "").unwrap();
        assert!(prompt.contains("It blocks when the tests claim the behaviour is covered."));
        assert!(prompt.contains("`weak_pin`"));
        assert!(prompt.contains("a fact without its quote counts as false"));
        assert!(prompt.contains("A missing test, or a comment, doc or ledger line"));
        assert!(prompt.contains("`pre_existing`"));
        assert!(prompt.contains("Fixed rules decide from these facts"));
    }

    #[test]
    fn a_judge_answer_survives_what_models_write() {
        let text = r#"Checked each claim.
```json
{"findings": [
  {"title": "Lost email", "file": "welcome.go", "line": "128-138", "severity": "high",
   "split": "true", "sides": "one reviewer says it's fine"},
  {"title": "Nit", "line": 7, "severity": "Low: an unlikely edge", "real_defect": "no"},
  {"title": "Noise", "line": null, "severity": "REJECTED", "reason": "decided"}
]}
```
Note: `{AGENT}` in the prompt is filled in by the bridge, so it's fine."#;
        let ratings = read_ratings(text).unwrap();
        assert_eq!(ratings.len(), 3);
        assert_eq!(ratings[0].line, Some(128));
        assert_eq!(ratings[0].severity, Severity::High);
        assert!(ratings[0].split);
        assert_eq!(ratings[0].sides, ["one reviewer says it's fine"]);
        assert_eq!(ratings[1].severity, Severity::Low);
        assert!(!ratings[1].relevant);
        assert_eq!(ratings[2].line, None);
        assert_eq!(ratings[2].severity, Severity::Rejected);
        let err = format!(
            "{:#}",
            read_ratings(r#"{"findings": [{"title": "x", "severity": "dire"}]}"#).unwrap_err()
        );
        assert!(err.contains("isn't a severity"), "{err}");
    }

    #[test]
    fn the_answer_is_the_first_object_that_fits() {
        let text = "An example: {\"note\": 1}. The answer: {\"findings\": []} {stray";
        assert!(read_findings(text).unwrap().is_empty());
    }

    #[test]
    fn an_example_list_before_the_real_one_is_no_answer() {
        let real = r#"{"findings": [{"title": "Lost email", "file": "a.go", "line": 3}]}"#;
        let text = format!("The answer looks like {{\"findings\": []}}. Mine:\n{real}");
        let err = format!("{:#}", read_findings(&text).unwrap_err());
        assert!(err.contains("2 different lists of findings"), "{err}");
        let judged = format!(
            "Like {{\"findings\": []}}, but: {}",
            r#"{"findings": [{"title": "x", "severity": "High"}]}"#
        );
        assert!(read_ratings(&judged).is_err());
        // The same list twice is still one answer.
        assert_eq!(
            read_findings(&format!("{real}\n\n{real}")).unwrap().len(),
            1
        );
        // Other answers still take the first object that fits.
        let fixed = read_fixed(r#"{"done": "a"} then {"done": "b"}"#).unwrap();
        assert_eq!(fixed.done, "a");
    }

    #[test]
    fn an_unreadable_answer_is_an_error_not_an_empty_result() {
        for text in [
            "no findings!",
            "",
            "{\"findings\": ",
            "}{",
            "{\"other\": 1}",
        ] {
            assert!(read_findings(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_brief_holds_what_a_reviewer_needs_and_only_that() {
        let lens = Lens {
            id: "lens-x".into(),
            criteria: "the criteria".into(),
            attack: None,
            blocker: None,
            triggers: Default::default(),
            binding: None,
            severity: Default::default(),
            include_generated: false,
        };
        let brief = reviewer_prompt(&lens, "the whole change", "- decided thing", false, "");
        for part in [
            "lens-x",
            "the criteria",
            "the whole change",
            "- decided thing",
        ] {
            assert!(brief.contains(part), "{part}");
        }
        assert!(brief.contains("Find what is wrong"));
        assert!(!brief.contains("check this"));
        assert!(brief.contains("change nothing") && !brief.contains("mutation"));
        // One with a copy of its own is told to run and break things there.
        let running = reviewer_prompt(
            &lens,
            "the whole change",
            "- decided thing",
            true,
            "# How to attack in this lens\n\nbreak it\n\n",
        );
        assert!(running.contains("# How to attack in this lens\n\nbreak it\n\nNot findings"));
        assert!(running.contains("your own copy") && running.contains("mutation"));
        assert!(!running.contains("change nothing"));
        // It's told to wait on a long run, not keep checking on one; a
        // reviewer that runs nothing isn't.
        assert!(running.contains(WAIT) && !brief.contains(WAIT));
        let probes = super::super::stages::probes_prompt(
            "the whole change",
            "",
            None,
            "",
            &super::super::stages::Carried::Nothing,
        );
        let claims = super::super::stages::claims_prompt(&[], "the whole change", "", true, "");
        assert!(probes.contains(WAIT) && claims.contains(WAIT));
    }

    #[test]
    fn a_rating_has_a_fixed_severity() {
        let ok = r#"{"findings": [{"title": "t", "severity": "High"}]}"#;
        assert_eq!(read_ratings(ok).unwrap()[0].severity, Severity::High);
        let bad = r#"{"findings": [{"title": "t", "severity": "Spicy"}]}"#;
        assert!(read_ratings(bad).is_err());
    }

    #[test]
    fn a_handoff_is_read_from_its_mark_or_its_field_and_a_bad_one_is_an_error() {
        let body = r#"{"findings": [{"title": "half", "file": "a.go", "line": "3"}], "handoff": {"checked": "the parser", "left": "the writer"}}"#;
        let (text, h) = handoff_of(&format!("{HANDOFF_MARK}\n{body}"))
            .unwrap()
            .unwrap();
        assert_eq!(text, body);
        assert_eq!(h.checked, "the parser");
        assert_eq!(h.left, "the writer");
        assert_eq!(
            earlier_of(&text),
            vec![Earlier {
                title: "half".into(),
                file: Some("a.go".into()),
                line: Some(3)
            }]
        );
        // Unmarked but with the field: a handoff all the same.
        assert!(handoff_of(body).unwrap().is_ok());
        // An answer: no handoff.
        assert!(handoff_of(r#"{"findings": []}"#).is_none());
        assert!(handoff_of("It all looks fine.").is_none());
        // Marked, but unreadable: an error, never an answer.
        let bad = handoff_of(&format!("{HANDOFF_MARK}\nI ran out of room.")).unwrap();
        assert!(format!("{:#}", bad.unwrap_err()).contains("handoff couldn't be read"));
        // A scout's suspects count as what it has, too.
        let scout = r#"{"suspects": [{"title": "S", "file": "b.go"}], "handoff": {"checked": "c", "left": "l"}}"#;
        assert_eq!(earlier_of(scout).len(), 1);
    }

    #[test]
    fn a_fresh_session_is_told_what_was_checked_left_and_found_after_its_brief() {
        let text = continuation(
            &["the parser".into(), "the writer".into()],
            "the flush, next its error path",
            &[Earlier {
                title: "lost write".into(),
                file: Some("a.go".into()),
                line: Some(9),
            }],
            3,
        );
        assert!(text.starts_with("\n\n# Carrying on from an earlier session"));
        assert!(text.contains("you are session 3"));
        assert!(text.contains("- Session 1: the parser\n- Session 2: the writer\n"));
        assert!(text.contains("the flush, next its error path"));
        assert!(text.contains("- lost write (a.go:9)"));
        assert!(continuation(&[], "x", &[], 2).contains("- Nothing yet."));
    }

    #[test]
    fn a_scout_lists_suspects_and_each_is_verified_from_its_own_brief() {
        let found = read_suspects(
            r#"{"suspects": [{"title": "S1", "file": "a.go", "line": 2, "claim": "c", "check": "k"}]}"#,
        )
        .unwrap();
        assert_eq!(found[0].title, "S1");
        assert_eq!(found[0].line, Some(2));
        assert!(read_suspects(r#"{"findings": []}"#).is_err());
        assert!(scout_part().contains("\"suspects\""));
        let v = verify_part(&found[0]);
        assert!(v.contains("Title: S1\nWhere: a.go:2\nWhat's suspected: c\nHow to check: k"));
        assert!(v.contains("findings JSON above"));
    }
}
