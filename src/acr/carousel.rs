//! What a review waiting for a person asks them, as question carousels: the
//! shape Claude Code's AskUserQuestion takes, so that a session shows them
//! as they are, and a terminal or another agent shows them as a numbered
//! list. Each answer is applied as the person decided it, and each verdict
//! on a finding is kept as feedback for the repository's calibration.
//!
//! The rules guide; the person decides. A ruling is offered as it stands,
//! with what each other choice does, and nothing is chosen for them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::{Action, Config, Reattack, Severity};
use super::configure::{Feedback, Kept, Verdict};
use super::flow::{
    About, Finding, FindingState, Fixer, FlagState, Gate, Outcome, Reported, Run, Stage, Status,
};

/// The most questions one carousel holds.
const PER_CAROUSEL: usize = 4;
/// The most findings one "should these block?" question offers.
const PER_QUESTION: usize = 3;
/// The most of a finding's detail a question quotes.
const DETAIL: usize = 700;

const APPROVE: &str = "Approve";
const HIGHER: &str = "One tier higher";
const STOP: &str = "Stop the review";
const GO_ON: &str = "Go on";
const FIX: &str = "Fix it";
const WAIVE: &str = "Waive it";
const DEFER: &str = "Defer to a ticket";
const NOT_BLOCKER: &str = "Not a blocker";
const NOT_DEFECT: &str = "Not a defect";
const AS_RULED: &str = "As ruled";
const BLOCKS: &str = "It blocks";
const AGAIN: &str = "Try it again";
const WITHOUT: &str = "Go on without it";
const ONE_MORE: &str = "One more round";
const ANOTHER: &str = "Another round";
const NOTHING: &str = "Nothing missed";
const NOT_NOW: &str = "Not now";
const NONE: &str = "None of them";
const SESSION: &str = "This session";
const ONLY_BLOCKED: &str = "Only lenses with blockers";
const EVERY_LENS: &str = "Every lens";
const ORCHESTRATOR: &str = "The Orchestrator";
const KEEP_THEM: &str = "Keep them";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    /// At most 12 characters: the carousel's tab.
    pub header: String,
    /// 2 to 4.
    pub options: Vec<Choice>,
    #[serde(rename = "multiSelect")]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub label: String,
    pub description: String,
}

/// What a question decides, to apply its answer.
#[derive(Debug, Clone, PartialEq)]
enum Topic {
    /// An escalation, by its place in the run's list.
    Escalation(usize),
    Scope,
    Fixer,
    /// Which lenses attack again after a fix, when nothing chose it.
    Reattack,
    Blocker {
        lens: String,
        id: String,
    },
    ShouldBlock(Vec<(String, String)>),
    /// Files a fix changed that are flagged, by their place in the run's
    /// list: each kept or reverted.
    Flags(Vec<usize>),
    Missed,
    Final,
}

impl Topic {
    /// It must be answered for the review to go on. Whether logged findings
    /// should block is: an answer left out would keep them logged, and a
    /// review could end confirmed with a person's choice to block lost.
    fn required(&self) -> bool {
        // Left unanswered, re-attack keeps its default, and the report says
        // nobody chose.
        !matches!(self, Topic::Missed | Topic::Reattack)
    }
}

struct Asked {
    topic: Topic,
    question: Question,
}

/// What `acr ask` prints: everything a review waiting for a person asks.
#[derive(Debug, Serialize)]
pub struct Ask {
    pub run: String,
    pub gate: Option<&'static str>,
    pub summary: String,
    /// Each one an AskUserQuestion call, in order.
    pub carousels: Vec<Carousel>,
    /// What to do with the answers, or with the fixes.
    pub then: String,
}

#[derive(Debug, Serialize)]
pub struct Carousel {
    pub questions: Vec<Question>,
}

fn choice(label: &str, description: impl Into<String>) -> Choice {
    Choice {
        label: label.to_string(),
        description: description.into(),
    }
}

fn question(header: &str, text: String, options: Vec<Choice>, multi: bool) -> Question {
    debug_assert!(header.chars().count() <= 12, "{header}");
    debug_assert!((2..=4).contains(&options.len()), "{text}");
    Question {
        question: text,
        header: header.to_string(),
        options,
        multi_select: multi,
    }
}

fn cut(text: &str, most: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= most {
        return text.to_string();
    }
    let kept: String = text.chars().take(most).collect();
    format!("{}…", kept.trim_end())
}

/// A finding by its ID, with its lens: the latest of its rounds that has it.
fn finding<'a>(run: &'a Run, id: &str) -> Option<(&'a str, &'a Finding)> {
    run.lenses.values().find_map(|l| {
        l.rounds
            .iter()
            .rev()
            .find_map(|r| r.findings.iter().find(|f| f.id == id))
            .map(|f| (l.id.as_str(), f))
    })
}

fn finding_mut<'a>(run: &'a mut Run, id: &str) -> Option<&'a mut Finding> {
    run.lenses.values_mut().find_map(|l| {
        l.rounds
            .iter_mut()
            .rev()
            .find_map(|r| r.findings.iter_mut().find(|f| f.id == id))
    })
}

/// Each lens's latest round's findings in `state`, a defect found by more
/// than one lens once.
fn latest(run: &Run, keep: impl Fn(&Finding) -> bool) -> Vec<(String, Finding)> {
    let all: Vec<(String, Finding)> = run
        .lenses
        .values()
        .filter_map(|l| l.rounds.last().map(|r| (l.id.clone(), r)))
        .flat_map(|(lens, r)| {
            r.findings
                .iter()
                .filter(|f| keep(f))
                .map(move |f| (lens.clone(), f.clone()))
        })
        .collect();
    all.iter()
        .filter(|(_, f)| {
            f.same_as
                .as_ref()
                .is_none_or(|p| !all.iter().any(|(_, g)| &g.id == p))
        })
        .cloned()
        .collect()
}

/// The note in an answer that defers a blocker: "Defer to a ticket", then
/// whatever follows it, past a colon or a dash. None if it doesn't defer.
fn deferred_note(text: &str) -> Option<String> {
    let text = text.trim();
    let head = text.get(..DEFER.len())?;
    if !head.eq_ignore_ascii_case(DEFER) {
        return None;
    }
    let rest = text[DEFER.len()..].trim_start_matches(|c: char| {
        c.is_whitespace() || matches!(c, ':' | '-' | '—' | '–' | ',' | '.')
    });
    Some(rest.trim().to_string())
}

fn open_blocker(f: &Finding) -> bool {
    f.blocks() && f.state == FindingState::Open
}

fn who(run: &Run) -> &'static str {
    match run.fixer {
        Fixer::Session => "This session",
        Fixer::Orchestrator => "The Orchestrator",
    }
}

/// Which lenses attack again after each fix: an intensity, asked when the
/// config and the run's flags leave it unsaid.
fn reattack_question(run: &Run) -> Question {
    let lenses = run.lenses.len();
    question(
        "Re-attack",
        format!(
            "After each fix, which of the {lenses} lens(es) attack the new version? Set \
             `reattack` in the config to stop being asked."
        ),
        vec![
            choice(
                ONLY_BLOCKED,
                "The default. A lens that passed isn't attacked again: later rounds cost less, \
                 and a fix that breaks what a passed lens covers can go unseen",
            ),
            choice(
                EVERY_LENS,
                "Every lens attacks every fixed version, so a fix that breaks another lens is \
                 caught; each round after a fix costs about as much as the first",
            ),
        ],
        false,
    )
}

/// Who folds in what a review upholds: the default first, and marked, so
/// the answer most people give is the one at hand.
fn fixer(run: &Run) -> Question {
    let default = |is: bool, what: &str| {
        if is {
            format!("The default. {what}")
        } else {
            what.to_string()
        }
    };
    let session = choice(
        SESSION,
        default(
            run.fixer == Fixer::Session,
            "The session that ran the review fixes them, with the change's context, then \
             resumes it with --fixed",
        ),
    );
    let orchestrator = choice(
        ORCHESTRATOR,
        default(
            run.fixer == Fixer::Orchestrator,
            "A fresh agent on the Orchestrator's binding fixes them",
        ),
    );
    let options = match run.fixer {
        Fixer::Orchestrator => vec![orchestrator, session],
        Fixer::Session => vec![session, orchestrator],
    };
    // The text answers are keyed by stays as it was.
    question(
        "Fixer",
        "Who folds in the blockers this review upholds?".into(),
        options,
        false,
    )
}

fn asked(run: &Run, cfg: &Config) -> Vec<Asked> {
    let mut out = Vec::new();
    if run.status != Status::Waiting || run.waiting == Some(Gate::Fix) {
        return out;
    }
    for (n, e) in run.escalations.iter().enumerate().filter(|(_, e)| !e.seen) {
        out.push(Asked {
            topic: Topic::Escalation(n),
            question: escalation(run, &e.reason, e.about.as_ref()),
        });
    }
    out.extend(flagged(run));
    match run.stage {
        Stage::Scope => {
            out.push(Asked {
                topic: Topic::Scope,
                question: scope(run, cfg),
            });
            if cfg
                .binding(super::config::Role::Orchestrator, None)
                .is_ok_and(|b| b.writes)
            {
                out.push(Asked {
                    topic: Topic::Fixer,
                    question: fixer(run),
                });
            }
            if run.reattack.is_none() {
                out.push(Asked {
                    topic: Topic::Reattack,
                    question: reattack_question(run),
                });
            }
        }
        Stage::Decide => {
            for (n, (lens, f)) in latest(run, open_blocker).into_iter().enumerate() {
                out.push(Asked {
                    question: blocker(run, n + 1, &lens, &f),
                    topic: Topic::Blocker { lens, id: f.id },
                });
            }
            out.extend(after_round(run));
        }
        Stage::Final => {
            let lenses: Vec<&str> = run.lenses.keys().map(String::as_str).collect();
            out.push(Asked {
                topic: Topic::Final,
                question: question(
                    "Result",
                    format!(
                        "Every lens agrees, after {} round(s): {}. Approve the result?",
                        run.round,
                        lenses.join(", ")
                    ),
                    vec![
                        choice(APPROVE, "The review ends, confirmed"),
                        choice(
                            ANOTHER,
                            "Every lens attacks the change again, with fresh reviewers",
                        ),
                    ],
                    false,
                ),
            });
            out.extend(after_round(run));
        }
        _ => {}
    }
    unique(&mut out);
    out
}

/// The files fixes changed that are flagged and not yet answered, a few to
/// a question, by the round whose fix changed them: the person chooses any
/// to revert, and the rest stay as the fix left them.
fn flagged(run: &Run) -> Vec<Asked> {
    let open: Vec<usize> = (0..run.flags.len())
        .filter(|&i| run.flags[i].state == FlagState::Open)
        .collect();
    let mut chunks: Vec<Vec<usize>> = Vec::new();
    for &i in &open {
        match chunks.last_mut() {
            Some(c) if c.len() < PER_QUESTION && run.flags[c[0]].round == run.flags[i].round => {
                c.push(i)
            }
            _ => chunks.push(vec![i]),
        }
    }
    let mut out = Vec::new();
    let mut first = 1;
    for (n, chunk) in chunks.iter().enumerate() {
        let round = run.flags[chunk[0]].round;
        let mut options: Vec<Choice> = chunk
            .iter()
            .map(|&i| {
                let f = &run.flags[i];
                choice(
                    &f.path,
                    cut(
                        &format!("{}. Reverting it runs: {}", f.why(), f.command),
                        DETAIL,
                    ),
                )
            })
            .collect();
        options.push(choice(KEEP_THEM, "They stay as the fix left them"));
        let last = first + chunk.len() - 1;
        let which = if open.len() == 1 {
            String::new()
        } else if first == last {
            format!(" (flagged file {first} of {})", open.len())
        } else {
            format!(" (flagged files {first}-{last} of {})", open.len())
        };
        let what = if chunk.len() == 1 {
            "a file that's a binary or that its blockers don't point at. Choose it to revert it \
             to how it was before that fix, or keep it"
        } else {
            "these files, each a binary or a file its blockers don't point at. Choose any to \
             revert to how it was before that fix; the rest stay as the fix left them"
        };
        out.push(Asked {
            topic: Topic::Flags(chunk.clone()),
            question: question(
                &if chunks.len() > 1 {
                    format!("Flagged {}", n + 1)
                } else {
                    "Flagged".to_string()
                },
                format!(
                    "Round {round}'s fix changed {what}{which}. Reviewers who attacked the \
                     change since saw it as the fix left it."
                ),
                options,
                true,
            ),
        });
        first = last + 1;
    }
    out
}

/// Answers are keyed by a question's text, so no two may share it: a later
/// one that would gets its header added.
fn unique(asked: &mut [Asked]) {
    for i in 1..asked.len() {
        let (before, rest) = asked.split_at_mut(i);
        let q = &mut rest[0].question;
        if before.iter().any(|a| a.question.question == q.question) {
            q.question = format!("{} [{}]", q.question, q.header);
        }
    }
}

fn escalation(run: &Run, reason: &str, about: Option<&About>) -> Question {
    match about {
        Some(About::Finding { finding: id, .. }) => {
            let sides: String = run
                .disagreements
                .iter()
                .filter(|d| &d.finding == id)
                .flat_map(|d| d.sides.iter())
                .map(|side| format!("\n- {}", cut(side, 300)))
                .collect();
            let blocks = finding(run, id)
                .is_some_and(|(_, f)| f.blocks() && f.state != FindingState::Waived);
            let header = if reason.starts_with("a values call") {
                "Values call"
            } else if reason.starts_with("reviewers split") {
                "Split"
            } else {
                "Ruling"
            };
            let ruled = if blocks { "a blocker" } else { "not to block" };
            let text = format!(
                "{reason}{}\n\nIt's ruled {ruled}. What's your call?",
                if sides.is_empty() {
                    String::new()
                } else {
                    format!("\n\nThe sides:{sides}")
                }
            );
            let options = if blocks {
                vec![
                    choice(BLOCKS, "It stays a blocker, and is fixed"),
                    choice(
                        NOT_DEFECT,
                        "Drop it: the reviewers were wrong, and the calibration learns it",
                    ),
                    choice(STOP, "End the review here"),
                ]
            } else {
                vec![
                    choice(AS_RULED, "It doesn't block"),
                    choice(BLOCKS, "It's fixed, and the next round checks the fix"),
                    choice(STOP, "End the review here"),
                ]
            };
            question(header, text, options, false)
        }
        Some(About::Unfinished { .. }) => question(
            "Unfinished",
            format!("{reason}\n\nWhat now?"),
            vec![
                choice(AGAIN, "Its reviewers run again next round"),
                choice(
                    WITHOUT,
                    "The review goes on without this lens, which is recorded as waived",
                ),
                choice(STOP, "End the review here"),
            ],
            false,
        ),
        Some(About::MaxRounds) => question(
            "Max rounds",
            reason.to_string(),
            vec![
                choice(
                    ONE_MORE,
                    "Fold in its blockers and attack the fix once more",
                ),
                choice(STOP, "End here, with the blockers open"),
            ],
            false,
        ),
        Some(About::Unfixed) => question(
            "Unfixed",
            reason.to_string(),
            vec![
                choice(AGAIN, "The Orchestrator tries the fix again"),
                choice(
                    SESSION,
                    "The session that ran the review fixes them, then resumes it with --fixed",
                ),
                choice(STOP, "End the review here"),
            ],
            false,
        ),
        None => question(
            "Escalated",
            reason.to_string(),
            vec![
                choice(GO_ON, "Go on with the review"),
                choice(STOP, "End the review here"),
            ],
            false,
        ),
    }
}

fn scope(run: &Run, cfg: &Config) -> Question {
    let lenses: Vec<&str> = run.lenses.keys().map(String::as_str).collect();
    let mut options = vec![choice(APPROVE, "Its reviewers start")];
    let at = cfg.tiers.iter().position(|t| t.id == run.tier);
    if let Some(next) = at.and_then(|at| cfg.tiers.get(at + 1)) {
        options.push(choice(
            HIGHER,
            format!(
                "Tier {}: {} reviewer(s) a lens, and any lenses it brings, asked about again \
                 before anything runs",
                next.id, next.fan_out
            ),
        ));
    }
    options.push(choice(STOP, "End the review before any reviewer runs"));
    question(
        "Scope",
        format!(
            "Review at tier {} ({}), with {} lens(es), {} reviewer(s) each: {}. Go ahead?",
            run.tier,
            run.tier_why,
            lenses.len(),
            run.fan_out,
            lenses.join(", ")
        ),
        options,
        false,
    )
}

/// The findings in each lens's latest round that are the same defect as
/// `f`, as (lens, place), and what to say of them in its question:
/// " (also found by beta)", with each place when it differs from `f`'s,
/// and how many lenses and places in all.
fn found_also(run: &Run, f: &Finding) -> String {
    let same: Vec<(String, String)> = run
        .lenses
        .values()
        .flat_map(|l| {
            l.rounds.last().into_iter().flat_map(move |r| {
                r.findings
                    .iter()
                    .filter(|g| g.same_as.as_deref() == Some(&f.id))
                    .map(move |g| (l.id.clone(), super::flow::place(g)))
            })
        })
        .collect();
    if same.is_empty() {
        return String::new();
    }
    let here = super::flow::place(f);
    let also: Vec<String> = same
        .iter()
        .map(|(l, at)| {
            if *at == here {
                l.clone()
            } else {
                format!("{l} at {at}")
            }
        })
        .collect();
    let mut places = vec![here];
    for (_, at) in &same {
        if !places.contains(at) {
            places.push(at.clone());
        }
    }
    let lenses: BTreeSet<&str> = same.iter().map(|(l, _)| l.as_str()).collect();
    if places.len() > 1 {
        format!(
            " (also found by {}: found by {} lenses at {} places)",
            also.join(", "),
            lenses.len() + 1,
            places.len()
        )
    } else {
        format!(" (also found by {})", also.join(", "))
    }
}

fn blocker(run: &Run, n: usize, lens: &str, f: &Finding) -> Question {
    let also = found_also(run, f);
    let at = match (&f.file, f.line) {
        (Some(file), Some(line)) => format!("At {file}:{line}. "),
        (Some(file), None) => format!("In {file}. "),
        _ => String::new(),
    };
    let waivable = f.action == Action::BlocksUnlessWaived;
    // A question holds four choices at most, so waiving, which ships the
    // change with the defect and no follow-up, is an answer to type rather
    // than a choice: deferring to a ticket is what most people want.
    let text = format!(
        "{}, {:?} in {lens}{}: {}\n\n{}\n\n{at}Choose Other to have it fixed with your note \
         to the fixer, or answer \"{DEFER}: <your note>\" to defer it with a note.{}",
        f.id,
        f.severity,
        also,
        f.title,
        cut(&f.detail, DETAIL),
        if waivable {
            format!(" To ship with it and no ticket, answer \"{WAIVE}\".")
        } else {
            String::new()
        }
    );
    let mut options = vec![
        choice(
            FIX,
            format!("{} fixes it, and the next round checks the fix", who(run)),
        ),
        choice(
            DEFER,
            "It's real, but this change ships without the fix: it's recorded as deferred, \
             and a ticket is drafted for the follow-up",
        ),
    ];
    if waivable {
        options.push(choice(
            NOT_BLOCKER,
            "It's real, but shouldn't block: it's logged, and the calibration learns it",
        ));
    }
    options.push(choice(
        NOT_DEFECT,
        "Drop it: the reviewers were wrong, and the calibration learns it",
    ));
    question(&format!("Blocker {n}"), text, options, false)
}

/// What's asked at the end of a round, besides its blockers: whether a
/// logged finding should block, and what the reviewers missed.
/// The findings in each lens's latest round that are the same defect as
/// `id`, found by another lens, as (lens, id).
fn same_as(run: &Run, id: &str) -> Vec<(String, String)> {
    run.lenses
        .values()
        .filter_map(|l| l.rounds.last().map(|r| (l.id.clone(), r)))
        .flat_map(|(lens, r)| {
            r.findings
                .iter()
                .filter(|g| g.same_as.as_deref() == Some(id))
                .map(move |g| (lens.clone(), g.id.clone()))
        })
        .collect()
}

fn after_round(run: &Run) -> Vec<Asked> {
    let mut out = Vec::new();
    // Every logged finding reaches the person: one found by several lenses
    // once, naming the others, and the rejected counted.
    let logged = latest(run, |f| f.state == FindingState::Logged);
    let in_round = |keep: &dyn Fn(&Finding) -> bool| {
        run.lenses
            .values()
            .filter_map(|l| l.rounds.last())
            .flat_map(|r| r.findings.iter())
            .filter(|f| keep(f))
            .count()
    };
    let all_logged = in_round(&|f| f.state == FindingState::Logged);
    let rejected = in_round(&|f| f.state == FindingState::Dropped);
    let mut counted = Vec::new();
    if all_logged > logged.len() {
        counted.push(format!(
            "{} more are the same defect another lens found, shown with it",
            all_logged - logged.len()
        ));
    }
    if rejected > 0 {
        counted.push(format!("{rejected} rejected, not shown"));
    }
    let counted = if counted.is_empty() {
        String::new()
    } else {
        format!(" ({})", counted.join("; "))
    };
    let chunks: Vec<&[(String, Finding)]> = logged.chunks(PER_QUESTION).collect();
    let mut first = 1;
    for (n, chunk) in chunks.iter().enumerate() {
        let mut options: Vec<Choice> = chunk
            .iter()
            .map(|(lens, f)| {
                let others: Vec<String> = same_as(run, &f.id)
                    .into_iter()
                    .map(|(l, id)| format!("{l} ({id})"))
                    .collect();
                let also = if others.is_empty() {
                    String::new()
                } else {
                    format!("; also found by {}", others.join(", "))
                };
                choice(
                    &f.id,
                    cut(
                        &format!("{:?} in {lens}: {}{also}", f.severity, f.title),
                        300,
                    ),
                )
            })
            .collect();
        options.push(choice(NONE, "They stay logged"));
        let last = first + chunk.len() - 1;
        // Numbered, so no two questions share their text: answers are keyed
        // by it.
        let which = if logged.len() == 1 {
            "The logged finding".to_string()
        } else if first == last {
            format!("Logged finding {first} of {}", logged.len())
        } else {
            format!("Logged findings {first}-{last} of {}", logged.len())
        };
        out.push(Asked {
            topic: Topic::ShouldBlock(
                chunk
                    .iter()
                    .map(|(lens, f)| (lens.clone(), f.id.clone()))
                    .collect(),
            ),
            question: question(
                &if chunks.len() > 1 {
                    format!("Logged {}", n + 1)
                } else {
                    "Logged".to_string()
                },
                format!(
                    "{which}{}: should any of these block? They're upheld, but logged, not \
                     blocking.",
                    if n == 0 { counted.as_str() } else { "" }
                ),
                options,
                true,
            ),
        });
        first = last + 1;
    }
    out.push(Asked {
        topic: Topic::Missed,
        question: question(
            "Missed",
            format!(
                "Did the reviewers miss a defect? Choose Other and describe it: {} fixes it, \
                 and every lens attacks the fix.",
                who(run).to_lowercase()
            ),
            vec![
                choice(NOTHING, "They missed nothing I can see"),
                choice(NOT_NOW, "Go on without saying"),
            ],
            false,
        ),
    });
    out
}

/// Everything a waiting review asks, in carousels of at most four.
pub fn ask(run: &Run, cfg: &Config, app: &str) -> Ask {
    let id = &run.id;
    if run.waiting == Some(Gate::Fix) {
        return Ask {
            run: id.clone(),
            gate: Some(Gate::Fix.as_str()),
            summary: format!(
                "Waiting for you to fold these in, in the working tree:\n{}",
                super::flow::fix_brief(run)
            ),
            carousels: Vec::new(),
            then: format!(
                "Fix each one, write tests that pin the fixes, then `{app} acr resume {id} \
                 --fixed`, with `--brief <file>` to brief the next round's lenses on what \
                 changed. `--stop` ends the review instead."
            ),
        };
    }
    let all = asked(run, cfg);
    let carousels: Vec<Carousel> = all
        .chunks(PER_CAROUSEL)
        .map(|c| Carousel {
            questions: c.iter().map(|a| a.question.clone()).collect(),
        })
        .collect();
    let summary = match (run.status, run.waiting) {
        (Status::Waiting, Some(gate)) => format!(
            "Review {id} is waiting for you at its {} gate, round {}: {} question(s), in {} \
             carousel(s).",
            gate.as_str(),
            run.round,
            all.len(),
            all.len().div_ceil(PER_CAROUSEL)
        ),
        _ => format!("Review {id} isn't waiting for anyone."),
    };
    Ask {
        run: id.clone(),
        gate: run.waiting.map(Gate::as_str),
        summary,
        then: if carousels.is_empty() {
            String::new()
        } else {
            format!(
                "Show all {} carousel(s) to your user, each with one AskUserQuestion call, \
                 exactly as given and in order, and never answer for them. Then pass every \
                 answer, from every carousel, keyed by its question's text (or by \"[header] \
                 text\"), as one JSON object: `{app} acr resume {id} --answers '<json>'`, or \
                 `--answers @<file>`. An answer left out of a question that needs one, or one \
                 for no question, is refused, so nothing is lost.",
                carousels.len()
            )
        },
        carousels,
    }
}

/// The same questions as numbered text, for a terminal or an agent that
/// has no question tool.
pub fn as_text(ask: &Ask) -> String {
    let mut out = format!("{}\n", ask.summary);
    let mut n = 0;
    for carousel in &ask.carousels {
        for q in &carousel.questions {
            n += 1;
            out.push_str(&format!("\n{n}. [{}] {}\n", q.header, q.question));
            for (i, o) in q.options.iter().enumerate() {
                out.push_str(&format!("   {}) {}: {}\n", i + 1, o.label, o.description));
            }
            if q.multi_select {
                out.push_str("   Choose any, with commas.\n");
            }
        }
    }
    if !ask.then.is_empty() {
        out.push_str(&format!("\n{}\n", ask.then));
    }
    out
}

/// An answer to one question: the choices, by label, or text of its own.
#[derive(Debug, Clone, PartialEq)]
enum Picked {
    Chose(Vec<String>),
    Other(String),
}

fn picked(q: &Question, value: &Value) -> Picked {
    let named = |text: &str| {
        q.options
            .iter()
            .find(|o| o.label.eq_ignore_ascii_case(text.trim()))
            .map(|o| o.label.clone())
    };
    let parts: Vec<String> = match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Value::String(text) => {
            if named(text).is_some() || !q.multi_select {
                vec![text.clone()]
            } else {
                text.split(',').map(|p| p.trim().to_string()).collect()
            }
        }
        other => vec![other.to_string()],
    };
    let chose: Vec<Option<String>> = parts.iter().map(|p| named(p)).collect();
    if !parts.is_empty() && chose.iter().all(Option::is_some) {
        Picked::Chose(chose.into_iter().flatten().collect())
    } else {
        Picked::Other(parts.join(", "))
    }
}

/// Whether answer `key` is for `q`: its text, as given or trimmed; its first
/// line, which an agent may have kept alone; or `[header] text`, as a
/// refusal names a question.
fn keyed(key: &str, q: &Question) -> bool {
    let key = key.trim();
    let text = q.question.trim();
    let tagged = format!("[{}] {text}", q.header);
    key == text
        || key == tagged
        || key.lines().next().unwrap_or_default().trim()
            == text.lines().next().unwrap_or_default().trim()
}

/// The answer to `q` among `answers`, by [`keyed`], its exact text first.
fn answer_to<'a>(answers: &'a BTreeMap<String, Value>, q: &Question) -> Option<&'a Value> {
    answers
        .get(&q.question)
        .or_else(|| answers.iter().find(|(k, _)| keyed(k, q)).map(|(_, v)| v))
}

/// What applying answers did, for whoever ran the review.
#[derive(Debug, Default)]
pub struct Applied {
    /// Each verdict on a finding, and each defect reported missed, as
    /// feedback for the calibration.
    pub feedback: Vec<Feedback>,
    /// The person ended the review.
    pub stopped: bool,
    /// What a person should hear about how their answers were applied.
    pub notes: Vec<String>,
}

/// Applies a person's answers to the questions the review asks now.
pub fn apply(
    run: &mut Run,
    cfg: &Config,
    answers: &BTreeMap<String, Value>,
    at: i64,
) -> Result<Applied> {
    let all = asked(run, cfg);
    if all.is_empty() {
        bail!(
            "review {} asks nothing now{}",
            run.id,
            if run.waiting == Some(Gate::Fix) {
                ": it waits for the fixes, so resume it with --fixed"
            } else {
                ""
            }
        );
    }
    // An answer that's for no question would be lost without a word.
    let stray: Vec<&String> = answers
        .keys()
        .filter(|k| !all.iter().any(|a| keyed(k, &a.question)))
        .collect();
    if !stray.is_empty() {
        let asked: Vec<String> = all
            .iter()
            .map(|a| format!("[{}] {}", a.question.header, a.question.question))
            .collect();
        bail!(
            "these answers are for no question it asks: {}. Key each answer by its question's \
             text, or by \"[header] text\". It asks:\n- {}",
            stray
                .iter()
                .map(|k| format!("\"{k}\""))
                .collect::<Vec<_>>()
                .join(", "),
            asked.join("\n- ")
        );
    }
    let mut missing = Vec::new();
    let mut picks: Vec<(Topic, Question, Option<Picked>)> = Vec::new();
    for a in all {
        let pick = answer_to(answers, &a.question).map(|v| picked(&a.question, v));
        if pick.is_none() && a.topic.required() {
            missing.push(format!("[{}] {}", a.question.header, a.question.question));
        }
        picks.push((a.topic, a.question, pick));
    }
    if !missing.is_empty() {
        bail!(
            "these need an answer, keyed by the question's text or by \"[header] text\":\n- {}",
            missing.join("\n- ")
        );
    }
    // Free text only means something where a question says what it does.
    for (topic, q, pick) in &picks {
        if let (Some(Picked::Other(text)), false) =
            (pick, matches!(topic, Topic::Blocker { .. } | Topic::Missed))
        {
            let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
            bail!(
                "\"{text}\" doesn't answer [{}] {}: choose one of {}",
                q.header,
                q.question,
                labels.join(", ")
            );
        }
    }
    let chose = |pick: &Option<Picked>, label: &str| matches!(pick, Some(Picked::Chose(c)) if c.iter().any(|l| l == label));
    let mut applied = Applied::default();
    let verdict = |run: &Run, id: &str, v: Verdict, note: Option<String>| {
        finding(run, id).map(|(lens, f)| Feedback {
            at,
            run: run.id.clone(),
            finding: Some(Kept::of(lens, f)),
            verdict: Some(v),
            missed: None,
            lens: None,
            note,
        })
    };
    let stop = picks.iter().any(|(_, _, p)| chose(p, STOP));
    for e in run.escalations.iter_mut() {
        e.seen = true;
    }
    let mut fold = false;
    let mut blocked = 0;
    for (topic, _, pick) in &picks {
        match topic {
            Topic::Escalation(n) => match run.escalations[*n].about.clone() {
                Some(About::Finding { lens, finding: id }) => {
                    let blocks = finding(run, &id).is_some_and(|(_, f)| f.blocks());
                    if chose(pick, BLOCKS) && !blocks {
                        to_blocker(run, cfg, &lens, &id);
                        applied
                            .feedback
                            .extend(verdict(run, &id, Verdict::ShouldBlock, None));
                        fold |= run.stage != Stage::Decide;
                    } else if chose(pick, NOT_DEFECT) {
                        applied
                            .feedback
                            .extend(verdict(run, &id, Verdict::NotADefect, None));
                        if let Some(f) = finding_mut(run, &id) {
                            f.state = FindingState::Dropped;
                            f.reason.push_str(" A person ruled it isn't a defect.");
                        }
                    } else if !stop {
                        applied
                            .feedback
                            .extend(verdict(run, &id, Verdict::Right, None));
                    }
                }
                Some(About::Unfinished { lens }) if chose(pick, WITHOUT) => {
                    if let Some(l) = run.lenses.get_mut(&lens) {
                        l.outcome = Outcome::Waived;
                        l.done.push(
                            "waived: its reviewers didn't finish, and a person chose to go on \
                             without it"
                                .into(),
                        );
                    }
                }
                Some(About::MaxRounds) if chose(pick, ONE_MORE) => {
                    run.stage = if cfg.human_gate.asks_fold() {
                        Stage::Decide
                    } else {
                        fold = true;
                        Stage::Fix
                    };
                }
                Some(About::Unfixed) if chose(pick, AGAIN) => run.stage = Stage::Fix,
                Some(About::Unfixed) if chose(pick, SESSION) => {
                    run.fixer = Fixer::Session;
                    run.stage = Stage::Fix;
                }
                _ => {}
            },
            Topic::Scope if chose(pick, APPROVE) => run.stage = Stage::Review,
            Topic::Scope if chose(pick, HIGHER) => {
                let at = cfg.tiers.iter().position(|t| t.id == run.tier);
                if let Some(next) = at.and_then(|at| cfg.tiers.get(at + 1)) {
                    run.tier_why = format!("one tier higher than {}, as you chose", run.tier);
                    run.tier = next.id.clone();
                    run.fan_out = next.fan_out;
                    run.rescope = true;
                }
            }
            Topic::Fixer if chose(pick, SESSION) => run.fixer = Fixer::Session,
            Topic::Fixer if chose(pick, ORCHESTRATOR) => run.fixer = Fixer::Orchestrator,
            Topic::Reattack if chose(pick, ONLY_BLOCKED) => run.reattack = Some(Reattack::Blocked),
            Topic::Reattack if chose(pick, EVERY_LENS) => run.reattack = Some(Reattack::All),
            Topic::Blocker { id, .. } => {
                let waivable =
                    finding(run, id).is_some_and(|(_, f)| f.action == Action::BlocksUnlessWaived);
                let (state, v, note) = match pick {
                    Some(Picked::Other(text)) => match deferred_note(text) {
                        Some(note) => (FindingState::Deferred, Verdict::Right, note),
                        None if text.trim().eq_ignore_ascii_case(WAIVE) => {
                            if !waivable {
                                bail!("{id} can't be waived: fix it, or defer it to a ticket");
                            }
                            (FindingState::Waived, Verdict::Right, String::new())
                        }
                        None => (FindingState::Folded, Verdict::Right, text.clone()),
                    },
                    p if chose(p, DEFER) => (FindingState::Deferred, Verdict::Right, String::new()),
                    p if chose(p, WAIVE) => (FindingState::Waived, Verdict::Right, String::new()),
                    p if chose(p, NOT_BLOCKER) => {
                        (FindingState::Logged, Verdict::NotABlocker, String::new())
                    }
                    p if chose(p, NOT_DEFECT) => {
                        (FindingState::Dropped, Verdict::NotADefect, String::new())
                    }
                    _ => (FindingState::Folded, Verdict::Right, String::new()),
                };
                let said = match (state, note.is_empty()) {
                    (FindingState::Deferred, true) => Some("Deferred to a ticket.".to_string()),
                    (FindingState::Deferred, false) => {
                        Some(format!("Deferred to a ticket: {note}"))
                    }
                    (_, false) => Some(format!("To the fixer: {note}")),
                    (_, true) => None,
                };
                applied.feedback.extend(verdict(run, id, v, said));
                // A defect several lenses found is decided once, for all.
                let same: Vec<String> = std::iter::once(id.clone())
                    .chain(run.lenses.values().flat_map(|l| {
                        l.rounds.last().into_iter().flat_map(|r| {
                            r.findings
                                .iter()
                                .filter(|g| g.same_as.as_deref() == Some(id))
                                .map(|g| g.id.clone())
                        })
                    }))
                    .collect();
                for sid in same {
                    if let Some(f) = finding_mut(run, &sid) {
                        f.state = state;
                        if !note.is_empty() {
                            f.note = note.clone();
                        }
                        match state {
                            FindingState::Logged => {
                                f.reason.push_str(" A person ruled it shouldn't block.")
                            }
                            FindingState::Dropped => {
                                f.reason.push_str(" A person ruled it isn't a defect.")
                            }
                            FindingState::Deferred => {
                                f.reason.push_str(" A person deferred it to a ticket.")
                            }
                            _ => {}
                        }
                    }
                }
                fold |= state == FindingState::Folded;
            }
            Topic::ShouldBlock(offered) => {
                if let Some(Picked::Chose(labels)) = pick {
                    for (lens, id) in offered.iter().filter(|(_, id)| labels.contains(id)) {
                        // A defect several lenses found blocks in each.
                        let same = std::iter::once((lens.clone(), id.clone()))
                            .chain(same_as(run, id))
                            .collect::<Vec<_>>();
                        for (lens, id) in same {
                            to_blocker(run, cfg, &lens, &id);
                            if let Some(f) = finding_mut(run, &id) {
                                f.state = FindingState::Folded;
                            }
                            applied
                                .feedback
                                .extend(verdict(run, &id, Verdict::ShouldBlock, None));
                            blocked += 1;
                        }
                        fold = true;
                    }
                }
            }
            Topic::Flags(offered) => {
                let chosen = match pick {
                    Some(Picked::Chose(labels)) => labels.clone(),
                    _ => Vec::new(),
                };
                for &i in offered {
                    let f = &mut run.flags[i];
                    f.state = if chosen.contains(&f.path) {
                        FlagState::Reverting
                    } else {
                        FlagState::Kept
                    };
                }
            }
            Topic::Missed => {
                if let Some(Picked::Other(text)) = pick
                    && !text.trim().is_empty()
                {
                    let id = format!("you-{}", run.reported.len() + 1);
                    run.reported.push(Reported {
                        id,
                        round: run.round,
                        text: text.trim().to_string(),
                        fixed: false,
                    });
                    applied.feedback.push(Feedback {
                        at,
                        run: run.id.clone(),
                        finding: None,
                        verdict: None,
                        missed: Some(text.trim().to_string()),
                        lens: None,
                        note: None,
                    });
                    fold = true;
                }
            }
            Topic::Final if chose(pick, APPROVE) => run.stage = Stage::Done,
            Topic::Final if chose(pick, ANOTHER) => {
                super::flow::reopen_all(run);
                run.round += 1;
                run.stage = Stage::Review;
            }
            _ => {}
        }
    }
    if stop {
        run.stage = Stage::Done;
        applied.stopped = true;
        return Ok(applied);
    }
    // Approving the result while saying findings should block: the choice to
    // block wins, so they're fixed rather than lost, and the review goes on.
    let approved = picks
        .iter()
        .any(|(t, _, p)| *t == Topic::Final && chose(p, APPROVE));
    if approved && blocked > 0 {
        applied.notes.push(format!(
            "You approved the result and said {blocked} finding(s) should block: they're \
             blockers now, so the review doesn't end yet. They're fixed, and every lens attacks \
             the fix."
        ));
    }
    // A lens with nothing left to fix or decide is settled the way its
    // blockers were.
    if run.stage == Stage::Decide {
        for lens in run.lenses.values_mut() {
            let Some(round) = lens.rounds.last() else {
                continue;
            };
            let left = round.findings.iter().any(|f| {
                f.blocks() && matches!(f.state, FindingState::Open | FindingState::Folded)
            });
            if lens.outcome == Outcome::Open && !left && round.failures.is_empty() {
                lens.outcome = Outcome::Waived;
            }
        }
    }
    let undecided = run.lenses.values().any(|l| {
        l.rounds
            .last()
            .is_some_and(|r| r.findings.iter().any(open_blocker))
    });
    if fold {
        run.stage = Stage::Fix;
    } else if run.stage == Stage::Decide && !undecided {
        run.stage = Stage::Done;
    }
    Ok(applied)
}

/// A finding a person says blocks: it's raised to the least that blocks in
/// its lens, and its lens is open again.
fn to_blocker(run: &mut Run, cfg: &Config, lens: &str, id: &str) {
    let action = cfg
        .lenses
        .iter()
        .find(|l| l.id == lens)
        .map(|l| cfg.action(l, Severity::High))
        .filter(|a| matches!(a, Action::Blocks | Action::BlocksUnlessWaived))
        .unwrap_or(Action::Blocks);
    let deciding = run.stage == Stage::Decide;
    if let Some(f) = finding_mut(run, id) {
        if f.severity > Severity::High {
            f.severity = Severity::High;
        }
        f.action = action;
        f.state = if deciding {
            FindingState::Open
        } else {
            FindingState::Folded
        };
        f.reason.push_str(" A person ruled that it blocks.");
    }
    if let Some(l) = run.lenses.get_mut(lens) {
        l.outcome = Outcome::Open;
    }
}
