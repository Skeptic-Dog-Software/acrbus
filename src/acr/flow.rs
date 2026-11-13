//! The review loop. Code tracks the rounds, so a run is repeatable and can
//! stop and start again: a model is called only for what needs judgment.
//!
//! A lens is confirmed only by a round that really ran. A reviewer or Judge
//! that fails leaves its lens open and goes to a person, because a review
//! that didn't happen must never read as a clean one.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::proto::RunUsage;

use super::change::{self, Change};
use super::config::{
    Action, Binding, Config, GateMode, JudgeMode, Lens, Reattack, ReviewerInput, Role, Severity,
    Stage as Kind,
};
use super::plan::{self, Why, tokens_in};
use super::roles::{self, Raw};
use super::stages;

/// What a role's call needs.
pub struct Call<'a> {
    pub role: Role,
    pub binding_name: &'a str,
    pub binding: &'a Binding,
    pub prompt: &'a str,
    /// The change, as a file.
    pub artifact: Option<&'a Path>,
    /// It may change the working tree: only an Orchestrator fixing.
    pub writes: bool,
    /// It runs with no tools at all, to answer from what it's given: the
    /// Sizer's first look. Its bridge refuses it if its agent can't.
    pub no_tools: bool,
    /// The prompt tokens of one turn past which it hands off to a fresh
    /// session, when its role's calls relay and its agent can.
    pub relay_tokens: Option<u64>,
    /// The repository, which every role works in: reviewers read it, and an
    /// Orchestrator fixes it.
    pub repo: &'a Path,
    /// What the call is, for a person following the review: "correctness:
    /// reviewer 1", or "the Sizer".
    pub label: String,
}

pub struct Answer {
    pub text: String,
    pub run_id: String,
    /// What the call used, if its agent said.
    pub usage: Option<RunUsage>,
    /// How long it took, from sending it to its answer, when the caller
    /// timed it.
    pub secs: Option<u64>,
}

/// A bridge and model: the calls that can share a cached prefix.
pub type ToolGroup = (String, Option<String>);

/// A round's shared facts, as its callers give them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    /// The rendered facts, kept in the run's folder.
    pub facts: PathBuf,
    /// The run. Each call adds its bridge and model, for the cache's key:
    /// every round's calls to one model reach the same replica, so a round
    /// reads what the last one left in its cache.
    pub key: String,
    /// By bridge and model, the tools every reviewer's and Judge's call on
    /// it is given when their roles' tools are matched: their union, sorted.
    pub tools: BTreeMap<ToolGroup, Vec<String>>,
    /// The agents the round's reviewers and Judge are bound to, each of
    /// which must take the facts for any call to be given them.
    pub agents: Vec<String>,
}

/// What a round's facts were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactsRound {
    pub round: u32,
    /// A hash of the rendered facts.
    pub key: String,
    pub bytes: u64,
    pub files: u64,
    /// The calls were given them: every agent the reviewers and the Judge
    /// are bound to takes them.
    #[serde(default)]
    pub given: bool,
}

/// How a role reaches its model.
pub trait Caller: Sync {
    fn call(&self, call: &Call) -> Result<Answer>;

    /// Tells a person following the review what it's doing now, such as a
    /// round starting. Calls are reported by the caller itself.
    fn note(&self, _what: &str) {}

    /// Tells a person something that happened, without changing what it
    /// says it's doing: a lens with nothing for its Judge, say.
    fn say(&self, _what: &str) {}

    /// The round's shared facts, for every reviewer's and Judge's call from
    /// now on: given to the agent ahead of the call's own text. None stops.
    /// True if it gives them: only then do the calls leave out what the
    /// facts carry. A caller that can't give them to every agent the
    /// round's calls go to gives them to none.
    fn share(&self, _shared: Option<Shared>) -> bool {
        false
    }

    /// Whether the call labelled `label` has filled the prompt cache yet:
    /// its first answer has come, or its first tokens show. None if this
    /// caller can't tell.
    fn warmed(&self, _label: &str) -> Option<bool> {
        None
    }

    /// Whether `binding`'s bridge has a slot to spare for a call no review
    /// is waiting on, such as a keep-alive.
    fn has_room(&self, _binding: &Binding) -> bool {
        true
    }

    /// What the checkout is now, to tell whether a role that may only read
    /// changed it. None if this caller can't say.
    fn fingerprint(&self, _repo: &Path) -> Option<String> {
        None
    }

    /// The checkout as it is now, to compare later. None if this caller
    /// can't say, and every lens attacks again after a fix.
    fn snapshot(&self, _repo: &Path) -> Option<String> {
        None
    }

    /// The files that changed between two snapshots, and the diff.
    fn delta(&self, _repo: &Path, _from: &str, _to: &str) -> Option<(Vec<String>, String)> {
        None
    }

    /// Each file that changed between two snapshots, binaries marked. None
    /// if this caller can't say, and nothing a fix changed is flagged.
    fn changed(&self, _repo: &Path, _from: &str, _to: &str) -> Option<Vec<super::change::Changed>> {
        None
    }

    /// Puts `path` back as it was in snapshot `from`, or removes it if it
    /// wasn't there: only ever as a person chose.
    fn revert(&self, _repo: &Path, _from: &str, _path: &str, _existed: bool) -> Result<()> {
        bail!("this review can't revert a file itself")
    }

    /// The call labelled `label` is a restart after its model couldn't be
    /// reached: it goes with a new cache key, so it may land on another
    /// replica than the one that didn't answer.
    fn rekey(&self, _label: &str) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Running,
    /// Waiting for a person, for the gate in `waiting`.
    Waiting,
    /// Every lens confirmed or waived.
    Confirmed,
    /// It ended with lenses open.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Gate {
    Scope,
    Fold,
    Final,
    Escalation,
    /// For the session that ran the review to fold in what was chosen, in
    /// the working tree, and say so.
    Fix,
}

impl Gate {
    pub fn as_str(self) -> &'static str {
        match self {
            Gate::Scope => "scope",
            Gate::Fold => "fold",
            Gate::Final => "final",
            Gate::Escalation => "escalation",
            Gate::Fix => "fix",
        }
    }
}

/// Who folds in what a review upholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fixer {
    /// A fresh agent on the Orchestrator's binding, which must write.
    #[default]
    Orchestrator,
    /// The session, or person, that ran the review: it holds the change's
    /// context, as the builder does in the in-session method.
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Scope,
    Review,
    Decide,
    Fix,
    /// The session that ran the review is fixing: see `Gate::Fix`.
    Fixing,
    Final,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Open,
    Confirmed,
    Waived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingState {
    /// A blocker with nothing done about it yet.
    Open,
    /// To be fixed, or was.
    Folded,
    Waived,
    /// Real, and left for a follow-up: a person deferred it to a ticket,
    /// with their note, and the review drafts the ticket. It doesn't block
    /// the change, and isn't waived: it's owed.
    Deferred,
    /// Doesn't block: logged.
    Logged,
    /// Rejected: dropped, with its reason kept.
    Dropped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub severity: Severity,
    pub action: Action,
    pub reason: String,
    pub state: FindingState,
    /// What a person said of it, for whoever fixes it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Another lens's finding this one is the same defect as: it's decided
    /// and fixed with that one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_as: Option<String>,
    /// Its ruling was carried over from this round's, not judged again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<u32>,
    /// The binding whose Judge ruled it, when a first pass is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruled_by: Option<String>,
}

impl Finding {
    pub fn blocks(&self) -> bool {
        matches!(self.action, Action::Blocks | Action::BlocksUnlessWaived)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Round {
    pub round: u32,
    /// Each reviewer that ran, labeled as the AI agent it is.
    pub reviewers: Vec<String>,
    /// What went wrong, if the round didn't fully run.
    pub failures: Vec<String>,
    pub findings: Vec<Finding>,
    /// What a stage reports of its own work, such as how many mutants the
    /// tests caught.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LensResult {
    pub id: String,
    pub criteria: String,
    pub rounds: Vec<Round>,
    /// What the Orchestrator did about its blockers.
    pub done: Vec<String>,
    pub outcome: Outcome,
    /// For a lens a later round's version of the change brought in: when,
    /// and what of it did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub joined: String,
    /// Rounds it didn't attack because the blast-radius check found the fix
    /// before them couldn't touch its concern. See `savings.blast_radius`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
}

/// A round a lens didn't attack, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skipped {
    pub round: u32,
    pub reason: String,
}

impl LensResult {
    fn latest(&self) -> Option<&Round> {
        self.rounds.last()
    }

    /// The skip since its last attack, if the change as it is now hasn't
    /// been attacked by it: it hasn't passed this version, whatever its
    /// outcome says.
    pub fn skipped_since_attack(&self) -> Option<&Skipped> {
        let attacked = self.rounds.last().map_or(0, |r| r.round);
        self.skipped.last().filter(|s| s.round > attacked)
    }

    pub fn blockers(&self) -> Vec<&Finding> {
        let round = self.latest().map_or(&[][..], |r| &r.findings);
        round
            .iter()
            .filter(|f| f.blocks() && f.state == FindingState::Open)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Escalation {
    pub reason: String,
    /// A person has seen it.
    pub seen: bool,
    /// What it's about, for the question it becomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<About>,
}

/// What an escalation is about, so that a person can answer it in kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum About {
    /// A finding whose ruling is a person's call: a values call, a split,
    /// or a Critical that was there before.
    Finding { lens: String, finding: String },
    /// A lens that didn't finish its work this round.
    Unfinished { lens: String },
    /// Lenses still blocked at `max_rounds`.
    MaxRounds,
    /// The Orchestrator couldn't fix what was folded in.
    Unfixed,
}

/// A defect a person says the reviewers missed. It's folded in like a
/// blocker, and the rounds after attack the fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reported {
    pub id: String,
    pub round: u32,
    pub text: String,
    pub fixed: bool,
}

/// A session's fixes under way: the checkout from before them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fixing {
    pub from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disagreement {
    pub lens: String,
    pub finding: String,
    pub sides: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub role: Role,
    pub binding: String,
    pub agent: String,
    pub model: Option<String>,
    pub run_id: String,
    /// Input not read from or written to a cache.
    pub tokens_in: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    pub tokens_out: u64,
    /// By the config's prices, or the agent's if the config has none.
    pub usd: Option<f64>,
    /// The tokens are the agent's own count, not an estimate.
    #[serde(default)]
    pub measured: bool,
    /// What the agent said the call cost.
    #[serde(default)]
    pub agent_usd: Option<f64>,
    /// It came out of a subscription: tokens count, dollars don't.
    #[serde(default)]
    pub subscription: bool,
    /// The round it was spent in.
    #[serde(default)]
    pub round: u32,
    /// How long the call took, from sending it to its answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u64>,
    /// What the call was, as progress named it: "correctness: reviewer 1".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The fan-out it was part of, such as "reviewers" or "judges", for the
    /// cache's figures: the first call fills a prefix the rest should read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_out: Option<String>,
    /// It was its fan-out's first call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first: bool,
    /// It put another call's answer in the form asked: the Sizer's repair
    /// of an answer that couldn't be read as it came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repaired: Option<Repaired>,
    /// The lens, for a reviewer's call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens: Option<String>,
    /// The files its agent read, as its tool calls named them, when its
    /// bridge says: what the review learns its reviewers open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opened: Vec<String>,
    /// For a reviewer's session, the reviewer call it belongs to, by that
    /// call's first label: a relay's legs and a scout's verifying sessions
    /// are one call, whose reading counts once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    /// Its first model call alone, by the agent's count: what the call's
    /// first turn read from the cache, which shows a warm-up worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_turn: Option<crate::proto::TurnUsage>,
    /// It finished in a round that didn't: a run stopped mid-round, its
    /// usage read from the answer as it came, not from the round's end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unfinished: bool,
    /// Its part of a call broken into sessions: "leg 2" of a relay,
    /// "scout", or "verify 3".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    /// Tokens a turn was billed fresh though its session's turn before had
    /// just sent them, from its second turn on: what the provider should
    /// have served from its cache. Its first turn's share is the report's.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_shortfall: u64,
    /// What its agent says the fresh input and the cached input cost, when
    /// it splits its cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_cache_read: Option<f64>,
    /// The config's price, as dollars a token for fresh input and for input
    /// read from a cache, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_rates: Option<[f64; 2]>,
    /// Its model calls, and the tool calls they made.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub turns: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tool_calls: u64,
    /// A session restarted fresh after the unit's first came back unusable,
    /// and why: counted apart from the work it redid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    /// A fix group's route under `savings.fixer`: "easy" on the fixer's
    /// binding, "hard" on the Orchestrator's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// The tools its model asked for that its binding doesn't name, and how
    /// often, whether or not they ran: one a gateway added to the request,
    /// or one the model made up. Said as it answers, and in the report.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unnamed_tools: BTreeMap<String, u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Whose answer a repair put in form: its role, binding and model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Repaired {
    pub role: Role,
    pub binding: String,
    pub model: Option<String>,
}

impl Repaired {
    fn of(call: &Call) -> Repaired {
        Repaired {
            role: call.role,
            binding: call.binding_name.to_string(),
            model: call.binding.model.clone(),
        }
    }
}

/// The share of a binding's calls whose answers needed a repair, above
/// which progress says so, once there are at least `REPAIR_WARN_MIN`.
pub const REPAIR_WARN_SHARE: f64 = 0.2;
pub const REPAIR_WARN_MIN: usize = 2;

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.tokens_in + self.cache_read + self.cache_write + self.tokens_out
    }
}

/// The dollars of the calls billed in them. None if one of those has no
/// price, or none was billed in dollars at all.
pub fn dollars(usage: &[Usage]) -> Option<f64> {
    let mut billed = usage.iter().filter(|u| !u.subscription).peekable();
    billed.peek()?;
    billed.map(|u| u.usd).sum()
}

/// The label of the Sizer's call that finds the defects more than one lens
/// found, which a usage summary counts apart from its sizing.
pub const DEDUPE: &str = "the Sizer, finding defects more than one lens found";

/// A review, as far as it's got. It's all saved, so that it can stop for a
/// person and carry on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    /// The bus name of the session that started the review, when it was
    /// one: who its fixes are for, if answers come from elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter: Option<String>,
    pub repo: PathBuf,
    pub base: String,
    pub head: Option<String>,
    pub tier: String,
    pub tier_why: String,
    pub fan_out: u32,
    pub round: u32,
    pub stage: Stage,
    pub status: Status,
    pub waiting: Option<Gate>,
    pub lenses: BTreeMap<String, LensResult>,
    pub escalations: Vec<Escalation>,
    pub disagreements: Vec<Disagreement>,
    pub usage: Vec<Usage>,
    /// What the change says it's for, such as its pull request's
    /// description, kept so a resumed review sends the same.
    #[serde(default)]
    pub description: Option<String>,
    /// What the last steelman's fixes changed, in a review to convergence:
    /// the next round attacks it first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixDelta>,
    /// This round is the full sweep: every lens that hasn't attacked the
    /// change as it is now does, before the review can converge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sweep: bool,
    /// Reviewed to convergence: see `Setting::converge`.
    #[serde(default)]
    pub converge: bool,
    /// What each steelman between rounds did, in order.
    #[serde(default)]
    pub steelmans: Vec<Steelman>,
    /// The briefs the session that fixed gave for the round after its fix,
    /// outside a review to convergence, where a steelman's carry them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_briefs: Option<FixBriefs>,
    /// Which lenses attack again after a fix, as the config, a flag or a
    /// person at the scope gate chose. None: nobody chose, so `blocked`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reattack: Option<Reattack>,
    /// The builder's brief the run started with, kept so a resumed run
    /// briefs its reviewers as one that never stopped does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder_brief: Option<stages::BuilderBrief>,
    /// What the run's flags made weaker than the repository's config, each
    /// said at its start, at every gate and in the report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weakened: Vec<String>,
    /// Where each gate's answers came from, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answered_by: Vec<AnswerSource>,
    /// Defects a person reported the reviewers missed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported: Vec<Reported>,
    /// Who folds in what's upheld.
    #[serde(default)]
    pub fixer: Fixer,
    /// The tier was raised at the scope gate, so the lenses are chosen
    /// again before it's asked once more.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rescope: bool,
    /// The session's fixes, while they're under way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixing: Option<Fixing>,
    /// Each round's shared facts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<FactsRound>,
    /// What a fan-out's calls, and each round with the last, share at the
    /// head of what they send.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefixes: Vec<super::facts::Prefix>,
    /// The rounds whose reviewers' reading is already in the repository's
    /// record of what its reviewers open, so a resumed round isn't counted
    /// twice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub learned: Vec<u32>,
    /// A copy of the commit under review, which the review reads until its
    /// first fix, so the checkout can be on any branch meanwhile. None for a
    /// review of the working tree, and for one saved before reviews read a
    /// copy: those read the checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_copy: Option<PathBuf>,
    /// The change's pull request, whose words each round's facts carry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// Rulings that didn't block, kept for a later round's same claim. See
    /// `savings.reuse_rulings`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rulings: Vec<PastRuling>,
    /// A fix in groups that stopped part way: the groups it did, so a fix
    /// tried again carries on from the first it didn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_fix: Option<PartialFix>,
    /// Files fixes changed that a person is asked about: binaries, and
    /// files no blocker the fix was given points at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<Flag>,
    /// A round paused because its calls couldn't reach their model: `acr
    /// resume` continues it, keeping what its units finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outage: Option<Paused>,
    /// Where each fix group went under `savings.fixer`, to tell whether its
    /// blockers held.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<FixRoute>,
}

/// A round paused by an outage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paused {
    pub round: u32,
    pub why: String,
}

/// A fix group's route under `savings.fixer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixRoute {
    pub round: u32,
    /// Its blockers, by finding id.
    pub ids: Vec<String>,
    pub binding: String,
    pub easy: bool,
    pub why: String,
}

/// A file a fix changed that a person keeps or reverts: a binary, or one
/// no blocker the fix was given points at. It's flagged and asked about,
/// and nothing reverts it but their choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flag {
    /// The round whose fix changed it.
    pub round: u32,
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
    /// No blocker the fix was given points at it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    /// The checkout before the fix, as a git tree, to revert it from.
    pub from: String,
    /// It was there before the fix: reverting it restores it, rather than
    /// removing it.
    pub existed: bool,
    /// What reverts it, for a person to run.
    pub command: String,
    #[serde(default)]
    pub state: FlagState,
}

impl Flag {
    /// Why it's flagged, in a few words.
    pub fn why(&self) -> &'static str {
        match (self.binary, self.outside) {
            (true, true) => "a binary, and no blocker points at it",
            (true, false) => "a binary",
            _ => "no blocker points at it",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FlagState {
    /// Not asked about yet, or a revert that failed.
    #[default]
    Open,
    Kept,
    /// A person chose to revert it, and the review's next step does.
    Reverting,
    Reverted,
}

/// A fix in groups that stopped at a group: the round's, with the checkout
/// from before its first group and each group done since.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialFix {
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default)]
    pub groups: Vec<GroupDone>,
}

/// One group of a fix, done: its blockers' IDs, and what its fixer said it
/// changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDone {
    pub ids: Vec<String>,
    pub done: String,
}

/// A Judge's ruling that didn't block, kept so a later round's finding that
/// makes the same claim, in the same lens, at the same place in a file no
/// fix has changed, takes it instead of a Judge's call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastRuling {
    pub lens: String,
    pub round: u32,
    pub file: String,
    pub line: u64,
    /// The claim, as its titles normalized: the Judge's own, and those of
    /// the reviewers' findings it rated at that place.
    pub claims: Vec<String>,
    /// The file's content when it was ruled on, by hash.
    pub file_hash: String,
    /// What else the ruling rested on, by hash: the lens's text, the decided
    /// non-issues, the Judge's bindings and the rules.
    pub context: String,
    pub rating: roles::Rating,
}

/// How far, in lines, a later finding may sit from a kept ruling's and
/// still be the same place, in a file nothing has changed.
const SAME_PLACE: u64 = 3;

/// A claim's title, for matching: its letters and digits, lowercased and
/// single-spaced.
pub(super) fn claim_key(title: &str) -> String {
    title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A file's content, by hash, in the tree a round reads. None when it can't
/// be read, which never matches.
fn file_hash(tree: &Path, file: &str) -> Option<String> {
    let bytes = fs::read(tree.join(file)).ok()?;
    Some(super::calibration::hash(&[&String::from_utf8_lossy(
        &bytes,
    )]))
}

/// What a lens's rulings rest on besides the finding and its file: a
/// change to any of it means judging again.
fn ruling_context(cfg: &Config, lens: &Lens, not_findings: &str, rules: &str) -> String {
    let judge = cfg.binding_name(Role::Judge, None).unwrap_or_default();
    let model = cfg
        .binding(Role::Judge, None)
        .ok()
        .and_then(|b| b.model.clone())
        .unwrap_or_default();
    let first = cfg.savings.first_pass_judge.as_deref().unwrap_or_default();
    super::calibration::hash(&[
        &lens.id,
        &lens.criteria,
        lens.blocker.as_deref().unwrap_or_default(),
        lens.attack.as_deref().unwrap_or_default(),
        &format!("{:?}", lens.severity),
        not_findings,
        judge,
        &model,
        first,
        rules,
    ])
}

/// The kept ruling a reviewer's finding can take, if any: the same lens,
/// the same claim, at the same place, in a file no fix has changed, with
/// nothing else it rested on changed, and still not blocking under the
/// rules as they apply now. Anything short of that goes to a Judge.
#[allow(clippy::too_many_arguments)]
fn reusable<'a>(
    past: &'a [PastRuling],
    cfg: &Config,
    lens: &Lens,
    context: &str,
    tree: &Path,
    raw: &Raw,
    changed: &stages::ChangedLines,
) -> Option<(usize, &'a PastRuling)> {
    let (file, line) = (raw.file.as_deref()?, raw.line?);
    let claim = claim_key(&raw.title);
    if claim.is_empty() {
        return None;
    }
    let (i, p) = past.iter().enumerate().find(|(_, p)| {
        p.lens == lens.id
            && p.file == file
            && p.line.abs_diff(line) <= SAME_PLACE
            && p.context == context
            && p.claims.contains(&claim)
    })?;
    if file_hash(tree, file).as_deref() != Some(p.file_hash.as_str()) {
        return None;
    }
    let now = ruling(
        cfg,
        lens,
        &grounded(p.rating.clone(), changed, lens.blocker.as_deref()),
    );
    let blocks = matches!(now.action, Action::Blocks | Action::BlocksUnlessWaived);
    (!blocks && !now.escalate).then_some((i, p))
}

/// Whether a first-pass Judge's rating goes on to the Judge's binding:
/// anything that could block, rates Medium or above, is split or a values
/// call, states a fact the rules raise, or that it isn't sure of. What's
/// left, it settles: Low findings and rejections it's sure of.
pub(super) fn first_pass_escalates(
    cfg: &Config,
    lens: &Lens,
    r: &roles::Rating,
    changed: &stages::ChangedLines,
) -> bool {
    let ruled = ruling(
        cfg,
        lens,
        &grounded(r.clone(), changed, lens.blocker.as_deref()),
    );
    // The facts as the first pass states them, before the diff settles
    // them: a claim it can't quote still goes on, for the Judge to settle.
    matches!(ruled.action, Action::Blocks | Action::BlocksUnlessWaived)
        || ruled.escalate
        || r.unsure
        || r.split
        || r.values_call
        || matches!(
            r.severity,
            Severity::Critical | Severity::High | Severity::Medium
        )
        || r.weak_pin
        || r.misleads
        || r.meets_blocker
        || r.claims_fix
        || r.worsens
}

impl Run {
    /// Which lenses attack again after a fix: as chosen, or `blocked`.
    pub fn reattacks(&self) -> Reattack {
        self.reattack.unwrap_or(Reattack::Blocked)
    }

    /// What the report says of it: the choice, and whose it was.
    pub fn reattack_line(&self) -> String {
        match self.reattack {
            Some(Reattack::All) => "re-attack: every lens after each fix".into(),
            Some(Reattack::Blocked) => {
                "re-attack: only the lenses with blockers after each fix".into()
            }
            None => "re-attack: only the lenses with blockers after each fix, the default: \
                     nobody chose"
                .into(),
        }
    }

    /// Whether a fix has gone in. From then on the review is of the
    /// checkout's working tree, where the fixes are.
    pub fn fixed(&self) -> bool {
        self.lenses.values().any(|l| !l.done.is_empty())
    }

    /// Where the review reads the change: the copy of its commit until the
    /// first fix, and the checkout from then on, or throughout when there's
    /// no copy.
    pub fn reading(&self) -> &Path {
        match &self.head_copy {
            Some(copy) if !self.fixed() => copy,
            _ => &self.repo,
        }
    }
}

/// A synthesis between rounds of a review to convergence: what the
/// Orchestrator was given to fold in, what it did, and what it declined and
/// why. A declined finding is a disagreement, not a resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steelman {
    /// The round whose findings it folded in.
    pub round: u32,
    pub blockers: Vec<String>,
    pub done: String,
    /// What the test writer that followed it wrote, and how each test
    /// fared on the code from before the fixes and after.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<Pin>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tests_done: String,
    /// Its briefs for the next round's reviewers, by lens.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub briefs: BTreeMap<String, String>,
    pub declined: Vec<roles::Declined>,
}

/// Where a gate's answers came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerSource {
    pub round: u32,
    pub gate: String,
    /// A terminal's prompt, a Grayboard reply and its sender, or
    /// `acr resume` and its flags.
    pub by: String,
}

impl Run {
    /// Records that the gate it waits at was answered, and by what.
    pub fn answered(&mut self, by: impl Into<String>) {
        let gate = self.waiting.map_or("none", Gate::as_str).to_string();
        self.answered_by.push(AnswerSource {
            round: self.round,
            gate,
            by: by.into(),
        });
    }
}

/// A fixing session's briefs for the round after its fix, by lens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixBriefs {
    /// The round they brief.
    pub round: u32,
    pub briefs: BTreeMap<String, String>,
}

/// What a steelman's fixes changed: the round whose blockers they fixed,
/// and the files. The diff is kept beside the round's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixDelta {
    pub round: u32,
    pub files: Vec<String>,
}

/// A test file the writer wrote between rounds/// A test file the writer wrote between rounds, and how it fared: a pin
/// fails on the code from before the fixes and passes after. None where it
/// couldn't be run, or ran past the limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub file: String,
    /// The findings it pins.
    #[serde(default)]
    pub pins: Vec<String>,
    pub fails_before: Option<bool>,
    pub passes_after: Option<bool>,
}

/// What stays the same through a review./// What stays the same through a review.
pub struct Setting<'a> {
    pub cfg: &'a Config,
    pub not_findings: String,
    /// Where a round's diff is written.
    pub work_dir: PathBuf,
    /// What the change says it's for, such as its pull request's
    /// description. Every role gets it, with the commit messages, ahead of
    /// the diff.
    pub description: Option<String>,
    /// Review to convergence, as the essay ACR comes from prescribes: build,
    /// attack, steelman, attack again, until the attackers agree. After each
    /// round with a blocker, the Orchestrator folds in the blockers the Judge
    /// upheld, and then every lens is attacked again by fresh reviewers,
    /// since a fix for one can break another. It ends when one round, on one
    /// version of the change, has no blocker in any lens.
    pub converge: bool,
    /// End the run at the first failure: a reviewer, Judge or stage that
    /// didn't finish its work, or a brief that couldn't be written. Without
    /// it, a failure is recorded and the run goes on without that work.
    pub stop_on_failure: bool,
    /// Seconds a unit whose call couldn't reach its model waits to see
    /// whether the round's other calls fail the same way, an outage, before
    /// it restarts.
    pub outage_wait_secs: u64,
    /// Where a reviewer whose binding can change files gets a copy of the
    /// checkout of its own, for the round. Without it, every reviewer works
    /// in the checkout.
    pub copies: Option<PathBuf>,
    /// A copy of the commit under review, made for a new run with `--head`:
    /// see `Run::head_copy`.
    pub head_copy: Option<PathBuf>,
    /// The change's pull request, for a new run: see `Run::pr`.
    pub pr: Option<u64>,
    /// Where each repository's record of what its reviewers open is kept
    /// (see `learned`). None for a trial, whose arms are measured blind, and
    /// for tests.
    pub learned: Option<PathBuf>,
    /// The building session's brief for round 1's reviewers, if it wrote
    /// one. Later rounds are briefed by the steelman, which holds the
    /// change's context by then.
    pub builder_brief: Option<stages::BuilderBrief>,
}

/// A person's answer at a gate.
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub fold: Vec<String>,
    pub waive: Vec<String>,
    /// Blockers deferred to a ticket: any blocker, since a person decides.
    pub defer: Vec<String>,
    pub approve: bool,
    pub stop: bool,
}

/// What a call cost: by the agent's count if it gave one, or else by the
/// text it took and gave and by the config. Either is priced by the config,
/// falling back to what the agent said.
/// A call answered, as its caller kept it the moment its answer came, in
/// the run's folder: what a run stopped mid-round still counts, since a
/// round's calls are recorded only when the round ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Answered {
    /// The bridge's request.
    pub id: String,
    pub role: Role,
    pub binding: String,
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub subscription: bool,
    pub label: String,
    #[serde(default)]
    pub secs: Option<u64>,
    #[serde(default)]
    pub usage: Option<RunUsage>,
}

/// The file in a run's folder its answered calls are kept in, a line each.
pub const ANSWERED: &str = "answered";

impl Answered {
    /// Every answered call kept in run folder `dir`, oldest first: none if
    /// it has none, and a line that can't be read is passed over.
    pub fn read(dir: &Path) -> Vec<Answered> {
        fs::read_to_string(dir.join(ANSWERED))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Its usage as the run would have recorded it in `round`, priced by
    /// what its agent said it cost, and marked as from a round that didn't
    /// finish.
    pub fn usage(&self, round: u32) -> Usage {
        let counted = self.usage.clone().unwrap_or_default();
        Usage {
            role: self.role,
            binding: self.binding.clone(),
            agent: self.agent.clone(),
            model: self.model.clone(),
            run_id: self.id.clone(),
            tokens_in: counted.input_tokens,
            cache_read: counted.cache_read_tokens,
            cache_write: counted.cache_write_tokens,
            tokens_out: counted.output_tokens,
            usd: if self.subscription { None } else { counted.usd },
            measured: self.usage.is_some(),
            agent_usd: counted.usd,
            subscription: self.subscription,
            round,
            secs: self.secs,
            label: Some(self.label.clone()),
            fan_out: None,
            first: false,
            repaired: None,
            lens: None,
            opened: counted.opened.clone(),
            call: None,
            first_turn: counted.first_turn,
            unfinished: true,
            part: None,
            cache_shortfall: counted.cache_shortfall_tokens,
            usd_input: counted.usd_input,
            usd_cache_read: counted.usd_cache_read,
            price_rates: None,
            turns: counted.calls,
            tool_calls: counted.tool_calls,
            restart: None,
            route: None,
            unnamed_tools: BTreeMap::new(),
        }
    }
}

/// The tools in `used` that `binding` doesn't name, with how often each was
/// asked for.
pub fn unnamed_tools(binding: &Binding, used: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    binding
        .unnamed(used.keys())
        .into_iter()
        .map(|name| {
            let n = used.get(&name).copied().unwrap_or_default();
            (name, n)
        })
        .collect()
}

fn usage(cfg: &Config, call: &Call, input_bytes: usize, answer: &Answer) -> Usage {
    let mut usage = Usage {
        role: call.role,
        binding: call.binding_name.to_string(),
        agent: call.binding.agent.clone(),
        model: call.binding.model.clone(),
        run_id: answer.run_id.clone(),
        tokens_in: 0,
        cache_read: 0,
        cache_write: 0,
        tokens_out: 0,
        usd: None,
        measured: false,
        agent_usd: None,
        subscription: call.binding.subscription,
        round: 0,
        secs: answer.secs,
        label: Some(call.label.clone()),
        fan_out: None,
        first: false,
        repaired: None,
        lens: None,
        opened: answer
            .usage
            .as_ref()
            .map(|u| u.opened.clone())
            .unwrap_or_default(),
        call: None,
        first_turn: None,
        unfinished: false,
        part: None,
        cache_shortfall: 0,
        usd_input: None,
        usd_cache_read: None,
        price_rates: None,
        turns: 0,
        tool_calls: 0,
        restart: None,
        route: None,
        unnamed_tools: answer
            .usage
            .as_ref()
            .map(|u| unnamed_tools(call.binding, &u.tools))
            .unwrap_or_default(),
    };
    match &answer.usage {
        Some(counted) => {
            usage.first_turn = counted.first_turn;
            usage.cache_shortfall = counted.cache_shortfall_tokens;
            usage.usd_input = counted.usd_input;
            usage.usd_cache_read = counted.usd_cache_read;
            usage.turns = counted.calls;
            usage.tool_calls = counted.tool_calls;
            usage.price_rates = cfg.price(call.binding_name, call.binding).map(|p| {
                [
                    p.input_per_mtok / 1e6,
                    p.cached_input_per_mtok.unwrap_or(p.input_per_mtok) / 1e6,
                ]
            });
            usage.tokens_in = counted.input_tokens;
            usage.cache_read = counted.cache_read_tokens;
            usage.cache_write = counted.cache_write_tokens;
            usage.tokens_out = counted.output_tokens;
            usage.measured = true;
            if usage.subscription {
                return usage;
            }
            usage.agent_usd = counted.usd;
            usage.usd = cfg
                .price(call.binding_name, call.binding)
                .map(|p| {
                    p.usd(
                        usage.tokens_in,
                        usage.cache_read,
                        usage.cache_write,
                        usage.tokens_out,
                    )
                })
                .or(counted.usd);
        }
        None => {
            let guess = plan::cost(
                cfg,
                call.role,
                call.binding_name,
                Some(call.binding),
                tokens_in(input_bytes),
                tokens_in(answer.text.len()),
            );
            usage.tokens_in = guess.tokens_in;
            usage.cache_read = guess.cache_read;
            usage.tokens_out = guess.tokens_out;
            usage.usd = guess.usd;
        }
    }
    usage
}

fn record(run: &mut Run, cfg: &Config, call: &Call, input_bytes: usize, answer: &Answer) {
    let mut usage = usage(cfg, call, input_bytes, answer);
    usage.round = run.round;
    run.usage.push(usage);
}

fn escalate(run: &mut Run, reason: String) {
    escalate_about(run, reason, None);
}

/// An escalation a person can answer in kind, as `about` says.
fn escalate_about(run: &mut Run, reason: String, about: Option<About>) {
    if !run.escalations.iter().any(|e| e.reason == reason) {
        run.escalations.push(Escalation {
            reason,
            seen: false,
            about,
        });
    }
}

fn input_bytes(prompt: &str, artifact: Option<&Path>) -> usize {
    prompt.len()
        + artifact
            .and_then(|p| fs::metadata(p).ok())
            .map_or(0, |m| m.len() as usize)
}

/// One call's sessions. With `relay_tokens` set, a session past that size
/// hands off, and a fresh one on the same binding carries on from its
/// handoff, from the shared opening, until one answers. Without it, one
/// session, as always.
pub struct Legs {
    /// Each session's usage, marked "leg N" when there was more than one.
    pub usage: Vec<Usage>,
    /// The handoffs' answers, holding what each session found, to merge.
    pub handed: Vec<String>,
    /// The last session's answer.
    pub last: String,
    /// The last session's label.
    pub label: String,
    /// The tool calls its sessions made, when every one's bridge counts
    /// them: 0 is a call that never read the code.
    pub tools: Option<u64>,
}

/// A step observed doing no work: said, and an error, so it fails as any
/// step does, to be tried again or escalated, and never passes.
pub(super) fn no_work(caller: &dyn Caller, label: &str, what: &str) -> anyhow::Error {
    let line = format!("{label}: did no work: {what}");
    caller.say(&line);
    anyhow::Error::new(NoWork(line))
}

/// Why a call to an agent failed, as its caller could tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Its agent's run failed.
    Failed,
    /// Its model requests kept getting no response, so its run ended: the
    /// provider, or a gateway on the way, may be down.
    NoResponse,
    /// It couldn't reach its agent, or its agent couldn't reach its model.
    Unreachable,
    /// It ran as long as its bridge allows, or went quiet on every try.
    TimedOut,
    /// Its bridge refused what it asked for.
    Refused,
    /// It was cancelled.
    Cancelled,
}

impl Failure {
    /// Its model, or the way to it, is down, as in an outage.
    pub fn connection(self) -> bool {
        matches!(self, Failure::NoResponse | Failure::Unreachable)
    }

    /// One a fresh session could do better at: not a refusal or a cancel,
    /// which one would only repeat.
    fn restarts(self) -> bool {
        !matches!(self, Failure::Refused | Failure::Cancelled)
    }
}

/// A call that failed, and how.
#[derive(Debug)]
pub struct CallFailed {
    pub failure: Failure,
    pub message: String,
}

impl std::fmt::Display for CallFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CallFailed {}

/// A step observed doing no work.
#[derive(Debug)]
struct NoWork(String);

impl std::fmt::Display for NoWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoWork {}

/// A unit of work left unfinished because the round's calls can't reach
/// their model: it runs again when the review is continued.
#[derive(Debug)]
pub struct Outage(pub String);

impl std::fmt::Display for Outage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the model is unreachable: {}", self.0)
    }
}

impl std::error::Error for Outage {}

fn find<T: std::error::Error + 'static>(e: &anyhow::Error) -> Option<&T> {
    e.chain().find_map(|c| c.downcast_ref::<T>())
}

/// A round paused by an outage, for its caller to save and say how to
/// continue.
pub fn is_outage(e: &anyhow::Error) -> bool {
    find::<Outage>(e).is_some()
}

fn is_connection(e: &anyhow::Error) -> bool {
    find::<CallFailed>(e).is_some_and(|f| f.failure.connection())
}

/// An answer that can't be read, as an error that says so.
fn unreadable(e: anyhow::Error) -> anyhow::Error {
    if find::<Unreadable>(&e).is_some() {
        return e;
    }
    anyhow::Error::new(Unreadable(format!("{e:#}")))
}

/// Why a unit of work's failure is one a fresh session could do better at:
/// its answer couldn't be read, it did no work, or its run failed. None for
/// a stop, a cancel, a refusal, an outage, or anything in the config, which
/// a fresh session would only repeat. True with it when its model couldn't
/// be reached.
fn restartable(e: &anyhow::Error) -> Option<bool> {
    if is_outage(e) {
        return None;
    }
    if let Some(f) = find::<CallFailed>(e) {
        return f.failure.restarts().then_some(f.failure.connection());
    }
    let unusable = find::<Unreadable>(e).is_some()
        || find::<NoWork>(e).is_some()
        || format!("{e:#}").contains("did no work: ");
    unusable.then_some(false)
}

/// The label of a unit's session restarted fresh.
fn restarted_label(label: &str) -> String {
    format!("{label}, restarted")
}

/// Restarts a unit of work once, in a fresh session, when it comes back
/// unusable: on the same binding, with the same inputs, and nothing of the
/// failed session's context. One whose model couldn't be reached gets a new
/// cache key, so it may land on another replica. A restart that fails too
/// is a failure, naming both reasons, to be asked about: never a pass. In an
/// outage, a unit isn't restarted: it's left for the review's continuing.
pub(super) struct Restarter<'a> {
    caller: &'a dyn Caller,
    watch: Option<&'a OutageWatch>,
    /// Each restarted session's label, and why.
    done: std::sync::Mutex<Vec<(String, String)>>,
}

impl<'a> Restarter<'a> {
    pub(super) fn new(caller: &'a dyn Caller, watch: Option<&'a OutageWatch>) -> Restarter<'a> {
        Restarter {
            caller,
            watch,
            done: Default::default(),
        }
    }

    /// An error that ends a unit in an outage, if this is one.
    fn outage(&self, e: &anyhow::Error) -> Option<anyhow::Error> {
        if is_outage(e) {
            return Some(anyhow::Error::new(Outage(format!("{e:#}"))));
        }
        let why = self.watch.and_then(OutageWatch::declared)?;
        is_connection(e).then(|| anyhow::Error::new(Outage(why)))
    }

    /// `first`, or, if it came back unusable, `again` in a fresh session,
    /// labeled by `restarted_label`.
    pub(super) fn once<T>(
        &self,
        unit: &str,
        first: Result<T>,
        again: impl FnOnce(&str) -> Result<T>,
    ) -> Result<T> {
        let e = match first {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        if let Some(out) = self.outage(&e) {
            return Err(out);
        }
        let Some(connection) = restartable(&e) else {
            return Err(e);
        };
        // A model that couldn't be reached may be down for every call: an
        // outage pauses the round rather than restarting each unit.
        if connection
            && let Some(watch) = self.watch
            && watch.settle()
        {
            return Err(self.outage(&e).unwrap_or(e));
        }
        let why = format!("{e:#}");
        let label = restarted_label(unit);
        if connection {
            self.caller.rekey(&label);
        }
        self.caller.say(&format!(
            "restarted fresh: {unit}, attempt 2, because {why}"
        ));
        lock_vec(&self.done).push((label.clone(), why.clone()));
        match again(&label) {
            Ok(v) => Ok(v),
            Err(e2) => match self.outage(&e2) {
                Some(out) => Err(out),
                None => Err(anyhow!(
                    "{unit} came back unusable twice: first {why}; then, restarted fresh, {e2:#}"
                )),
            },
        }
    }

    /// Marks each call a restarted session made, and its relays' and
    /// checks' sessions, with why it was restarted.
    pub(super) fn mark(&self, usage: &mut [Usage]) {
        let done = lock_vec(&self.done);
        for u in usage.iter_mut() {
            let Some(label) = u.label.as_deref() else {
                continue;
            };
            if let Some((_, why)) = done.iter().find(|(l, _)| {
                label == l
                    || label.starts_with(&format!("{l} ("))
                    || label.starts_with(&format!("{l}: "))
            }) {
                u.restart = Some(why.clone());
            }
        }
    }
}

/// What a round's calls say of an outage: when calls that couldn't reach
/// their model are at least two, and at least half of those in flight,
/// with none answered in between, within `OUTAGE_WINDOW`, the round pauses.
pub(super) struct OutageWatch {
    wait: std::time::Duration,
    state: std::sync::Mutex<WatchState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct WatchState {
    in_flight: usize,
    /// The most in flight since a call last answered.
    peak: usize,
    /// When calls couldn't reach their model, since a call last answered.
    failed: Vec<std::time::Instant>,
    /// Bumped each time a call answers.
    answered: u64,
    declared: Option<String>,
}

/// How close together failures to reach the model count as one outage.
const OUTAGE_WINDOW: std::time::Duration = std::time::Duration::from_secs(600);

impl OutageWatch {
    pub(super) fn new(wait_secs: u64) -> OutageWatch {
        OutageWatch {
            wait: std::time::Duration::from_secs(wait_secs),
            state: Default::default(),
            changed: Default::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Why the round's calls can't reach their model, once that's clear.
    pub(super) fn declared(&self) -> Option<String> {
        self.lock().declared.clone()
    }

    fn started(&self) {
        let mut st = self.lock();
        st.in_flight += 1;
        st.peak = st.peak.max(st.in_flight);
    }

    fn answered(&self) {
        let mut st = self.lock();
        st.in_flight = st.in_flight.saturating_sub(1);
        st.failed.clear();
        st.answered += 1;
        st.peak = st.in_flight;
        self.changed.notify_all();
    }

    fn failed(&self, e: &anyhow::Error) {
        let mut st = self.lock();
        st.in_flight = st.in_flight.saturating_sub(1);
        if is_connection(e) {
            let now = std::time::Instant::now();
            st.failed.retain(|t| now.duration_since(*t) < OUTAGE_WINDOW);
            st.failed.push(now);
            let n = st.failed.len();
            if st.declared.is_none() && n >= 2 && n * 2 >= st.peak {
                st.declared = Some(format!(
                    "{n} of the {} calls in flight couldn't reach their model, and none answered \
                     in between; the last: {e:#}",
                    st.peak
                ));
            }
        }
        self.changed.notify_all();
    }

    /// After a call that couldn't reach its model: waits, up to the
    /// outage wait, for the round's other calls in flight to tell an outage
    /// from one call's failure. True if it's an outage.
    fn settle(&self) -> bool {
        let deadline = std::time::Instant::now() + self.wait;
        let mut st = self.lock();
        let seen = st.answered;
        loop {
            if st.declared.is_some() {
                return true;
            }
            let now = std::time::Instant::now();
            if st.answered != seen || st.in_flight == 0 || now >= deadline {
                return false;
            }
            st = self
                .changed
                .wait_timeout(st, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

/// A round's caller, watched for an outage: once one is clear, it starts
/// no more calls.
struct Watched<'a> {
    inner: &'a dyn Caller,
    watch: &'a OutageWatch,
}

impl Caller for Watched<'_> {
    fn call(&self, call: &Call) -> Result<Answer> {
        if let Some(why) = self.watch.declared() {
            return Err(anyhow::Error::new(Outage(why)));
        }
        self.watch.started();
        let answer = self.inner.call(call);
        match &answer {
            Ok(_) => self.watch.answered(),
            Err(e) => self.watch.failed(e),
        }
        answer
    }
    fn note(&self, what: &str) {
        self.inner.note(what)
    }
    fn say(&self, what: &str) {
        self.inner.say(what)
    }
    fn share(&self, shared: Option<Shared>) -> bool {
        self.inner.share(shared)
    }
    fn warmed(&self, label: &str) -> Option<bool> {
        self.inner.warmed(label)
    }
    fn has_room(&self, binding: &Binding) -> bool {
        self.inner.has_room(binding)
    }
    fn fingerprint(&self, repo: &Path) -> Option<String> {
        self.inner.fingerprint(repo)
    }
    fn snapshot(&self, repo: &Path) -> Option<String> {
        self.inner.snapshot(repo)
    }
    fn delta(&self, repo: &Path, from: &str, to: &str) -> Option<(Vec<String>, String)> {
        self.inner.delta(repo, from, to)
    }
    fn changed(&self, repo: &Path, from: &str, to: &str) -> Option<Vec<super::change::Changed>> {
        self.inner.changed(repo, from, to)
    }
    fn revert(&self, repo: &Path, from: &str, path: &str, existed: bool) -> Result<()> {
        self.inner.revert(repo, from, path, existed)
    }
    fn rekey(&self, label: &str) {
        self.inner.rekey(label)
    }
}

/// A reviewer's work: its sessions' usage, its findings, and its stage's
/// note.
type Done = (Vec<Usage>, Vec<Raw>, Option<String>);

/// A unit of work a round finished, kept in the run's folder as it
/// finishes, so a round paused part way, as by an outage, goes on from
/// there and runs only what didn't finish.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum UnitDone {
    Briefs {
        briefs: BTreeMap<String, String>,
        missing: Vec<String>,
    },
    Reviewer {
        label: String,
        usage: Vec<Usage>,
        found: Vec<Raw>,
        note: Option<String>,
    },
    ReviewerFailed {
        label: String,
        why: String,
    },
    Judge {
        lens: String,
        usage: Vec<Usage>,
        ratings: Vec<roles::Rating>,
    },
}

/// A round's finished units: those kept from before it paused, and a
/// record of each as it finishes.
struct Units {
    path: PathBuf,
    kept: Vec<UnitDone>,
    lock: std::sync::Mutex<()>,
}

impl Units {
    /// Round `round`'s, in `dir`: what it kept, if it's going on from a
    /// pause, or a new record.
    fn open(dir: &Path, round: u32, going_on: bool) -> Units {
        let path = dir.join(format!("round-{round}.units.jsonl"));
        let kept = if going_on {
            fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        } else {
            let _ = fs::remove_file(&path);
            Vec::new()
        };
        Units {
            path,
            kept,
            lock: Default::default(),
        }
    }

    fn keep(&self, unit: &UnitDone) {
        let _held = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(line) = serde_json::to_string(unit) {
            use std::io::Write;
            if let Some(dir) = self.path.parent() {
                let _ = fs::create_dir_all(dir);
            }
            if let Ok(mut f) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    fn briefs(&self) -> Option<(BTreeMap<String, String>, Vec<String>)> {
        self.kept.iter().find_map(|u| match u {
            UnitDone::Briefs { briefs, missing } => Some((briefs.clone(), missing.clone())),
            _ => None,
        })
    }

    fn reviewer(&self, label: &str) -> Option<Result<Done>> {
        self.kept.iter().find_map(|u| match u {
            UnitDone::Reviewer {
                label: l,
                usage,
                found,
                note,
            } if l == label => Some(Ok((usage.clone(), found.clone(), note.clone()))),
            UnitDone::ReviewerFailed { label: l, why } if l == label => Some(Err(anyhow!("{why}"))),
            _ => None,
        })
    }

    fn judge(&self, lens: &str) -> Option<(Vec<Usage>, Vec<roles::Rating>)> {
        self.kept.iter().find_map(|u| match u {
            UnitDone::Judge {
                lens: l,
                usage,
                ratings,
            } if l == lens => Some((usage.clone(), ratings.clone())),
            _ => None,
        })
    }

    fn going_on(&self) -> bool {
        !self.kept.is_empty()
    }
}

/// A stage's error, said when it's one that did no work, as a step's is.
fn idle_said(caller: &dyn Caller, label: &str, e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}");
    if text.starts_with("did no work: ") {
        caller.say(&format!("{label}: {text}"));
    }
    e
}

/// Text as its words, to tell what's new from what's said again in other
/// case, spacing or punctuation.
fn words(text: &str) -> String {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The items a handoff says it checked, each as its words.
fn items(text: &str) -> Vec<String> {
    text.split(['\n', ';', ','])
        .map(words)
        .filter(|w| !w.is_empty())
        .collect()
}

/// A reader's call that ended without calling a tool: it never read the
/// code beyond what it was given.
fn read_something(caller: &dyn Caller, label: &str, legs: &Legs) -> Result<()> {
    if legs.tools == Some(0) {
        return Err(no_work(
            caller,
            label,
            "it ended without calling a tool, so it never read the change beyond what it was \
             given",
        ));
    }
    Ok(())
}

/// Runs `call`, relayed if it asks to be. A handoff that can't be read, or
/// one that did no work, is an error, never a finished call: what's left
/// would go unchecked. A fixer's handoff did no work when the checkout is as
/// its session found it; a reader's, when it found nothing new, checked
/// nothing more, and has the same left as the last.
pub fn relayed(
    caller: &dyn Caller,
    call: &Call,
    used: &dyn Fn(&Call, &Answer) -> Usage,
) -> Result<Legs> {
    let mut usage = Vec::new();
    let mut handed = Vec::new();
    let mut checked: Vec<String> = Vec::new();
    let mut earlier: Vec<roles::Earlier> = Vec::new();
    let mut last_left: Option<String> = None;
    // What the sessions said they checked, as words, to tell a handoff
    // that checked more.
    let mut seen: Vec<String> = Vec::new();
    // A fixer's sessions: what each said it changed, which is in the
    // checkout for the next, and what each read.
    let mut changed: Vec<String> = Vec::new();
    let mut read: Vec<String> = Vec::new();
    // A fixer's checkout as its session starts, to see whether it changed.
    let mut tree = if call.writes {
        caller.snapshot(call.repo)
    } else {
        None
    };
    let mut tools = Some(0);
    let mut prompt = call.prompt.to_string();
    let mut session: u32 = 1;
    loop {
        let label = if session == 1 {
            call.label.clone()
        } else {
            format!("{} (session {session})", call.label)
        };
        let this = Call {
            role: call.role,
            binding_name: call.binding_name,
            binding: call.binding,
            prompt: &prompt,
            artifact: call.artifact,
            writes: call.writes,
            no_tools: call.no_tools,
            relay_tokens: call.relay_tokens,
            repo: call.repo,
            label: label.clone(),
        };
        let answer = caller.call(&this)?;
        tools = match (tools, answer.usage.as_ref().filter(|u| u.counts_tools)) {
            (Some(n), Some(u)) => Some(n + u.tool_calls),
            _ => None,
        };
        let mut u = used(&this, &answer);
        u.part = Some(format!("leg {session}"));
        usage.push(u);
        let handoff = if call.relay_tokens.is_some() {
            roles::handoff_of(&answer.text)
        } else {
            None
        };
        let Some(handoff) = handoff else {
            if usage.len() == 1 {
                usage[0].part = None;
            }
            return Ok(Legs {
                usage,
                handed,
                last: answer.text,
                label,
                tools,
            });
        };
        let (body, h) = handoff.map_err(unreadable).with_context(|| label.clone())?;
        let left = h.left.trim().to_string();
        if call.writes {
            let done = roles::read_fixed(&body)
                .map(|f| f.done.trim().to_string())
                .unwrap_or_default();
            // By the checkout, which says what the session did whatever its
            // answer says; by its words only when there's no checkout to
            // read, and then in any case, spacing or punctuation.
            let now = tree.as_ref().and_then(|_| caller.snapshot(call.repo));
            match (&tree, &now) {
                (Some(before), Some(after)) if before == after => {
                    return Err(no_work(
                        caller,
                        &label,
                        "handed off with no change to the working tree while its blockers are \
                         open, so its relay stopped there and what's left went unfixed",
                    ));
                }
                (Some(_), Some(_)) => {}
                _ => {
                    let more = !words(&done).is_empty()
                        && changed.last().is_none_or(|c| words(c) != words(&done));
                    if !more
                        && last_left
                            .as_deref()
                            .is_some_and(|l| words(l) == words(&left))
                    {
                        return Err(no_work(
                            caller,
                            &label,
                            "handed off again with nothing more done and the same left to fix, \
                             so its relay stopped there and what's left went unfixed",
                        ));
                    }
                }
            }
            tree = now;
            last_left = Some(left.clone());
            caller.say(&format!(
                "{label}: out of room, so it handed off what it changed to session {}",
                session + 1
            ));
            changed.push(done);
            read.push(h.read.clone());
            handed.push(body);
            session += 1;
            prompt = format!(
                "{}{}",
                call.prompt,
                roles::fix_continuation(&changed, &read, &h.planned, &left, session)
            );
            continue;
        }
        let new = roles::earlier_of(&body);
        let said = items(&h.checked);
        let more = said.iter().any(|c| !seen.contains(c));
        if new.is_empty()
            && !more
            && last_left
                .as_deref()
                .is_some_and(|l| words(l) == words(&left))
        {
            return Err(no_work(
                caller,
                &label,
                "handed off again with nothing new found, nothing more checked and the same left \
                 to check, so its relay stopped there and what's left went unchecked",
            ));
        }
        seen.extend(said);
        last_left = Some(left.clone());
        caller.say(&format!(
            "{label}: out of room, so it handed off what it found ({} so far) to session {}",
            earlier.len() + new.len(),
            session + 1
        ));
        checked.push(h.checked);
        earlier.extend(new);
        handed.push(body);
        session += 1;
        prompt = format!(
            "{}{}",
            call.prompt,
            roles::continuation(&checked, &left, &earlier, session)
        );
    }
}

/// Findings merged across sessions: each once, by its title and where it
/// points.
fn merged<T>(
    lists: Vec<Vec<T>>,
    key: impl Fn(&T) -> (String, Option<String>, Option<u64>),
) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for item in lists.into_iter().flatten() {
        let (title, file, line) = key(&item);
        if seen.insert((title.trim().to_lowercase(), file, line)) {
            out.push(item);
        }
    }
    out
}

fn raw_key(f: &Raw) -> (String, Option<String>, Option<u64>) {
    (f.title.clone(), f.file.clone(), f.line)
}

/// A fix from every session its call took: what each changed, in order,
/// and what the last declined and briefed, after any earlier session's.
fn fixed_of(legs: &Legs) -> Result<roles::Fixed> {
    let last = roles::read_fixed(&legs.last).map_err(unreadable)?;
    if legs.handed.is_empty() {
        return Ok(last);
    }
    let earlier: Vec<roles::Fixed> = legs
        .handed
        .iter()
        .filter_map(|text| roles::read_fixed(text).ok())
        .collect();
    let done: Vec<String> = earlier
        .iter()
        .chain(std::iter::once(&last))
        .map(|f| f.done.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    let mut declined: Vec<roles::Declined> = Vec::new();
    let mut briefs = BTreeMap::new();
    for f in earlier.into_iter().chain(std::iter::once(last)) {
        for d in f.declined {
            if !declined.iter().any(|e| e.finding == d.finding) {
                declined.push(d);
            }
        }
        briefs.extend(f.briefs);
    }
    Ok(roles::Fixed {
        done: done.join("\n"),
        declined,
        briefs,
    })
}

/// A call outside a round's fan-out, watched for an outage: one that
/// couldn't reach its model waits `outage_wait_secs` and goes once more,
/// and if that can't either, the review pauses, saved, as a round's
/// outage does, rather than taking a gateway that's down for a step that
/// failed. Anything else it returns as it came.
fn watched<T>(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    label: &str,
    mut f: impl FnMut(&mut Run) -> Result<T>,
) -> Result<T> {
    match f(run) {
        Err(e) if is_connection(&e) => {
            caller.say(&format!(
                "{label}: its model couldn't be reached, so it waits {}s and goes once more",
                s.outage_wait_secs
            ));
            thread::sleep(std::time::Duration::from_secs(s.outage_wait_secs));
            match f(run) {
                Err(e) if is_connection(&e) => {
                    let why = format!("{label}: {e:#}");
                    run.outage = Some(Paused {
                        round: run.round,
                        why: why.clone(),
                    });
                    Err(anyhow::Error::new(Outage(why)))
                }
                other => other,
            }
        }
        other => other,
    }
}

/// An outage out of a step's result, to go up: anything else stays in it.
fn outage_out<T>(r: Result<T>) -> Result<Result<T>> {
    match r {
        Err(e) if is_outage(&e) => Err(e),
        other => Ok(other),
    }
}

/// One call to a role, on the record.
#[allow(clippy::too_many_arguments)]
fn ask(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    role: Role,
    what: &str,
    prompt: &str,
    artifact: Option<&Path>,
) -> Result<String> {
    let name = s.cfg.binding_name(role, None)?;
    let repo = run.reading().to_path_buf();
    let call = Call {
        role,
        binding_name: name,
        binding: s.cfg.binding(role, None)?,
        prompt,
        artifact,
        writes: false,
        no_tools: false,
        relay_tokens: None,
        repo: &repo,
        label: what.to_string(),
    };
    let answer = caller.call(&call)?;
    record(run, s.cfg, &call, input_bytes(prompt, artifact), &answer);
    Ok(answer.text)
}

/// The form an answer is asked in: the reminder its prompt ends with, its
/// shape for a repair, and what reads it.
type InForm<'a, T> = (&'a str, roles::Shape, fn(&str) -> Result<T>);

/// [`ask`], for an answer in a JSON form: the form said again at the end
/// of its prompt; an answer that can't be read put in form by the Sizer;
/// and one that still can't be, asked again once, fresh, before it fails.
#[allow(clippy::too_many_arguments)]
fn ask_in_form<T>(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    role: Role,
    what: &str,
    prompt: &str,
    artifact: Option<&Path>,
    (reminder, shape, read): InForm<'_, T>,
) -> Result<T> {
    let prompt = format!("{prompt}{reminder}");
    let once = |run: &mut Run, label: &str| -> Result<T> {
        let text = ask(run, s, caller, role, label, &prompt, artifact)?;
        let repo = run.reading().to_path_buf();
        let asked = Call {
            role,
            binding_name: s.cfg.binding_name(role, None)?,
            binding: s.cfg.binding(role, None)?,
            prompt: &prompt,
            artifact,
            writes: false,
            no_tools: false,
            relay_tokens: None,
            repo: &repo,
            label: label.to_string(),
        };
        let repairs = std::sync::Mutex::new(Vec::new());
        let got = repaired(caller, s.cfg, &text, &asked, &repairs, shape, read);
        for mut u in repairs.into_inner().unwrap_or_else(|e| e.into_inner()) {
            u.round = run.round;
            run.usage.push(u);
        }
        got
    };
    match once(run, what) {
        Err(first) if first.downcast_ref::<Unreadable>().is_some() => {
            let why = "its answer couldn't be read, even put in form";
            caller.say(&format!(
                "restarted fresh: {what}, attempt 2, because {why}"
            ));
            let start = run.usage.len();
            let again = once(run, &format!("{what}, again"));
            for u in &mut run.usage[start..] {
                u.restart = Some(why.to_string());
            }
            again
                .map_err(|then| anyhow!("came back unusable twice: first {first:#}; then {then:#}"))
        }
        other => other,
    }
}

/// [`ask`], of a call with no tools and no attachment: it answers from its
/// prompt alone.
fn ask_bare(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    role: Role,
    what: &str,
    prompt: &str,
) -> Result<String> {
    let name = s.cfg.binding_name(role, None)?;
    let repo = run.reading().to_path_buf();
    let call = Call {
        role,
        binding_name: name,
        binding: s.cfg.binding(role, None)?,
        prompt,
        artifact: None,
        writes: false,
        no_tools: true,
        relay_tokens: None,
        repo: &repo,
        label: what.to_string(),
    };
    let answer = caller.call(&call)?;
    record(run, s.cfg, &call, input_bytes(prompt, None), &answer);
    Ok(answer.text)
}

/// A round's diff, as files reviewers are sent.
pub struct Artifacts {
    /// Without generated files, which it lists.
    pub diff: PathBuf,
    /// Everything, when generated files were left out.
    pub full: Option<PathBuf>,
}

impl Artifacts {
    /// The diff a lens's reviewers get.
    fn for_lens(&self, lens: &Lens) -> &Path {
        match (&self.full, lens.include_generated) {
            (Some(full), true) => full,
            _ => &self.diff,
        }
    }
}

/// A round's change as every role gets it: what the change says it's for,
/// then its diff.
fn write_artifact(s: &Setting, run: &Run, change: &Change, round: u32) -> Result<Artifacts> {
    fs::create_dir_all(&s.work_dir)?;
    let mut intent = super::change::intent(s.description.as_deref(), &change.commits);
    // A model's account of the change, which facts-only reviewers don't get.
    if let Some(brief) = s
        .builder_brief
        .as_ref()
        .filter(|_| s.cfg.facts.reviewers == ReviewerInput::All)
    {
        intent.push_str(&stages::scope_section(brief));
    }
    // What earlier commits and the docs say is said about the change too;
    // where its tests and callers are is a lead, after it.
    if s.cfg.stages.contains(&Kind::Context) {
        let pack = stages::context_pack(run.reading(), &run.base, change);
        intent.push_str(&pack.said);
        intent.push_str(&pack.leads);
    }
    if let Some(fix) = run
        .fix
        .as_ref()
        .filter(|f| !run.sweep && round == f.round + 1)
    {
        let diff = fs::read_to_string(s.work_dir.join(format!("round-{}.fix.diff", fix.round)))
            .unwrap_or_default();
        if !diff.trim().is_empty() {
            intent.push_str(&format!(
                "# What the fixes after round {} changed\n\nThis round, attack these \
                 changes, and what they touch, first: the rest of the change was attacked \
                 before, and is attacked again before the review ends.\n\n```diff\n{}\n```\n\n",
                fix.round,
                stages::clip_text(&diff, 30 * 1024)
            ));
        }
    }
    let diff = s.work_dir.join(format!("round-{round}.diff"));
    fs::write(&diff, format!("{intent}# The diff\n\n{}", change.diff))?;
    let full = match &change.full {
        Some(text) => {
            let path = s.work_dir.join(format!("round-{round}.full.diff"));
            fs::write(&path, format!("{intent}# The diff\n\n{text}"))?;
            Some(path)
        }
        None => None,
    };
    Ok(Artifacts { diff, full })
}

/// The tier: from paths if they settle it, or else the Sizer's, and higher
/// when it isn't sure.
fn choose_tier(
    run: &mut Run,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
    artifact: &Path,
) -> Result<()> {
    let cfg = s.cfg;
    let tier = if let Some(tier) = plan::tier_by_paths(cfg, change)? {
        run.tier_why = "its paths match".into();
        tier
    } else {
        let described = plan::described_tiers(cfg);
        // First from the file list, the size of each change and what the
        // change says it's for, with no tools: most changes are sized from
        // that alone, in a fraction of the time.
        let light = roles::sizer_light_prompt(
            &described,
            &change::diffstat(change),
            s.description.as_deref(),
            &change.commits,
        );
        let what = "the Sizer, choosing a tier from the file list";
        let quick = outage_out(watched(run, s, caller, what, |run| {
            ask_bare(run, s, caller, Role::Sizer, what, &light)
        }))?
        .and_then(|text| roles::read_sized(&text));
        let (answer, how) = match quick {
            // Only a sure answer: otherwise it reads the change, with its
            // tools, as it always could.
            Ok(sized) if sized.sure && sized.tier != roles::UNKNOWN_TIER => {
                (Ok(sized), "from the file list")
            }
            quick => {
                let why = match quick {
                    Ok(sized) if sized.tier == roles::UNKNOWN_TIER => {
                        format!("couldn't tell from the file list ({})", sized.reason)
                    }
                    Ok(sized) => format!(
                        "wasn't sure of {} from the file list ({})",
                        sized.tier, sized.reason
                    ),
                    Err(e) => format!("couldn't answer from the file list ({e:#})"),
                };
                caller.say(&format!("the Sizer {why}, so it reads the change"));
                let prompt = roles::sizer_prompt(&described, &change.files);
                let what = "the Sizer, choosing a tier";
                let full = outage_out(watched(run, s, caller, what, |run| {
                    ask(run, s, caller, Role::Sizer, what, &prompt, Some(artifact))
                }))?
                .and_then(|text| roles::read_sized(&text));
                (full, "having read the change")
            }
        };
        match answer {
            Ok(sized) => match cfg.tiers.iter().position(|t| t.id == sized.tier) {
                Some(at) if sized.sure => {
                    run.tier_why = format!("the Sizer, {how}: {}", sized.reason);
                    &cfg.tiers[at]
                }
                Some(at) => {
                    run.tier_why =
                        format!("the Sizer wasn't sure of {}, so one higher", sized.tier);
                    &cfg.tiers[(at + 1).min(cfg.tiers.len() - 1)]
                }
                None => {
                    run.tier_why =
                        format!("the Sizer named no tier ({}), so the highest", sized.tier);
                    plan::highest(cfg)
                }
            },
            Err(e) => {
                run.tier_why = format!("the Sizer failed ({e:#}), so the highest");
                plan::highest(cfg)
            }
        }
    };
    run.tier = tier.id.clone();
    run.fan_out = tier.fan_out;
    Ok(())
}

/// The lenses: those triggered, plus those the Orchestrator says apply. If
/// it can't say, every candidate stays in, and a person hears why.
fn choose_lenses(
    run: &mut Run,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
    artifact: &Path,
) -> Result<()> {
    let cfg = s.cfg;
    let tier = cfg
        .tiers
        .iter()
        .find(|t| t.id == run.tier)
        .ok_or_else(|| anyhow!("no tier {}", run.tier))?;
    // Asked again each round, of the change as it is then, since fixes can
    // take it where a lens it didn't need at first applies. It's judged
    // afresh, without what was ruled out before, which may no longer hold.
    // A lens that has run stays on, so the version it ends at is one every
    // lens that attacked agrees on.
    let later = run.round > 1;
    let candidates: Vec<(&Lens, Why)> = plan::select(cfg, tier, change)?
        .into_iter()
        .filter(|(lens, _)| !run.lenses.contains_key(&lens.id))
        .collect();
    let described: Vec<(&str, &str)> = candidates
        .iter()
        .filter_map(|(lens, why)| match why {
            Why::Description(d) => Some((lens.id.as_str(), d.as_str())),
            _ => None,
        })
        .collect();
    // What always escalates is asked once, at the start.
    let escalating: &[String] = if later { &[] } else { &cfg.always_escalate };
    let mut wanted: Option<Vec<String>> = None;
    if !described.is_empty() || !escalating.is_empty() {
        let prompt = roles::orchestrator_prompt(&described, escalating);
        let what = "the Orchestrator, choosing lenses";
        let answer = outage_out(watched(run, s, caller, what, |run| {
            ask(
                run,
                s,
                caller,
                Role::Orchestrator,
                what,
                &prompt,
                Some(artifact),
            )
        }))?
        .and_then(|text| selected_of(run, s, caller, &text, &described, escalating));
        match answer {
            Ok(selected) => {
                for item in selected.escalate.unwrap_or_default() {
                    if cfg.always_escalate.contains(&item) {
                        escalate(run, format!("always_escalate: {item}"));
                    }
                }
                wanted = Some(selected.lenses);
            }
            Err(e) => escalate(
                run,
                format!("the Orchestrator couldn't check this change's descriptions: {e:#}"),
            ),
        }
    }
    for (lens, why) in candidates {
        let described = matches!(why, Why::Description(_));
        if described && wanted.as_ref().is_some_and(|w| !w.contains(&lens.id)) {
            continue;
        }
        let joined = if later {
            let what = match &why {
                Why::Matched(what) => format!("its {what} matched"),
                Why::Required => "its tier requires it".to_string(),
                Why::Description(_) => "the Orchestrator chose it from its description".to_string(),
            };
            format!("round {}: {what}", run.round)
        } else {
            String::new()
        };
        run.lenses.insert(
            lens.id.clone(),
            LensResult {
                id: lens.id.clone(),
                criteria: lens.criteria.clone(),
                rounds: Vec::new(),
                done: Vec::new(),
                outcome: Outcome::Open,
                joined,
                skipped: Vec::new(),
            },
        );
    }
    if run.lenses.is_empty() {
        escalate(run, "no lens applies to this change".into());
    }
    Ok(())
}

/// Starts a review of `change`.
pub fn start(
    id: &str,
    repo: &Path,
    base: &str,
    head: Option<&str>,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
) -> Result<Run> {
    let mut run = Run {
        id: id.to_string(),
        starter: None,
        // Whole, so that history finds it from anywhere.
        repo: repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf()),
        base: base.to_string(),
        head: head.map(str::to_string),
        tier: String::new(),
        tier_why: String::new(),
        fan_out: 1,
        round: 1,
        stage: Stage::Review,
        status: Status::Running,
        waiting: None,
        lenses: BTreeMap::new(),
        escalations: Vec::new(),
        disagreements: Vec::new(),
        usage: Vec::new(),
        description: s.description.clone(),
        converge: s.converge,
        steelmans: Vec::new(),
        fix_briefs: None,
        reattack: s.cfg.reattack,
        builder_brief: s.builder_brief.clone(),
        weakened: Vec::new(),
        answered_by: Vec::new(),
        fix: None,
        sweep: false,
        reported: Vec::new(),
        facts: Vec::new(),
        prefixes: Vec::new(),
        learned: Vec::new(),
        // An Orchestrator that can't write leaves it to whoever ran the
        // review, rather than to nobody.
        fixer: if s
            .cfg
            .binding(Role::Orchestrator, None)
            .is_ok_and(|b| b.writes)
        {
            Fixer::Orchestrator
        } else {
            Fixer::Session
        },
        rescope: false,
        fixing: None,
        head_copy: s.head_copy.clone(),
        pr: s.pr,
        rulings: Vec::new(),
        partial_fix: None,
        flags: Vec::new(),
        outage: None,
        routes: Vec::new(),
    };
    let artifacts = write_artifact(s, &run, change, 0)?;
    choose_tier(&mut run, s, change, caller, &artifacts.diff)?;
    choose_lenses(&mut run, s, change, caller, &artifacts.diff)?;
    if let Some(why) = plan::config_escalation(s.cfg, change) {
        escalate(&mut run, why);
    }
    if s.cfg.human_gate.asks_scope() {
        run.stage = Stage::Scope;
    }
    Ok(run)
}

fn lens_of<'a>(cfg: &'a Config, id: &str) -> Result<&'a Lens> {
    cfg.lenses
        .iter()
        .find(|l| l.id == id)
        .ok_or_else(|| anyhow!("no lens {id}"))
}

/// Every open lens's reviewers, at once, each a fresh session that sees
/// only its brief and the change.
/// What one reviewer is asked to do.
enum Brief {
    /// Review, and answer with findings.
    Review { prompt: String, repo: PathBuf },
    /// Probe in a copy, and answer with findings; what the prober wrote is
    /// then kept at `kept`, told from the copy's tree `before`, for the next
    /// round's prober in its slot.
    Probe {
        prompt: String,
        repo: PathBuf,
        kept: PathBuf,
        before: Option<String>,
    },
    /// Propose mutants, which are then run against the tests in `runs`.
    Mutate {
        prompt: String,
        repo: PathBuf,
        runs: PathBuf,
        command: String,
    },
    /// Can't run, and why.
    Fail(String),
}

/// What attacking a round gives back: each lens's reviewers' findings, and
/// what its stages noted.
/// A lens's Judge's answer, if it had findings to judge.
type Judged = Option<Result<(Vec<Usage>, Vec<roles::Rating>)>>;

/// Each lens's reviewers' findings, its Judge's answer, and its notes.
type Attacked = (
    BTreeMap<String, Vec<Result<Vec<Raw>>>>,
    BTreeMap<String, Judged>,
    BTreeMap<String, Vec<String>>,
);

/// Where a round's copies go, and what they're made from: the round's warm
/// copy, or the checkout.
#[derive(Clone, Copy)]
struct Copying<'a> {
    dir: Option<&'a Path>,
    source: &'a Path,
}

/// The file in a review's folder that says where each lens of the round
/// under way stands, written as each moves: for `acr ask` and a person to
/// see mid-round, when `run.json` is saved only between stages.
pub const LENS_STATES: &str = "lenses.json";

/// Where each lens of a round stands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LensStates {
    pub round: u32,
    pub lenses: BTreeMap<String, LensNow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LensNow {
    /// `reviewing`, `no findings`, `no usable answer`, `judging`, `judged`
    /// or `judge failed`.
    pub state: String,
    pub reviewers: usize,
    pub answered: usize,
    /// When it last moved, in Unix milliseconds.
    pub at: i64,
}

/// [`LensStates`], kept in its file as each lens moves.
struct Board {
    path: PathBuf,
    states: std::sync::Mutex<LensStates>,
}

impl Board {
    fn new(dir: &Path, round: u32, lenses: &[(&Lens, usize)]) -> Board {
        let now = crate::util::now_ms();
        let states = LensStates {
            round,
            lenses: lenses
                .iter()
                .map(|(l, reviewers)| {
                    let now = LensNow {
                        state: "reviewing".into(),
                        reviewers: *reviewers,
                        answered: 0,
                        at: now,
                    };
                    (l.id.clone(), now)
                })
                .collect(),
        };
        let board = Board {
            path: dir.join(LENS_STATES),
            states: std::sync::Mutex::new(states),
        };
        board.write();
        board
    }

    fn change(&self, lens: &str, f: impl FnOnce(&mut LensNow)) {
        {
            let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(now) = states.lenses.get_mut(lens) {
                f(now);
                now.at = crate::util::now_ms();
            }
        }
        self.write();
    }

    fn answered(&self, lens: &str) {
        self.change(lens, |l| l.answered += 1);
    }

    fn set(&self, lens: &str, state: &str) {
        self.change(lens, |l| l.state = state.to_string());
    }

    /// Written whole and moved into place, so a reader never sees half.
    fn write(&self) {
        let text = {
            let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
            serde_json::to_vec_pretty(&*states)
        };
        let Ok(text) = text else { return };
        let part = self
            .path
            .with_extension(format!("json.{}", crate::util::new_id()));
        if fs::write(&part, text).is_ok() && fs::rename(&part, &self.path).is_err() {
            let _ = fs::remove_file(&part);
        }
    }
}

fn attack(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    artifacts: &Artifacts,
    change: &Change,
    copying: Copying,
    gate_text: &str,
) -> Result<Attacked> {
    let Copying { dir, source } = copying;
    let cfg = s.cfg;
    // Every call this round makes is watched for an outage, and every unit
    // of work is restarted once if it comes back unusable.
    let watch = OutageWatch::new(s.outage_wait_secs);
    let watched = Watched {
        inner: caller,
        watch: &watch,
    };
    let caller: &dyn Caller = &watched;
    let restarter = Restarter::new(caller, Some(&watch));
    let restarter = &restarter;
    // What a round paused by an outage finished, which it keeps.
    let going_on = run.outage.as_ref().is_some_and(|o| o.round == run.round);
    let units = Units::open(&s.work_dir, run.round, going_on);
    let units = &units;
    if going_on {
        caller.note(&format!(
            "round {}: going on after an outage, keeping what its units finished",
            run.round
        ));
        run.prefixes.retain(|p| p.round != run.round);
    }
    let scope = match run.head.as_deref() {
        Some(head) => format!("the change from {} to {head}", run.base),
        None => format!("the working tree against {}", run.base),
    };
    let open: Vec<String> = run
        .lenses
        .values()
        .filter(|l| l.outcome == Outcome::Open)
        .map(|l| l.id.clone())
        .collect();
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Each open lens's brief for this change, from one call, before any
    // reviewer starts.
    // Round 1's lenses are briefed by the change's builder, if it wrote a
    // brief; the next round's, after a fix, by the steelman. The brief
    // stage covers any lens they left out.
    // Facts-only reviewers get nothing a model wrote: no brief is asked for,
    // and none given.
    let facts_only = cfg.facts.reviewers == ReviewerInput::FactsOnly;
    let given: BTreeMap<String, String> = if facts_only {
        BTreeMap::new()
    } else if run.round == 1 {
        s.builder_brief
            .as_ref()
            .map(|b| b.lenses.clone())
            .unwrap_or_default()
    } else {
        run.steelmans
            .last()
            .filter(|m| m.round + 1 == run.round)
            .map(|m| m.briefs.clone())
            .or_else(|| {
                run.fix_briefs
                    .as_ref()
                    .filter(|b| b.round == run.round)
                    .map(|b| b.briefs.clone())
            })
            .unwrap_or_default()
    };
    let unbriefed: Vec<String> = open
        .iter()
        .filter(|id| !given.contains_key(*id))
        .cloned()
        .collect();
    let mut briefs: BTreeMap<String, String> = if let Some((kept, missing)) = units.briefs() {
        for id in missing {
            notes
                .entry(id)
                .or_default()
                .push("no brief: the briefer's answer has none for it".into());
        }
        kept
    } else if cfg.stages.contains(&Kind::Brief) && !unbriefed.is_empty() && !facts_only {
        let lenses: Vec<stages::ToBrief> = unbriefed
            .iter()
            .map(|id| {
                lens_of(cfg, id).map(|l| stages::ToBrief {
                    id: &l.id,
                    criteria: &l.criteria,
                    attack: l.attack.as_deref(),
                })
            })
            .collect::<Result<_>>()?;
        let prompt = stages::briefer_prompt(&scope, &lenses, cfg.runbook.as_deref());
        match ask(
            run,
            s,
            caller,
            Role::Orchestrator,
            "the Orchestrator, briefing the lenses",
            &prompt,
            Some(&artifacts.diff),
        )
        .and_then(|text| briefs_of(run, s, caller, &text, &unbriefed))
        {
            // A lens the briefs leave out, whatever form they came in,
            // is no brief, and says so.
            Ok(briefs) => {
                let missing: Vec<String> = unbriefed
                    .iter()
                    .filter(|id| !briefs.contains_key(*id))
                    .cloned()
                    .collect();
                for id in &missing {
                    notes
                        .entry(id.clone())
                        .or_default()
                        .push("no brief: the briefer's answer has none for it".into());
                }
                units.keep(&UnitDone::Briefs {
                    briefs: briefs.clone(),
                    missing,
                });
                briefs
            }
            Err(e) => {
                for id in &unbriefed {
                    notes
                        .entry(id.clone())
                        .or_default()
                        .push(format!("no brief: {e:#}"));
                }
                BTreeMap::new()
            }
        }
    } else {
        BTreeMap::new()
    };
    briefs.extend(given);
    let copies = dir.map(Path::to_path_buf);
    let name = run
        .repo
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_owned());
    let command = cfg
        .tests
        .command
        .clone()
        .or_else(|| stages::test_command(run.reading()));
    // After a fix, where to look first, said last in each reviewer's brief.
    let focus = if cfg.savings.focus_after_fix {
        focus_note(run, s)
    } else {
        None
    };
    if focus.is_some() {
        caller.say(&format!(
            "round {}: the reviewers are told what the last fix changed, as where to look first",
            run.round
        ));
    }
    let mut asks: Vec<(&Lens, Brief, String)> = Vec::new();
    for id in &open {
        let lens = lens_of(cfg, id)?;
        let binding = cfg.binding(Role::Reviewer, Some(lens))?;
        let own = binding.changes_files() && copies.is_some();
        let stage = stages::of_lens(&lens.id, &cfg.stages);
        // The gate ran before the reviewers.
        if stage == Some(Kind::Gate) {
            continue;
        }
        // A reviewer that can run things gets the runbook too.
        let brief_text = briefs.get(&lens.id).map(String::as_str);
        let runbook = cfg.runbook.as_deref().filter(|_| own);
        let not_findings = if facts_only {
            ""
        } else {
            s.not_findings.as_str()
        };
        let guide = stages::guide(lens.attack.as_deref(), brief_text, runbook) + gate_text;
        // A mutation reviewer only proposes, so one is enough.
        let fan_out = if stage == Some(Kind::Mutation) {
            1
        } else {
            run.fan_out
        };
        for n in 1..=fan_out {
            let repo = match &copies {
                Some(dir) if own => {
                    let copy = dir.join(format!("{}-{n}", lens.id)).join(&name);
                    super::change::copy_of(source, &copy)
                        .with_context(|| format!("making {}'s reviewer a copy", lens.id))?;
                    copy
                }
                _ => run.reading().to_path_buf(),
            };
            let brief = match stage {
                None
                | Some(Kind::Context)
                | Some(Kind::Brief)
                | Some(Kind::Gate)
                | Some(Kind::TestWriting) => Brief::Review {
                    prompt: roles::join(&roles::reviewer_parts(
                        lens,
                        &roles::ReviewerCall {
                            scope: &scope,
                            not_findings,
                            own_copy: own,
                            runbook,
                            brief: brief_text,
                            gate: gate_text,
                        },
                    )),
                    repo,
                },
                Some(Kind::Claims) => Brief::Review {
                    prompt: stages::claims_prompt(
                        &stages::claims(change, s.description.as_deref()),
                        &scope,
                        not_findings,
                        own,
                        &guide,
                    ),
                    repo,
                },
                Some(Kind::Probes) if !own => Brief::Fail(
                    "the probes stage runs scenarios, so its reviewer's binding needs a shell"
                        .into(),
                ),
                Some(Kind::Probes) => {
                    // Earlier rounds' probes from this slot go back in, to
                    // run again on the change as it is now.
                    let kept = s.work_dir.join(format!("probes-{n}.diff"));
                    // As the round found them, for a restarted prober: the
                    // first keeps its own probes there before its answer is
                    // read.
                    let _ = match fs::read(&kept) {
                        Ok(bytes) => fs::write(probes_at_start(&kept), bytes),
                        Err(_) => fs::remove_file(probes_at_start(&kept)),
                    };
                    let (before, carried) = stages::carry_probes(&repo, &kept);
                    if carried == stages::Carried::Unapplied {
                        notes.entry(lens.id.clone()).or_default().push(format!(
                            "earlier rounds' probes didn't apply to this version, so prober {n} \
                             was given them to bring over"
                        ));
                    }
                    Brief::Probe {
                        prompt: stages::probes_prompt(
                            &scope,
                            not_findings,
                            command.as_deref(),
                            &guide,
                            &carried,
                        ),
                        repo,
                        kept,
                        before,
                    }
                }
                Some(Kind::Mutation) => match (&copies, &command) {
                    (Some(dir), Some(command)) => Brief::Mutate {
                        prompt: stages::mutants_prompt(
                            &scope,
                            cfg.tests.mutants.unwrap_or(12),
                            &guide,
                        ) + roles::MUTANTS_REMINDER,
                        repo,
                        runs: stages::mutation_dir(dir, &lens.id, n),
                        command: command.clone(),
                    },
                    (None, _) => Brief::Fail("the mutation stage needs a place for copies".into()),
                    (_, None) => Brief::Fail(
                        "the mutation stage can't tell how this repository's tests run: \
                         set tests.command in its review config"
                            .into(),
                    ),
                },
            };
            let mut brief = brief;
            if let (Some(focus), Brief::Review { prompt, .. } | Brief::Probe { prompt, .. }) =
                (&focus, &mut brief)
            {
                prompt.push_str(focus);
            }
            let who = match &brief {
                Brief::Probe { .. } => "prober",
                Brief::Mutate { .. } => "mutant proposer",
                Brief::Review { .. } | Brief::Fail(_) => "reviewer",
            };
            let label = if fan_out > 1 {
                format!("{}: {who} {n}", lens.id)
            } else {
                format!("{}: {who}", lens.id)
            };
            asks.push((lens, brief, label));
        }
    }
    let limit = std::time::Duration::from_secs(cfg.tests.timeout.unwrap_or(900));
    let parallel = cfg.tests.parallel.unwrap_or(2) as usize;
    let checkout = source;
    // A reviewer's findings, said at once when its answer can't be read,
    // rather than only when the round ends.
    let repairs: std::sync::Mutex<Vec<Usage>> = Default::default();
    let repairs = &repairs;
    // What a reviewer's sessions used when its answer couldn't be used:
    // spent all the same, so counted with the round's other calls.
    let unused: std::sync::Mutex<Vec<Usage>> = Default::default();
    let unused = &unused;
    let spent = |usage: Vec<Usage>| lock_vec(unused).extend(usage);
    let read = |text: &str, call: &Call| -> Result<Vec<Raw>> {
        findings_of(caller, cfg, text, call, repairs).inspect_err(|e| {
            caller.say(&format!(
                "{}: its answer couldn't be read: {e:#}",
                call.label
            ));
        })
    };
    // The prompt cache's warm-up, for each role's fan-out: its first call
    // goes alone until the cache holds the prefix the rest share.
    let warm_secs = if cfg.facts.shared {
        cfg.facts.warm_secs
    } else {
        0
    };
    let warm_reviewers = CacheWarm::new(warm_secs, cfg.facts.stagger_secs);
    let warm_judges = CacheWarm::new(warm_secs, cfg.facts.stagger_secs);
    let kept_alive: std::sync::Mutex<Vec<Usage>> = Default::default();
    let judge_prompts: std::sync::Mutex<Vec<String>> = Default::default();
    let stop = std::sync::atomic::AtomicBool::new(false);
    // What a fan-out's calls share at the head: ACR's system prompt, the
    // facts, and as much of their own text as they have in common.
    let facts_bytes = run
        .facts
        .iter()
        .find(|f| f.round == run.round && f.given)
        .map_or(0, |f| f.bytes as usize);
    let head = if facts_bytes > 0 {
        crate::setup::SYSTEM_PROMPT.len() + facts_bytes
    } else {
        0
    };
    if head > 0 {
        let prompts: Vec<&str> = asks
            .iter()
            .filter_map(|(_, brief, _)| match brief {
                Brief::Review { prompt, .. }
                | Brief::Probe { prompt, .. }
                | Brief::Mutate { prompt, .. } => Some(prompt.as_str()),
                Brief::Fail(_) => None,
            })
            .collect();
        if prompts.len() > 1 {
            let p = super::facts::Prefix::new(
                run.round,
                "reviewers",
                head + super::facts::common_prefix(&prompts),
            );
            caller.say(&format!("round {}: {}", run.round, p.line()));
            run.prefixes.push(p);
        }
    }
    // One reviewer's work: its call, and whatever its stage runs after.
    // A call's findings, from every session it took: what each handoff
    // already held, then its last answer, each once.
    let gathered = |legs: &Legs, asked: &Call| -> Result<Vec<Raw>> {
        let mut lists = Vec::new();
        // A handoff's findings, put in form as an answer's are.
        for text in &legs.handed {
            lists.push(repaired(
                caller,
                cfg,
                text,
                asked,
                repairs,
                roles::Shape::Findings,
                roles::read_findings,
            )?);
        }
        let last = Call {
            role: asked.role,
            binding_name: asked.binding_name,
            binding: asked.binding,
            prompt: asked.prompt,
            artifact: asked.artifact,
            writes: asked.writes,
            no_tools: asked.no_tools,
            relay_tokens: asked.relay_tokens,
            repo: asked.repo,
            label: legs.label.clone(),
        };
        lists.push(read(&legs.last, &last)?);
        Ok(merged(lists, raw_key))
    };
    // A scouted lens's reviewer: one session lists what it suspects, and
    // each suspicion is confirmed or dropped in a session of its own, from
    // the reviewer's brief and that suspicion alone. Every suspicion is
    // checked, however many there are, as many at once as the bridge runs.
    // One that can't be checked fails the reviewer: an unchecked suspicion is
    // never taken for a clean lens.
    let scouted = |call: &Call, used: &(dyn Fn(&Call, &Answer) -> Usage + Sync)| -> Result<Done> {
        let scout_prompt = format!(
            "{}{}{}",
            call.prompt,
            roles::scout_part(),
            roles::SUSPECTS_REMINDER
        );
        let scout = Call {
            role: call.role,
            binding_name: call.binding_name,
            binding: call.binding,
            prompt: &scout_prompt,
            artifact: call.artifact,
            writes: false,
            no_tools: false,
            relay_tokens: call.relay_tokens,
            repo: call.repo,
            label: call.label.clone(),
        };
        let legs = relayed(caller, &scout, used)?;
        let listed =
            read_something(caller, &call.label, &legs).and_then(|()| {
                let mut lists = Vec::new();
                let in_form = |text: &str| {
                    repaired(
                        caller,
                        cfg,
                        text,
                        &scout,
                        repairs,
                        roles::Shape::Suspects,
                        roles::read_suspects,
                    )
                };
                for text in &legs.handed {
                    lists.push(in_form(text)?);
                }
                lists.push(in_form(&legs.last).with_context(|| {
                    format!("{}: the scout's list couldn't be read", call.label)
                })?);
                Ok(merged(lists, |s: &roles::Suspect| {
                    (s.title.clone(), s.file.clone(), s.line)
                }))
            });
        let mut usage: Vec<Usage> = legs
            .usage
            .into_iter()
            .map(|mut u| {
                u.part = Some(match u.part {
                    Some(leg) => format!("scout, {leg}"),
                    None => "scout".into(),
                });
                u
            })
            .collect();
        let suspects = match listed {
            Ok(suspects) => suspects,
            Err(e) => {
                spent(usage);
                return Err(e);
            }
        };
        caller.say(&format!(
            "{}: the scout suspects {} thing(s), each to be verified on its own",
            call.label,
            suspects.len()
        ));
        let verified: Vec<Result<(Vec<Usage>, Vec<Raw>)>> = thread::scope(|scope| {
            let handles: Vec<_> = suspects
                .iter()
                .enumerate()
                .map(|(i, suspect)| {
                    scope.spawn(move || -> Result<(Vec<Usage>, Vec<Raw>)> {
                        let n = i + 1;
                        let prompt = format!(
                            "{}{}{}",
                            call.prompt,
                            roles::verify_part(suspect),
                            roles::FINDINGS_REMINDER
                        );
                        let verify = |label: &str| -> Result<(Vec<Usage>, Vec<Raw>)> {
                            let check = Call {
                                role: call.role,
                                binding_name: call.binding_name,
                                binding: call.binding,
                                prompt: &prompt,
                                artifact: call.artifact,
                                writes: false,
                                no_tools: false,
                                relay_tokens: call.relay_tokens,
                                repo: call.repo,
                                label: label.to_string(),
                            };
                            let legs = relayed(caller, &check, used)?;
                            let found = read_something(caller, &check.label, &legs)
                                .and_then(|()| gathered(&legs, &check));
                            let usage: Vec<Usage> = legs
                                .usage
                                .into_iter()
                                .map(|mut u| {
                                    u.part = Some(match u.part {
                                        Some(leg) => format!("verify {n}, {leg}"),
                                        None => format!("verify {n}"),
                                    });
                                    u
                                })
                                .collect();
                            match found {
                                Ok(found) => Ok((usage, found)),
                                Err(e) => {
                                    spent(usage);
                                    Err(e)
                                }
                            }
                        };
                        let label = format!("{}: verify {n}", call.label);
                        restarter.once(&label, verify(&label), verify)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("a verifying session's thread died")))
                })
                .collect()
        });
        let mut found = Vec::new();
        let mut failed = Vec::new();
        let mut paused = None;
        for (i, v) in verified.into_iter().enumerate() {
            match v {
                Ok((mut u, mut f)) => {
                    usage.append(&mut u);
                    found.push(std::mem::take(&mut f));
                }
                Err(e) if is_outage(&e) => paused = Some(e),
                Err(e) => failed.push(format!("verify {}: {e:#}", i + 1)),
            }
        }
        // A check an outage left unfinished leaves the reviewer unfinished:
        // the review's continuing runs it again.
        if let Some(e) = paused {
            spent(usage);
            return Err(e);
        }
        if !failed.is_empty() {
            spent(usage);
            bail!(
                "{}: {} of its suspicions couldn't be checked, so the lens isn't clean: {}",
                call.label,
                failed.len(),
                failed.join("; ")
            );
        }
        Ok((usage, merged(found, raw_key), None))
    };
    // One reviewer's work: its call, relayed if its role's calls relay, and
    // whatever its stage runs after.
    let review = |lens: &Lens, brief: &Brief, label: &String, first: bool| -> Result<Done> {
        let artifact = artifacts.for_lens(lens);
        let name = cfg.binding_name(Role::Reviewer, Some(lens))?;
        let binding = cfg.binding(Role::Reviewer, Some(lens))?;
        let (prompt, repo) = match brief {
            Brief::Fail(why) => bail!("{why}"),
            Brief::Review { prompt, repo }
            | Brief::Probe { prompt, repo, .. }
            | Brief::Mutate { prompt, repo, .. } => (prompt, repo),
        };
        // A mutation proposer answers with mutants, not findings, so it
        // runs whole.
        let relay_tokens = if matches!(brief, Brief::Mutate { .. }) {
            None
        } else {
            cfg.sessions.relay(Role::Reviewer)
        };
        // The answer's form, said again at the very end, after all that's
        // shared: a scouted lens's sessions say their own.
        let scouts = matches!(brief, Brief::Review { .. }) && cfg.sessions.scouts(&lens.id);
        let reminded = format!("{prompt}{}", roles::FINDINGS_REMINDER);
        let asked: &str = if scouts || matches!(brief, Brief::Mutate { .. }) {
            prompt
        } else {
            &reminded
        };
        let call = Call {
            role: Role::Reviewer,
            binding_name: name,
            binding,
            prompt: asked,
            artifact: Some(artifact),
            writes: false,
            no_tools: false,
            relay_tokens,
            repo: repo.as_path(),
            label: label.clone(),
        };
        let used = |c: &Call, a: &Answer| -> Usage {
            let mut u = usage(cfg, c, input_bytes(c.prompt, Some(artifact)), a);
            u.fan_out = Some("reviewers".into());
            // Its first session only, which goes alone to warm the cache.
            u.first = first && c.label == *label && c.prompt == asked;
            // Every session of the call is its lens's and its call's, so what
            // all its sessions read counts once, for the call.
            u.lens = Some(lens.id.clone());
            u.call = Some(label.clone());
            u
        };
        if scouts {
            return scouted(&call, &used);
        }
        let legs = relayed(caller, &call, &used)?;
        let got = match brief {
            Brief::Mutate { runs, command, .. } => repaired(
                caller,
                cfg,
                &legs.last,
                &call,
                repairs,
                roles::Shape::Mutants,
                stages::read_mutants,
            )
            .and_then(|mutants| {
                let runner = stages::Runner {
                    repo: checkout,
                    dir: runs,
                    command,
                    parallel,
                    limit,
                };
                let (found, summary) = stages::run_mutants(&runner, &mutants)
                    .map_err(|e| idle_said(caller, label, e))?;
                Ok((found, Some(summary)))
            }),
            Brief::Probe { kept, before, .. } => {
                // Kept before its answer is read, so a prober
                // that answers badly still leaves its probes.
                let (wrote, note) = match before.as_deref() {
                    Some(before) => stages::keep_probes(repo, before, kept),
                    None => (None, None),
                };
                // A prober writes its probes in its copy and runs them: a
                // copy as it was given is one that probed nothing.
                if wrote == Some(false) {
                    Err(no_work(
                        caller,
                        label,
                        "it wrote no probe in its copy, so it ran none",
                    ))
                } else {
                    read_something(caller, label, &legs)
                        .and_then(|()| gathered(&legs, &call))
                        .map(|found| (found, note))
                }
            }
            _ => read_something(caller, label, &legs)
                .and_then(|()| gathered(&legs, &call))
                .map(|found| (found, None)),
        };
        match got {
            Ok((found, note)) => Ok((legs.usage, found, note)),
            Err(e) => {
                spent(legs.usage);
                Err(e)
            }
        }
    };
    // A restarted unit's brief: a prober's in a fresh copy, so it can't see
    // the probes the first wrote; a mutant proposer's with a fresh place to
    // run its mutants; any other's as it was.
    let fresh_brief = |brief: &Brief| -> Result<Brief> {
        Ok(match brief {
            Brief::Probe {
                prompt, repo, kept, ..
            } => {
                let copy = restarted_copy(repo);
                super::change::copy_of(source, &copy)
                    .context("making a restarted prober a fresh copy")?;
                // Earlier rounds' probes, without the ones the failed
                // session wrote.
                let _ = match fs::read(probes_at_start(kept)) {
                    Ok(bytes) => fs::write(kept, bytes),
                    Err(_) => fs::remove_file(kept),
                };
                let (before, _) = stages::carry_probes(&copy, kept);
                Brief::Probe {
                    prompt: prompt.clone(),
                    repo: copy,
                    kept: kept.clone(),
                    before,
                }
            }
            Brief::Mutate {
                prompt,
                repo,
                runs,
                command,
            } => Brief::Mutate {
                prompt: prompt.clone(),
                repo: repo.clone(),
                runs: restarted_copy(runs),
                command: command.clone(),
            },
            Brief::Review { prompt, repo } => Brief::Review {
                prompt: prompt.clone(),
                repo: repo.clone(),
            },
            Brief::Fail(why) => Brief::Fail(why.clone()),
        })
    };
    let fresh_brief = &fresh_brief;
    let judge_name = cfg.binding_name(Role::Judge, None)?;
    let judge = cfg.binding(Role::Judge, None)?;
    let main_path = run.reading().to_path_buf();
    let main: &Path = &main_path;
    let (warm_reviewers, warm_judges, stop) = (&warm_reviewers, &warm_judges, &stop);
    let (kept_alive, judge_prompts) = (&kept_alive, &judge_prompts);
    // One Judge's call, after its fan-out's first has warmed the cache.
    // Relayed if Judges' calls relay: its ratings are every session's,
    // each once.
    // A Judge's ratings from every session its call took, each once: its
    // last answer put in form if it can't be read as it is.
    let ratings_read = |legs: &Legs, asked: &Call| -> Result<Vec<roles::Rating>> {
        let mut lists = Vec::new();
        for text in &legs.handed {
            lists.push(roles::read_ratings(text).map_err(unreadable)?);
        }
        let last = Call {
            role: asked.role,
            binding_name: asked.binding_name,
            binding: asked.binding,
            prompt: asked.prompt,
            artifact: asked.artifact,
            writes: asked.writes,
            no_tools: asked.no_tools,
            relay_tokens: asked.relay_tokens,
            repo: asked.repo,
            label: legs.label.clone(),
        };
        lists.push(ratings_repaired(caller, cfg, &legs.last, &last, repairs)?);
        Ok(merged(lists, |r: &roles::Rating| {
            (r.title.clone(), r.file.clone(), r.line)
        }))
    };
    let ratings_read = &ratings_read;
    let judge_call = |prompt: &str,
                      artifact: &Path,
                      label: String|
     -> Result<(Vec<Usage>, Vec<roles::Rating>)> {
        let first = warm_judges.enter(&label, caller, "Judges");
        let asked = format!("{prompt}{}", roles::RATINGS_REMINDER);
        let call = Call {
            role: Role::Judge,
            binding_name: judge_name,
            binding: judge,
            prompt: &asked,
            artifact: Some(artifact),
            writes: false,
            no_tools: false,
            relay_tokens: cfg.sessions.relay(Role::Judge),
            repo: main,
            label: label.clone(),
        };
        let used = |c: &Call, a: &Answer| -> Usage {
            let mut u = usage(cfg, c, input_bytes(c.prompt, Some(artifact)), a);
            u.fan_out = Some("judges".into());
            u.first = first && c.label == label;
            u
        };
        let legs = relayed(caller, &call, &used);
        warm_judges.done(&label);
        let legs = legs?;
        lock_vec(judge_prompts).push(prompt.to_string());
        match ratings_read(&legs, &call) {
            Ok(ratings) => Ok((legs.usage, ratings)),
            Err(e) => {
                spent(legs.usage);
                Err(e)
            }
        }
    };
    let judge_call = &judge_call;
    // The savings: kept rulings a later round's same claim takes, and a
    // first-pass Judge. Neither takes anything from the Judge: what either
    // can't settle gets the Judge's call it would have had.
    let changed_now = stages::changed_lines(&change.diff);
    let past: Vec<PastRuling> = if cfg.savings.reuse_rulings {
        run.rulings.clone()
    } else {
        Vec::new()
    };
    let rules = if past.is_empty() {
        String::new()
    } else {
        rules_source()
    };
    let contexts: BTreeMap<String, String> = open
        .iter()
        .filter_map(|id| lens_of(cfg, id).ok())
        .map(|l| {
            (
                l.id.clone(),
                ruling_context(cfg, l, &s.not_findings, &rules),
            )
        })
        .collect();
    let first_pass = cfg.first_pass_judge()?;
    let (past, contexts, changed_now) = (&past, &contexts, &changed_now);
    // A first pass, if one is set, then the Judge's binding for whatever it
    // sends on. The Judge rules from the very prompt it would have had,
    // never seeing the first pass's ruling, so it isn't anchored by it.
    let judge_fully = |lens: &Lens,
                       prompt: &str,
                       artifact: &Path,
                       label: String,
                       findings: usize|
     -> Result<(Vec<Usage>, Vec<roles::Rating>)> {
        let mut usages = Vec::new();
        if let Some((name, binding)) = first_pass {
            let first_prompt = format!(
                "{prompt}{}{}",
                roles::FIRST_PASS_NOTE,
                roles::RATINGS_REMINDER
            );
            // One session of the first pass: relayed, as the Judge's own
            // calls are. One that fails, can't be read once put in form, or
            // rates nothing is restarted once, then the Judge rules.
            let first_once = |at: &str| -> Result<(Vec<Usage>, Vec<roles::Rating>)> {
                let call = Call {
                    role: Role::Judge,
                    binding_name: name,
                    binding,
                    prompt: &first_prompt,
                    artifact: Some(artifact),
                    writes: false,
                    no_tools: false,
                    relay_tokens: cfg.sessions.relay(Role::Judge),
                    repo: main,
                    label: at.to_string(),
                };
                let used = |c: &Call, a: &Answer| -> Usage {
                    let mut spent = usage(cfg, c, input_bytes(c.prompt, Some(artifact)), a);
                    spent.fan_out = Some("first-pass Judges".into());
                    spent
                };
                let legs = relayed(caller, &call, &used)?;
                let ratings = ratings_read(&legs, &call).and_then(|ratings| {
                    if ratings.is_empty() {
                        Err(no_work(
                            caller,
                            at,
                            &format!(
                                "it returned no ratings for the {findings} finding(s) it was \
                                 given"
                            ),
                        ))
                    } else {
                        Ok(ratings)
                    }
                });
                match ratings {
                    Ok(ratings) => Ok((legs.usage, ratings)),
                    Err(e) => {
                        spent(legs.usage);
                        Err(e)
                    }
                }
            };
            let first_label = format!("{label}, first pass");
            match restarter.once(&first_label, first_once(&first_label), first_once) {
                Ok((mut spent_now, mut ratings)) => {
                    restarter.mark(&mut spent_now);
                    usages.append(&mut spent_now);
                    // A finding it left unrated isn't settled: the Judge
                    // rules on every one, rather than one being lost.
                    let unrated = findings.saturating_sub(ratings.len());
                    if unrated > 0 {
                        caller.say(&format!(
                            "{label}: the first pass rated {} of the {findings} finding(s) it \
                             was given, so the Judge rules on them all",
                            ratings.len()
                        ));
                    } else if !ratings
                        .iter()
                        .any(|r| first_pass_escalates(cfg, lens, r, changed_now))
                    {
                        for r in &mut ratings {
                            r.ruled_by = Some(name.to_string());
                        }
                        caller.say(&format!(
                            "{label}: the first pass ({name}) settled it, so no Judge call"
                        ));
                        return Ok((usages, ratings));
                    } else {
                        caller.say(&format!(
                            "{label}: the first pass sends it on to the Judge, which rules on \
                             it fresh"
                        ));
                    }
                }
                // An outage leaves the lens's ruling for the review's
                // continuing.
                Err(e) if is_outage(&e) => return Err(e),
                Err(e) => caller.say(&format!(
                    "{label}: the first pass failed ({e:#}), so the Judge rules"
                )),
            }
        }
        // The Judge: one that fails, can't be read once put in form, or
        // rates nothing is restarted once, fresh.
        let judge_once = |at: &str| -> Result<(Vec<Usage>, Vec<roles::Rating>)> {
            let (used, ratings) = judge_call(prompt, artifact, at.to_string())?;
            if ratings.is_empty() {
                spent(used);
                return Err(no_work(
                    caller,
                    at,
                    &format!("it returned no ratings for the {findings} finding(s) it was given"),
                ));
            }
            Ok((used, ratings))
        };
        let (mut spent_now, mut ratings) =
            restarter.once(&label, judge_once(&label), judge_once)?;
        restarter.mark(&mut spent_now);
        usages.append(&mut spent_now);
        if first_pass.is_some() {
            for r in &mut ratings {
                r.ruled_by = Some(judge_name.to_string());
            }
        }
        Ok((usages, ratings))
    };
    let judge_fully = &judge_fully;
    // Each lens's Judge starts once its own reviewers are done, not the
    // slowest lens's: it sees only its own lens. Per finding, each finding
    // gets a Judge of its own, given only its claim and the lines it cites.
    let rule_now = |lens: &Lens, results: &[Result<Done>]| -> Judged {
        let lists: Vec<Vec<Raw>> = results
            .iter()
            .filter_map(|r| r.as_ref().ok().map(|(_, found, _)| found.clone()))
            .collect();
        if !lists.iter().any(|l| !l.is_empty()) {
            return None;
        }
        // A finding that makes a kept ruling's claim, at its place, in a
        // file no fix has changed, takes that ruling: one for each.
        let context = contexts.get(&lens.id).map_or("", String::as_str);
        let mut reused: BTreeMap<usize, roles::Rating> = BTreeMap::new();
        let lists: Vec<Vec<Raw>> = lists
            .into_iter()
            .map(|list| {
                list.into_iter()
                    .filter(|raw| {
                        match reusable(past, cfg, lens, context, main, raw, changed_now) {
                            Some((i, p)) => {
                                reused.entry(i).or_insert_with(|| {
                                    let mut r = p.rating.clone();
                                    r.reused_from = Some(p.round);
                                    r
                                });
                                false
                            }
                            None => true,
                        }
                    })
                    .collect()
            })
            .collect();
        let reused: Vec<roles::Rating> = reused.into_values().collect();
        if !reused.is_empty() {
            let rounds: BTreeSet<u32> = reused.iter().filter_map(|r| r.reused_from).collect();
            caller.say(&format!(
                "{}: {} finding(s) took round {}'s ruling, on code no fix has changed, so no \
                 Judge call for them",
                lens.id,
                reused.len(),
                rounds
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" and ")
            ));
        }
        if !lists.iter().any(|l| !l.is_empty()) {
            return Some(Ok((Vec::new(), reused)));
        }
        let judged = (|| -> Result<(Vec<Usage>, Vec<roles::Rating>)> {
            let diff = artifacts.for_lens(lens);
            let text = fs::read_to_string(diff).ok();
            match cfg.facts.judges {
                JudgeMode::PerLens => {
                    // The hunks its findings point at, up front: a start,
                    // with the whole change attached and every tool its
                    // binding gives.
                    let hunks = text
                        .as_deref()
                        .and_then(|t| change::hunks_for(t, lists.iter().flatten()))
                        .map(|h| roles::judge_hunks(&h))
                        .unwrap_or_default();
                    let prompt = roles::judge_prompt(lens, &lists, &s.not_findings)? + &hunks;
                    let findings = lists.iter().map(Vec::len).sum();
                    judge_fully(
                        lens,
                        &prompt,
                        diff,
                        format!("{}: the Judge", lens.id),
                        findings,
                    )
                }
                JudgeMode::PerFinding => {
                    let all: Vec<&Raw> = lists.iter().flatten().collect();
                    let text = &text;
                    let out: Vec<Result<(Vec<Usage>, Vec<roles::Rating>)>> =
                        thread::scope(|scope| {
                            let handles: Vec<_> = all
                                .iter()
                                .enumerate()
                                .map(|(i, f)| {
                                    scope.spawn(move || {
                                        let cited = cited_lines(main, f, text.as_deref());
                                        let prompt = roles::join(&roles::judge_finding_parts(
                                            lens,
                                            f,
                                            &cited,
                                            &s.not_findings,
                                        ));
                                        judge_fully(
                                            lens,
                                            &prompt,
                                            diff,
                                            format!("{}: the Judge, finding {}", lens.id, i + 1),
                                            1,
                                        )
                                    })
                                })
                                .collect();
                            handles
                                .into_iter()
                                .map(|h| {
                                    h.join()
                                        .unwrap_or_else(|_| Err(anyhow!("a Judge's thread died")))
                                })
                                .collect()
                        });
                    let mut usages = Vec::new();
                    let mut ratings = Vec::new();
                    for r in out {
                        let (mut us, mut rs) = r?;
                        usages.append(&mut us);
                        ratings.append(&mut rs);
                    }
                    Ok((usages, ratings))
                }
            }
        })();
        Some(judged.map(|(usages, mut ratings)| {
            ratings.extend(reused);
            (usages, ratings)
        }))
    };
    let rule = |lens: &Lens, results: &[Result<Done>]| -> Judged {
        // Kept from before a pause, it isn't asked again.
        if let Some(kept) = units.judge(&lens.id) {
            return Some(Ok(kept));
        }
        // A reviewer an outage left unfinished leaves the lens's Judge for
        // the review's continuing.
        if let Some(e) = results
            .iter()
            .find_map(|r| r.as_ref().err().filter(|e| is_outage(e)))
        {
            return Some(Err(anyhow::Error::new(Outage(format!("{e:#}")))));
        }
        let judged = rule_now(lens, results);
        if let Some(Ok((usage, ratings))) = &judged {
            units.keep(&UnitDone::Judge {
                lens: lens.id.clone(),
                usage: usage.clone(),
                ratings: ratings.clone(),
            });
        }
        judged
    };
    let mut groups: Vec<(&Lens, Vec<(&Brief, &String)>)> = Vec::new();
    for (lens, brief, label) in &asks {
        match groups.iter_mut().find(|(l, _)| l.id == lens.id) {
            Some((_, slots)) => slots.push((brief, label)),
            None => groups.push((lens, vec![(brief, label)])),
        }
    }
    let board = Board::new(
        &s.work_dir,
        run.round,
        &groups
            .iter()
            .map(|(lens, slots)| (*lens, slots.len()))
            .collect::<Vec<_>>(),
    );
    // What a keep-alive refreshes: each role's prefix, through the binding
    // its calls use.
    let first_lens = groups.first().map(|(lens, _)| *lens);
    let alive: Vec<Alive> = match first_lens {
        Some(lens) if cfg.facts.shared && cfg.facts.keep_alive_secs > 0 && head > 0 => vec![
            Alive {
                warm: warm_reviewers,
                role: Role::Reviewer,
                name: cfg.binding_name(Role::Reviewer, Some(lens))?,
                binding: cfg.binding(Role::Reviewer, Some(lens))?,
                what: "reviewers",
                till_done: true,
            },
            Alive {
                warm: warm_judges,
                role: Role::Judge,
                name: judge_name,
                binding: judge,
                what: "Judges",
                till_done: false,
            },
        ],
        _ => Vec::new(),
    };
    let (review, rule, board, alive) = (&review, &rule, &board, &alive);
    let main_repo = run.reading().to_path_buf();
    let main_repo = &main_repo;
    // Every lens and every reviewer at once, once the first has warmed the
    // cache, and each lens's Judge as soon as its own reviewers are done:
    // nothing here limits how many run, so only a bridge's max_concurrent
    // does, and progress shows a call waiting on it.
    let done: Vec<(Vec<Result<Done>>, Judged)> = thread::scope(|scope| {
        scope.spawn(move || warm_reviewers.watch(caller, stop, "reviewers"));
        scope.spawn(move || warm_judges.watch(caller, stop, "Judges"));
        if !alive.is_empty() {
            scope.spawn(move || keep_alive(caller, cfg, alive, main_repo, stop, kept_alive));
        }
        let handles: Vec<_> = groups
            .iter()
            .map(|(lens, slots)| {
                scope.spawn(move || {
                    let reviewers: Vec<_> = slots
                        .iter()
                        .map(|(brief, label)| {
                            scope.spawn(move || {
                                // Kept from before a pause, it isn't asked again.
                                if let Some(kept) = units.reviewer(label) {
                                    board.answered(&lens.id);
                                    return kept;
                                }
                                let first = warm_reviewers.enter(label, caller, "reviewers");
                                let result = review(lens, brief, label, first);
                                warm_reviewers.done(label);
                                let result = restarter
                                    .once(label, result, |again| {
                                        let fresh = fresh_brief(brief)?;
                                        let again = again.to_string();
                                        warm_reviewers.enter(&again, caller, "reviewers");
                                        let result = review(lens, &fresh, &again, false);
                                        warm_reviewers.done(&again);
                                        result
                                    })
                                    .map(|mut done| {
                                        restarter.mark(&mut done.0);
                                        done
                                    });
                                match &result {
                                    Ok((usage, found, note)) => units.keep(&UnitDone::Reviewer {
                                        label: label.to_string(),
                                        usage: usage.clone(),
                                        found: found.clone(),
                                        note: note.clone(),
                                    }),
                                    Err(e) if !is_outage(e) => {
                                        units.keep(&UnitDone::ReviewerFailed {
                                            label: label.to_string(),
                                            why: format!("{e:#}"),
                                        })
                                    }
                                    Err(_) => {}
                                }
                                board.answered(&lens.id);
                                result
                            })
                        })
                        .collect();
                    let results: Vec<Result<Done>> = reviewers
                        .into_iter()
                        .map(|h| {
                            h.join()
                                .unwrap_or_else(|_| Err(anyhow!("a reviewer's thread died")))
                        })
                        .collect();
                    // A lens with nothing to rate has no Judge: said, so it
                    // doesn't look like one that never started.
                    let found = results
                        .iter()
                        .any(|r| r.as_ref().is_ok_and(|(_, found, _)| !found.is_empty()));
                    if found {
                        board.set(&lens.id, "judging");
                    } else if results.iter().any(Result::is_ok) {
                        board.set(&lens.id, "no findings");
                        caller.say(&format!("{}: no findings, nothing for the Judge", lens.id));
                    } else {
                        board.set(&lens.id, "no usable answer");
                        caller.say(&format!(
                            "{}: no reviewer's answer could be used, so nothing for the Judge",
                            lens.id
                        ));
                    }
                    let judged = rule(lens, &results);
                    match &judged {
                        Some(Ok(_)) => board.set(&lens.id, "judged"),
                        Some(Err(_)) => board.set(&lens.id, "judge failed"),
                        None => {}
                    }
                    (results, judged)
                })
            })
            .collect();
        let done = handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    (
                        vec![Err(anyhow!("a lens's thread died"))],
                        Some(Err(anyhow!("a lens's thread died"))),
                    )
                })
            })
            .collect();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        done
    });
    for mut u in std::mem::take(&mut *lock_vec(kept_alive)) {
        u.round = run.round;
        run.usage.push(u);
    }
    for mut u in std::mem::take(&mut *lock_vec(repairs)) {
        u.round = run.round;
        run.usage.push(u);
    }
    let mut wasted = std::mem::take(&mut *lock_vec(unused));
    restarter.mark(&mut wasted);
    for mut u in wasted {
        u.round = run.round;
        run.usage.push(u);
    }
    // An outage left units unfinished: the round pauses, keeping what its
    // units finished, for `acr resume` to go on from. What they used is
    // counted when it does.
    let paused = done.iter().find_map(|(results, judged)| {
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .chain(judged.as_ref().and_then(|j| j.as_ref().err()))
            .find(|e| is_outage(e))
            .map(|e| format!("{e:#}"))
    });
    if let Some(why) = paused {
        let why = watch.declared().unwrap_or(why);
        caller.note(&format!(
            "round {}: paused, since its calls can't reach their model: {why}",
            run.round
        ));
        run.outage = Some(Paused {
            round: run.round,
            why: why.clone(),
        });
        return Err(anyhow::Error::new(Outage(why)));
    }
    run.outage = None;
    let prompts = std::mem::take(&mut *lock_vec(judge_prompts));
    if head > 0 && prompts.len() > 1 {
        let texts: Vec<&str> = prompts.iter().map(String::as_str).collect();
        let p = super::facts::Prefix::new(
            run.round,
            "Judges",
            head + super::facts::common_prefix(&texts),
        );
        caller.say(&format!("round {}: {}", run.round, p.line()));
        run.prefixes.push(p);
    }
    let mut by_lens: BTreeMap<String, Vec<Result<Vec<Raw>>>> = BTreeMap::new();
    let mut judgments: BTreeMap<String, Judged> = BTreeMap::new();
    for ((lens, _), (results, judged)) in groups.iter().zip(done) {
        for answer in results {
            let result = answer.map(|(usage, findings, note)| {
                for mut usage in usage {
                    usage.round = run.round;
                    run.usage.push(usage);
                }
                if let Some(note) = note {
                    notes.entry(lens.id.clone()).or_default().push(note);
                }
                findings
            });
            by_lens.entry(lens.id.clone()).or_default().push(result);
        }
        judgments.insert(lens.id.clone(), judged);
    }
    // Once the round's calls are all in, so each repair is set against them.
    warn_repairs(run, caller);
    say_restarts(run, caller);
    learn_round(run, s);
    let _ = units.going_on();
    Ok((by_lens, judgments, notes))
}

/// The round's restarts, and what they cost, in a line apart from its
/// repairs.
fn say_restarts(run: &Run, caller: &dyn Caller) {
    if let Some(line) = super::report::restarts(&run.usage)
        .iter()
        .find(|r| r.round == run.round)
        .map(super::report::Restarts::line)
    {
        caller.say(&line);
    }
}

/// A prober's kept probes as its round found them.
fn probes_at_start(kept: &Path) -> PathBuf {
    kept.with_extension("start.diff")
}

/// Where a restarted unit's fresh copy goes: beside the first's, as
/// `<slot>-restarted/<name>` for a copy at `<slot>/<name>`.
fn restarted_copy(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_os_string();
    match path.parent().filter(|p| p.file_name().is_some()) {
        Some(slot) => {
            let slot_name = slot.file_name().unwrap_or_default().to_string_lossy();
            slot.with_file_name(format!("{slot_name}-restarted"))
                .join(name)
        }
        None => path.with_file_name(format!("{}-restarted", name.to_string_lossy())),
    }
}

fn lock_vec<T>(m: &std::sync::Mutex<Vec<T>>) -> std::sync::MutexGuard<'_, Vec<T>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The lines a finding cites, for its own Judge to start from: around its
/// line in the checkout, then the diff's hunks near it. Nothing of the
/// reviewer's reasoning.
fn cited_lines(repo: &Path, f: &Raw, diff: Option<&str>) -> String {
    let mut out = String::new();
    if let (Some(file), Some(line)) = (f.file.as_deref(), f.line)
        && let Ok(text) = fs::read_to_string(repo.join(file))
    {
        let lines: Vec<&str> = text.lines().collect();
        let at = line.max(1) as usize;
        let from = at.saturating_sub(6).max(1);
        let to = (at + 6).min(lines.len());
        for n in from..=to {
            if let Some(l) = lines.get(n - 1) {
                out.push_str(&format!("{file}:{n}: {l}\n"));
            }
        }
    }
    if let Some(h) = diff.and_then(|t| change::hunks_for(t, std::iter::once(f))) {
        out.push_str(&h);
    }
    out
}

/// A role's fan-out, for the prompt cache: its first call goes alone until
/// the cache holds the prefix the rest share, then the rest go. With no
/// time to wait, they all go at once.
struct CacheWarm {
    secs: u64,
    /// The gap between the other calls' starts once the way opens, when
    /// the first went alone: `facts.stagger_secs`.
    stagger: std::time::Duration,
    state: std::sync::Mutex<WarmState>,
    changed: std::sync::Condvar,
}

#[derive(Default)]
struct WarmState {
    first: Option<String>,
    started: Option<std::time::Instant>,
    open: bool,
    /// Why the rest were let go, once they were, to say.
    why: Option<&'static str>,
    running: usize,
    /// The first went alone, so the rest start a stagger apart.
    staggers: bool,
    /// When the next of the rest may start.
    next: Option<std::time::Instant>,
}

impl CacheWarm {
    fn new(secs: u64, stagger_secs: u64) -> CacheWarm {
        CacheWarm {
            secs,
            stagger: std::time::Duration::from_secs(stagger_secs),
            state: std::sync::Mutex::new(WarmState {
                open: secs == 0,
                ..WarmState::default()
            }),
            changed: std::sync::Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WarmState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Waits a call's turn. True for its fan-out's first, which goes at once.
    fn enter(&self, label: &str, caller: &dyn Caller, what: &str) -> bool {
        let mut st = self.lock();
        st.running += 1;
        if st.first.is_none() {
            st.first = Some(label.to_string());
            st.started = Some(std::time::Instant::now());
            // A caller that can't tell when the cache is warm doesn't wait.
            if caller.warmed(label).is_none() {
                st.open = true;
            }
            if !st.open {
                st.staggers = !self.stagger.is_zero();
                caller.say(&format!(
                    "warming the cache: {label} goes first, and the other {what} wait up to {}s \
                     for it to fill the cache{}",
                    self.secs,
                    if st.staggers {
                        format!(", then start {}s apart", self.stagger.as_secs())
                    } else {
                        String::new()
                    }
                ));
            }
            self.changed.notify_all();
            return true;
        }
        while !st.open {
            st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        // A burst of new sessions can land on replicas that don't hold the
        // prefix: once the first has warmed one, the rest go a little apart.
        if st.staggers {
            let now = std::time::Instant::now();
            let at = st.next.map_or(now, |next| next.max(now));
            st.next = Some(at + self.stagger);
            drop(st);
            thread::sleep(at.saturating_duration_since(now));
        }
        false
    }

    /// A call is done. The first one's end opens the way for the rest, if
    /// nothing has yet.
    fn done(&self, label: &str) {
        let mut st = self.lock();
        st.running = st.running.saturating_sub(1);
        if st.first.as_deref() == Some(label) && !st.open {
            st.open = true;
            st.why = Some("the cache is warm (its first call is done)");
            self.changed.notify_all();
        }
    }

    /// When its first call started, if one has.
    fn started(&self) -> Option<std::time::Instant> {
        self.lock().started
    }

    fn running(&self) -> usize {
        self.lock().running
    }

    /// Opens the way once the first call has filled the cache, or its time
    /// is up, or the round ends.
    fn watch(&self, caller: &dyn Caller, stop: &std::sync::atomic::AtomicBool, what: &str) {
        let said = |why: &str, t0: Option<std::time::Instant>| {
            let secs = t0.map_or(0, |t| t.elapsed().as_secs());
            let apart = if self.lock().staggers {
                format!(", {}s apart", self.stagger.as_secs())
            } else {
                String::new()
            };
            caller.say(&format!(
                "{why} after {secs}s: the other {what} start{apart}"
            ));
        };
        loop {
            let (first, started, open, why) = {
                let st = self.lock();
                (st.first.clone(), st.started, st.open, st.why)
            };
            if open {
                if let Some(why) = why {
                    said(why, started);
                }
                return;
            }
            if let (Some(label), Some(t0)) = (first, started) {
                let warm = caller.warmed(&label) == Some(true);
                let late = t0.elapsed().as_secs() >= self.secs;
                if warm || late {
                    let mut st = self.lock();
                    if !st.open {
                        st.open = true;
                        st.why = Some(if warm {
                            "the cache is warm (its first call has read its prompt)"
                        } else {
                            "the cache isn't warm yet, but the wait is up"
                        });
                        self.changed.notify_all();
                    }
                    let why = st.why;
                    drop(st);
                    if let Some(why) = why {
                        said(why, Some(t0));
                    }
                    return;
                }
            }
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                let mut st = self.lock();
                st.open = true;
                self.changed.notify_all();
                return;
            }
            thread::sleep(std::time::Duration::from_millis(500));
        }
    }
}

/// A role's prefix a keep-alive refreshes.
struct Alive<'a> {
    warm: &'a CacheWarm,
    role: Role,
    name: &'a str,
    binding: &'a Binding,
    what: &'static str,
    /// Live only while its calls run, as reviewers', all started at once;
    /// otherwise from its first call to the round's end, as Judges', which
    /// start apart.
    till_done: bool,
}

/// Refreshes each live role's cached prefix before the provider could evict
/// it: a minimal call with the same prefix, through the same binding, only
/// when its bridge has a slot to spare. Its use is kept apart from the
/// roles' work.
///
/// Each role's refreshes are its own: one after another, never two at once,
/// and the next due an interval after the last one answered, not after it
/// went. A slow provider's answer is then never followed at once by another,
/// and one role's slow refresh never holds up another role's.
fn keep_alive(
    caller: &dyn Caller,
    cfg: &Config,
    alive: &[Alive],
    repo: &Path,
    stop: &std::sync::atomic::AtomicBool,
    out: &std::sync::Mutex<Vec<Usage>>,
) {
    let every = std::time::Duration::from_secs(cfg.facts.keep_alive_secs);
    thread::scope(|scope| {
        for a in alive {
            scope.spawn(move || keep_one_alive(caller, cfg, a, every, repo, stop, out));
        }
    });
}

/// One role's refreshes, for [`keep_alive`].
fn keep_one_alive(
    caller: &dyn Caller,
    cfg: &Config,
    a: &Alive,
    every: std::time::Duration,
    repo: &Path,
    stop: &std::sync::atomic::AtomicBool,
    out: &std::sync::Mutex<Vec<Usage>>,
) {
    // When the last refresh answered: the next is due an interval after.
    let mut answered: Option<std::time::Instant> = None;
    let mut said_slow = false;
    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        let due = a.warm.started().and_then(|since| {
            let live = !(a.till_done && a.warm.running() == 0);
            live.then(|| answered.unwrap_or(since) + every)
        });
        if let Some(due) = due
            && std::time::Instant::now() >= due
            && caller.has_room(a.binding)
        {
            let call = Call {
                role: a.role,
                binding_name: a.name,
                binding: a.binding,
                prompt: roles::KEEP_ALIVE,
                artifact: None,
                writes: false,
                no_tools: false,
                relay_tokens: None,
                repo,
                label: format!("cache keep-alive: {}", a.what),
            };
            let went = std::time::Instant::now();
            let result = caller.call(&call);
            answered = Some(std::time::Instant::now());
            let took = went.elapsed();
            if let Ok(answer) = result {
                let mut u = usage(cfg, &call, roles::KEEP_ALIVE.len(), &answer);
                u.fan_out = Some("keep-alive".into());
                lock_vec(out).push(u);
                caller.say(&format!("cache: refreshed the {}' prefix", a.what));
            }
            if took >= every && !said_slow {
                said_slow = true;
                caller.say(&format!(
                    "cache: the {}' keep-alive took {}s to answer, past its {}s interval: the \
                     next waits its interval after the answer, and none goes while one is out",
                    a.what,
                    took.as_secs(),
                    every.as_secs()
                ));
            }
        }
        for _ in 0..10 {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// The round's warm copy, and how its setup went.
struct Warm {
    /// The copy every other copy this round is made from. None if there's
    /// no setup, or it failed, so copies start cold.
    copy: Option<PathBuf>,
    /// For the round's record: why copies started cold.
    note: Option<String>,
    /// For progress: how the setup went.
    said: String,
    /// For every reviewer's brief, when the setup failed.
    brief: Option<String>,
}

/// The round's warm copy: a copy of the change with the repository's setup
/// run in it once, for every other copy this round to be made from. Its
/// `warm.log` says what ran, how it ended and how long it took.
fn warm_copy(run: &Run, s: &Setting, dir: Option<&Path>) -> Warm {
    let cold = |note: Option<String>, said: String, brief: Option<String>| Warm {
        copy: None,
        note,
        said,
        brief,
    };
    let Some(setup) = s.cfg.tests.setup.as_deref() else {
        return cold(None, "setup: none configured".into(), None);
    };
    let Some(dir) = dir else {
        return cold(
            None,
            "setup: not run, since there's no place for copies".into(),
            None,
        );
    };
    let copy = dir
        .join("warm")
        .join(run.repo.file_name().unwrap_or_default());
    let limit = std::time::Duration::from_secs(s.cfg.tests.timeout.unwrap_or(900));
    let log = dir.join("warm.log");
    let failed_brief = |how: &str, tail: &str| {
        let tail = if tail.trim().is_empty() {
            "It printed nothing.".to_string()
        } else {
            format!("Its output ended:\n\n```\n{}\n```", tail.trim_end())
        };
        format!(
            "# The repository's setup failed\n\nBefore your copy was made, its setup command \
             (`{setup}`) {how}, so what it installs or builds may be missing from your copy. \
             Install or build what you need yourself, and say so in your answer if you can't \
             run the tests. {tail}\n\n"
        )
    };
    let ran = super::change::copy_of(run.reading(), &copy)
        .and_then(|_| stages::run_setup(&copy, setup, limit, &log));
    match ran {
        Ok(r) if r.passed == Some(true) => Warm {
            copy: Some(copy),
            note: None,
            said: format!("setup: ok in {}s", r.secs),
            brief: None,
        },
        Ok(r) if r.passed == Some(false) => {
            let exit = r
                .code
                .map_or("killed by a signal".to_string(), |c| format!("exit {c}"));
            cold(
                Some("the setup command failed, so copies started cold".into()),
                format!(
                    "setup: failed, {exit}, in {}s: see {}",
                    r.secs,
                    log.display()
                ),
                Some(failed_brief(&format!("failed ({exit})"), &r.tail)),
            )
        }
        Ok(r) => cold(
            Some(format!(
                "the setup command ran past {} s, so copies started cold",
                limit.as_secs()
            )),
            format!(
                "setup: stopped after {}s, its limit: see {}",
                r.secs,
                log.display()
            ),
            Some(failed_brief(
                &format!(
                    "ran past its limit of {} s and was stopped",
                    limit.as_secs()
                ),
                &r.tail,
            )),
        ),
        Err(e) => cold(
            Some(format!("no warm copy, so copies started cold: {e:#}")),
            format!("setup: not run: {e:#}"),
            Some(failed_brief(&format!("couldn't run ({e:#})"), "")),
        ),
    }
}

/// What the test gate found this round, for the reviewers who follow it:
/// what it ran, and each gap with how it was rated.
fn gate_report(round: &Round) -> String {
    // Said only of what it ran: with no coverage command, it measured none.
    let coverage = round.notes.iter().any(|n| n.contains("coverage:"));
    let mut out = format!(
        "# What the test gate found this round\n\nIt ran the change's tests{}, against mutants \
         of its code, and other ways: don't repeat those runs.\n",
        if coverage { " with coverage" } else { "" }
    );
    for note in &round.notes {
        out.push_str(&format!("- {note}\n"));
    }
    for f in &round.findings {
        out.push_str(&format!("- {:?}: {}\n", f.severity, f.title));
    }
    out.push('\n');
    out
}

/// The test gate's round for its lens/// The test gate's round for its lens/// The test gate's round for its lens: mutants proposed by a reviewer and
/// run with coverage against the tests, and the gaps judged.
fn gate_round(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    artifacts: &Artifacts,
    change: &Change,
    id: &str,
    copying: Copying,
) -> Result<Round> {
    let Copying { dir, source } = copying;
    let cfg = s.cfg;
    let lens = lens_of(cfg, id)?;
    let mut round = Round {
        round: run.round,
        ..Round::default()
    };
    let Some(root) = dir else {
        round
            .failures
            .push("the test gate needs a place for copies".into());
        return Ok(round);
    };
    let Some(command) = cfg
        .tests
        .command
        .clone()
        .or_else(|| stages::test_command(run.reading()))
    else {
        round.failures.push(
            "the test gate can't tell how this repository's tests run: set tests.command in \
             its review config"
                .into(),
        );
        return Ok(round);
    };
    let scope = match run.head.as_deref() {
        Some(head) => format!("the change from {} to {head}", run.base),
        None => format!("the working tree against {}", run.base),
    };
    let dir = Copies::fresh(Some(root.join("gate")));
    let guide = stages::guide(lens.attack.as_deref(), None, None);
    let prompt = stages::mutants_prompt(&scope, cfg.tests.mutants.unwrap_or(12), &guide);
    let proposed = ask_in_form(
        run,
        s,
        caller,
        Role::Reviewer,
        &format!("{}: proposing mutants", lens.id),
        &prompt,
        Some(&artifacts.diff),
        (
            roles::MUTANTS_REMINDER,
            roles::Shape::Mutants,
            stages::read_mutants,
        ),
    );
    let mutants = match proposed {
        Ok(m) => m,
        Err(e) => {
            round.failures.push(format!("its mutants: {e:#}"));
            Vec::new()
        }
    };
    let runs = stages::GateRuns {
        runner: stages::Runner {
            repo: source,
            dir: dir.0.as_deref().expect("set above"),
            command: &command,
            parallel: cfg.tests.parallel.unwrap_or(2) as usize,
            limit: std::time::Duration::from_secs(cfg.tests.timeout.unwrap_or(900)),
        },
        coverage: cfg.tests.coverage.as_deref(),
        report: cfg.tests.report.as_deref().unwrap_or("coverage/lcov.info"),
        diff: &change.diff,
        variants: &cfg.tests.variants,
        repeat: cfg.tests.repeat.unwrap_or(0),
    };
    let gaps = match stages::run_gate(&runs, &mutants) {
        Ok((gaps, summary)) => {
            round.notes.push(summary);
            gaps
        }
        Err(e) => {
            let e = idle_said(caller, &format!("{id}: the test gate"), e);
            round.failures.push(format!("{e:#}"));
            Vec::new()
        }
    };
    drop(dir);
    round.reviewers.push(format!(
        "The test gate, {id}: coverage and mutants run here"
    ));
    if !gaps.is_empty() {
        // Tagged, so a fact the gate measured follows its finding through
        // the Judge.
        let tagged: Vec<Raw> = gaps
            .iter()
            .enumerate()
            .map(|(i, g)| Raw {
                title: format!("[G{i}] {}", g.raw.title),
                ..g.raw.clone()
            })
            .collect();
        let judged = roles::judge_prompt(lens, &[tagged], &s.not_findings).and_then(|prompt| {
            let prompt = prompt
                + "\n\nKeep each finding's title as it's given, with its [G..] tag, and don't \
                   merge them.";
            ask_in_form(
                run,
                s,
                caller,
                Role::Judge,
                &format!("{}: the Judge", lens.id),
                &prompt,
                Some(artifacts.for_lens(lens)),
                (
                    roles::RATINGS_REMINDER,
                    roles::Shape::Ratings,
                    roles::read_ratings,
                ),
            )
        });
        match judged {
            Ok(ratings) => {
                let tag = regex::Regex::new(r"^\[G(\d+)\]").expect("a fixed pattern");
                let mut findings = rate(
                    run,
                    cfg,
                    lens,
                    ratings,
                    &stages::changed_lines(&change.diff),
                );
                for f in &mut findings {
                    let measured = tag
                        .captures(&f.title)
                        .and_then(|c| c[1].parse::<usize>().ok())
                        .and_then(|i| gaps.get(i))
                        .is_some_and(|g| g.blocks);
                    gate_ruling(cfg, lens, f, measured);
                }
                round.findings = findings;
            }
            Err(e) => round.failures.push(format!("the Judge: {e:#}")),
        }
    }
    Ok(round)
}

/// The test-writing round between rounds: a fresh writer pins the blockers
/// just fixed, and each test it names is run on the code from before the
/// fixes, where it must fail, and after, where it must pass. A test that
/// passes before pins nothing, and a person hears so.
fn write_tests(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    blockers: &str,
    intent: &str,
    before: Option<&Path>,
) -> (Vec<Pin>, String) {
    let cfg = s.cfg;
    let prompt = stages::writer_prompt(intent, blockers, cfg.runbook.as_deref());
    let written = (|| -> Result<stages::Written> {
        let name = cfg.binding_name(Role::Orchestrator, None)?;
        let repo = run.repo.clone();
        let call = Call {
            role: Role::Orchestrator,
            binding_name: name,
            binding: cfg.binding(Role::Orchestrator, None)?,
            prompt: &prompt,
            artifact: None,
            writes: true,
            no_tools: false,
            relay_tokens: None,
            repo: &repo,
            label: "the test writer".to_string(),
        };
        let answer = caller.call(&call)?;
        record(run, cfg, &call, prompt.len(), &answer);
        stages::read_written(&answer.text)
    })();
    let written = match written {
        Ok(w) => w,
        Err(e) => {
            escalate(
                run,
                format!("the test writer after round {}: {e:#}", run.round),
            );
            return (Vec::new(), String::new());
        }
    };
    let command = cfg
        .tests
        .command
        .clone()
        .or_else(|| stages::test_command(&run.repo));
    let limit = std::time::Duration::from_secs(cfg.tests.timeout.unwrap_or(900));
    // The code after the fixes and the new tests, in a copy, so running the
    // tests leaves nothing in the checkout.
    let after = Copies::fresh(
        s.copies
            .as_ref()
            .map(|root| root.join(format!("{}-r{}-after", run.id, run.round))),
    );
    let after_copy = after.0.as_ref().and_then(|dir| {
        let copy = dir.join(run.repo.file_name().unwrap_or_default());
        super::change::copy_of(&run.repo, &copy).ok().map(|_| copy)
    });
    let mut pins = Vec::new();
    for t in &written.tests {
        let mut pin = Pin {
            file: t.file.clone(),
            pins: t.pins.clone(),
            fails_before: None,
            passes_after: None,
        };
        if let (Some(command), true) = (&command, run.repo.join(&t.file).is_file()) {
            let files = [t.file.clone()];
            if let Some(before) = before {
                let to = before.join(&t.file);
                let from = run.repo.join(&t.file);
                // A pin in the file it tests goes in as its tests alone:
                // copied whole, the file would bring the fix with it.
                let inline = fs::read_to_string(&from)
                    .ok()
                    .zip(fs::read_to_string(&to).ok())
                    .and_then(|(after, old)| stages::with_tests_of(&t.file, &old, &after));
                let copied = to
                    .parent()
                    .map_or(Ok(()), fs::create_dir_all)
                    .and_then(|_| match &inline {
                        Some(text) => fs::write(&to, text),
                        None => fs::copy(&from, &to).map(|_| ()),
                    });
                if copied.is_ok() {
                    let log = before.with_extension("before.log");
                    pin.fails_before = stages::tests_pass(before, command, &files, limit, &log)
                        .ok()
                        .flatten()
                        .map(|passed| !passed);
                }
            }
            if let Some(after) = &after_copy {
                let log = after.with_extension("after.log");
                pin.passes_after = stages::tests_pass(after, command, &files, limit, &log)
                    .ok()
                    .flatten();
            }
        }
        if pin.fails_before == Some(false) {
            escalate(
                run,
                format!(
                    "{} pins nothing: it passes on the code from before round {}'s fixes",
                    pin.file, run.round
                ),
            );
        }
        if pin.passes_after == Some(false) {
            escalate(
                run,
                format!(
                    "{} fails on the code after round {}'s fixes",
                    pin.file, run.round
                ),
            );
        }
        pins.push(pin);
    }
    (pins, written.done)
}

/// Where a round's copies go. Each is named afresh, starting with the time
/// it's made, so a copy never lands on one an earlier round or a resumed one
/// left, since they stay, to be looked at, until the daemon's cleanup clears
/// them. They're left unpacked: each is a clone of the repository that costs
/// next to nothing on disk, and what a reviewer did in it is in its packed
/// session.
struct Copies(Option<PathBuf>);

impl Copies {
    fn fresh(dir: Option<PathBuf>) -> Copies {
        Copies(dir.map(|dir| {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            let name = format!("{name}-{}", &crate::util::new_id()[20..]);
            dir.with_file_name(crate::util::stamped(&name))
        }))
    }
}

/// A fresh place for a copy under `dir`, named for the time it's made, as
/// a round's copies are: see [`Copies`].
pub(super) fn fresh_copy_dir(dir: PathBuf) -> PathBuf {
    Copies::fresh(Some(dir)).0.expect("given a folder")
}

/// Removes copies left `cleanup::KEEP` ago, under `root`. Returns how many.
pub(super) fn prune_copies(root: &Path) -> usize {
    let mut gone = 0;
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        if crate::util::older_than(&entry.path(), crate::cleanup::KEEP)
            && fs::remove_dir_all(entry.path()).is_ok()
        {
            gone += 1;
        }
    }
    gone
}

/// Which lenses and escalations an Orchestrator's answer picks. One written
/// in prose is put in form by one more call; only an answer that's still
/// unreadable goes to a person, as no check of the change's descriptions.
fn selected_of(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    text: &str,
    described: &[(&str, &str)],
    escalating: &[String],
) -> Result<roles::Selected> {
    // An answer that leaves out the escalations it was asked about would
    // pass for "none apply", which goes to no person.
    let read = |text: &str| {
        let selected = roles::read_selected(text)?;
        if !escalating.is_empty() && selected.escalate.is_none() {
            bail!("the answer doesn't say which things must go to a person");
        }
        Ok(selected)
    };
    let unread = match read(text) {
        Ok(selected) => return Ok(selected),
        Err(e) => e,
    };
    let ids: Vec<&str> = described.iter().map(|(id, _)| *id).collect();
    let prompt = roles::reformat_selected_prompt(text, &ids, escalating);
    let what = "the Sizer, putting the lens choice in form";
    let repaired = ask(run, s, caller, Role::Sizer, what, &prompt, None);
    if repaired.is_ok() {
        mark_repair(run, s.cfg, Role::Orchestrator);
    }
    repaired.and_then(|text| read(&text)).with_context(|| {
        format!("{unread:#}, and putting its answer in the form asked didn't work either")
    })
}

/// Marks the call just recorded, the Sizer's, as a repair of an answer by
/// `of`'s binding.
fn mark_repair(run: &mut Run, cfg: &Config, of: Role) {
    let (Ok(binding), Ok(b)) = (cfg.binding_name(of, None), cfg.binding(of, None)) else {
        return;
    };
    if let Some(u) = run.usage.last_mut() {
        u.repaired = Some(Repaired {
            role: of,
            binding: binding.to_string(),
            model: b.model.clone(),
        });
    }
}

/// Says when a binding's answers keep needing the Sizer to put them in
/// form: a model that misses the format often costs a call each time, and
/// may miss what's in it too.
fn warn_repairs(run: &Run, caller: &dyn Caller) {
    for r in super::report::repairs(&run.usage) {
        if r.repairs >= REPAIR_WARN_MIN && r.share() > REPAIR_WARN_SHARE {
            caller.say(&format!(
                "round {}: {}. Its format instructions may need tightening, or its model \
                 may suit another role better",
                run.round,
                r.line()
            ));
        }
    }
}

/// The briefs in a briefer's answer. One written in prose is put in form
/// by one more call; only an answer that's still unreadable is no brief.
fn briefs_of(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    text: &str,
    lenses: &[String],
) -> Result<BTreeMap<String, String>> {
    let unread = match stages::read_briefs(text) {
        Ok(briefs) => return Ok(briefs),
        Err(e) => e,
    };
    let prompt = stages::reformat_briefs_prompt(text, lenses);
    let what = "the Sizer, putting briefs in form";
    let repaired = ask(run, s, caller, Role::Sizer, what, &prompt, None);
    if repaired.is_ok() {
        mark_repair(run, s.cfg, Role::Orchestrator);
    }
    repaired
        .and_then(|text| stages::read_briefs(&text))
        .with_context(|| {
            format!("{unread:#}, and putting its briefs in the form asked didn't work either")
        })
}

/// A reviewer's findings. One that reported in prose, not the JSON asked
/// for, has its report put in that form by a quick call with the Sizer's
/// binding, adding and dropping nothing: a review isn't thrown away over its
/// format. A report that still can't be read is a failure, as before. The
/// repair's call goes in `repairs`, marked as one, to be counted.
fn findings_of(
    caller: &dyn Caller,
    cfg: &Config,
    text: &str,
    asked: &Call,
    repairs: &std::sync::Mutex<Vec<Usage>>,
) -> Result<Vec<Raw>> {
    repaired(
        caller,
        cfg,
        text,
        asked,
        repairs,
        roles::Shape::Findings,
        roles::read_findings,
    )
}

/// A Judge's ratings, from its answer: one that can't be read is put in
/// form as a reviewer's is, then it's unreadable.
fn ratings_repaired(
    caller: &dyn Caller,
    cfg: &Config,
    text: &str,
    asked: &Call,
    repairs: &std::sync::Mutex<Vec<Usage>>,
) -> Result<Vec<roles::Rating>> {
    repaired(
        caller,
        cfg,
        text,
        asked,
        repairs,
        roles::Shape::Ratings,
        roles::read_ratings,
    )
}

/// What `read` makes of an answer, or, if it can't, of the answer put in
/// the form asked by a quick call on the Sizer's binding, told why it
/// couldn't be read. One that still can't be is unreadable.
fn repaired<T>(
    caller: &dyn Caller,
    cfg: &Config,
    text: &str,
    asked: &Call,
    repairs: &std::sync::Mutex<Vec<Usage>>,
    shape: roles::Shape,
    read: fn(&str) -> Result<T>,
) -> Result<T> {
    let unread = match read(text) {
        Ok(found) => return Ok(found),
        Err(e) => e,
    };
    let prompt = roles::reformat_prompt(text, shape, &format!("{unread:#}"));
    let call = Call {
        role: Role::Sizer,
        binding_name: cfg.binding_name(Role::Sizer, None)?,
        binding: cfg.binding(Role::Sizer, None)?,
        prompt: &prompt,
        artifact: None,
        writes: false,
        no_tools: false,
        relay_tokens: None,
        repo: asked.repo,
        label: format!("{}, put in form by the Sizer", asked.label),
    };
    let answer = caller.call(&call)?;
    let mut spent = usage(cfg, &call, prompt.len(), &answer);
    spent.repaired = Some(Repaired::of(asked));
    lock_vec(repairs).push(spent);
    read(&answer.text).map_err(|e| {
        anyhow::Error::new(Unreadable(format!(
            "{unread:#}, and putting its report in the form asked didn't work either: {e:#}"
        )))
    })
}

/// An answer that couldn't be read, even once the Sizer had put it in form
/// where it could: the session gave no answer, so its unit is restarted
/// (see `Restarter`).
#[derive(Debug)]
struct Unreadable(String);

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreadable {}

/// Turns a Judge's ratings into a lens's findings for this round.
/// What blocks, decided by fixed rules from the facts the Judge states, not
/// by how a prompt words them. Rules only raise or route: a finding no rule
/// covers keeps the Judge's severity, so a missing rule never silences one.
/// Only two lower it, both explicit:
/// - not a real defect, or a break that changes nothing: dropped;
/// - there before the change, which the change neither claims to fix nor
///   makes worse: logged, never blocking. A Critical one also goes to a
///   person, so it's never silent.
///
/// And four raise it to High at least: a test the change adds that can't
/// fail when what it pins breaks; a claim that would lead someone to ship a
/// defect; a pre-existing defect the change claims to fix or makes worse;
/// and a defect of the change's own that meets the lens's definition of a
/// blocker, which the repository's calibration set.
/// A real finding outside the reviewer's own lens is rated like any other.
pub(super) fn ruling(cfg: &Config, lens: &Lens, r: &roles::Rating) -> Ruled {
    if !r.relevant || r.equivalent || r.severity == Severity::Rejected {
        return Ruled::plain(Severity::Rejected, Action::Drop);
    }
    if r.weak_pin
        || r.misleads
        || (r.pre_existing && (r.claims_fix || r.worsens))
        || (r.meets_blocker && !r.pre_existing)
    {
        let severity = if r.severity == Severity::Critical {
            Severity::Critical
        } else {
            Severity::High
        };
        return Ruled::plain(severity, cfg.action(lens, severity));
    }
    if r.pre_existing {
        return Ruled {
            severity: Severity::Low,
            action: Action::NeverBlocks,
            escalate: r.severity == Severity::Critical,
        };
    }
    Ruled::plain(r.severity, cfg.action(lens, r.severity))
}

/// What the gate measured in the change's own code and tests blocks by
/// rule, unless the Judge found it no defect at all.
pub(super) fn gate_ruling(cfg: &Config, lens: &Lens, f: &mut Finding, measured: bool) {
    if measured && f.state != FindingState::Dropped && !f.blocks() {
        f.severity = Severity::High;
        f.action = cfg.action(lens, Severity::High);
        f.state = FindingState::Open;
        f.reason = format!(
            "{} (Blocks by rule: the gate measured it in the change's own code and tests.)",
            f.reason
        );
    }
}

/// The text of the rules that decide what blocks: a calibration made before
/// they changed is stale.
pub(super) fn rules_source() -> String {
    let flow = super::calibration::source_of(
        include_str!("flow.rs"),
        &[
            "pub(super) fn ruling(",
            "pub(super) fn grounded(",
            "pub(super) fn gate_ruling(",
        ],
    );
    let stages = super::calibration::source_of(
        include_str!("stages.rs"),
        &[
            "pub fn changed_lines(",
            "pub fn adds_text(",
            "pub fn quotes_text(",
            "fn quote_parts(",
            // Whether a mutant every test passes blocks: it must break a
            // line the change adds, against one of the change's own tests.
            "fn in_the_change(",
            "pub fn changed_tests(",
            "fn added_lines(",
            "fn is_doc(",
            "fn is_test_path(",
            "fn test_modules(",
            "fn closing_line(",
            // Which of the gate's gaps block: by rule, and as the Judge rates.
            "pub fn run_gate(",
        ],
    );
    format!("{flow}\n{stages}")
}

/// A ruling: the severity and action rules give a finding, and whether it
/// also goes to a person.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Ruled {
    pub severity: Severity,
    pub action: Action,
    pub escalate: bool,
}

impl Ruled {
    fn plain(severity: Severity, action: Action) -> Ruled {
        Ruled {
            severity,
            action,
            escalate: false,
        }
    }
}

/// Facts the diff and the config settle, whatever the Judge said: a finding
/// in a line the change adds isn't pre-existing; a weak pin or a misleading
/// claim holds only when its quote is something the change adds; and a
/// finding meets the lens's definition of a blocker only when its quote is
/// words of that definition.
pub(super) fn grounded(
    mut r: roles::Rating,
    changed: &stages::ChangedLines,
    blocker: Option<&str>,
) -> roles::Rating {
    if let (Some(file), Some(line)) = (r.file.as_deref(), r.line)
        && changed
            .get(file)
            .is_some_and(|lines| lines.contains_key(&line))
    {
        r.pre_existing = false;
    }
    let quoted = |quotes: &[String]| quotes.iter().any(|q| stages::adds_text(changed, q));
    r.weak_pin = r.weak_pin && quoted(&r.weak_pin_test);
    r.misleads = r.misleads && quoted(&r.misleading_claim);
    r.meets_blocker = r.meets_blocker
        && blocker.is_some_and(|b| r.blocker_clause.iter().any(|q| stages::quotes_text(b, q)));
    r
}

fn rate(
    run: &mut Run,
    cfg: &Config,
    lens: &Lens,
    ratings: Vec<roles::Rating>,
    changed: &stages::ChangedLines,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (n, rating) in ratings.into_iter().enumerate() {
        let rating = grounded(rating, changed, lens.blocker.as_deref());
        let Ruled {
            severity,
            action,
            escalate: to_person,
        } = ruling(cfg, lens, &rating);
        let state = match action {
            Action::Blocks | Action::BlocksUnlessWaived => FindingState::Open,
            Action::NeverBlocks => FindingState::Logged,
            Action::Drop => FindingState::Dropped,
        };
        let id = format!("{}-r{}-{}", lens.id, run.round, n + 1);
        if to_person {
            escalate_about(
                run,
                format!(
                    "{id} is Critical and was there before the change, which neither claims to \
                     fix it nor makes it worse, so it doesn't block: {}",
                    rating.title
                ),
                Some(About::Finding {
                    lens: lens.id.clone(),
                    finding: id.clone(),
                }),
            );
        }
        if rating.split || (rating.values_call && severity == Severity::Rejected) {
            let what = if rating.split {
                "reviewers split"
            } else {
                "a values call"
            };
            escalate_about(
                run,
                format!("{what} on {id}: {}", rating.title),
                Some(About::Finding {
                    lens: lens.id.clone(),
                    finding: id.clone(),
                }),
            );
            run.disagreements.push(Disagreement {
                lens: lens.id.clone(),
                finding: id.clone(),
                sides: rating.sides.clone(),
            });
        }
        findings.push(Finding {
            id,
            title: rating.title,
            detail: rating.detail,
            file: rating.file,
            line: rating.line,
            severity,
            action,
            // Recorded, never lowered: a real defect blocks whichever lens
            // finds it.
            reason: if rating.on_task {
                rating.reason
            } else {
                format!("{} (outside this lens's criteria)", rating.reason)
            },
            state,
            note: String::new(),
            same_as: None,
            reused_from: rating.reused_from,
            ruled_by: rating.ruled_by,
        });
    }
    findings
}

/// The repository's record of what its reviewers open, when the review
/// keeps one and the config learns.
fn learned_record(run: &Run, s: &Setting) -> Option<PathBuf> {
    let dir = s.learned.as_deref().filter(|_| s.cfg.facts.learned)?;
    let name = run.repo.file_name()?.to_string_lossy().into_owned();
    Some(super::learned::Learned::path(dir, &name))
}

/// Adds to this round's facts the files the repository's reviewers keep
/// opening, ranked by the record, within the config's budget, and says what
/// it gave and left out.
fn add_learned(
    built: &mut super::facts::Facts,
    run: &Run,
    s: &Setting,
    prior: Option<&super::facts::Facts>,
    caller: &dyn Caller,
) {
    let Some(path) = learned_record(run, s) else {
        return;
    };
    let record = super::learned::Learned::load(&path);
    let changed: BTreeSet<String> = built.current().map(|f| f.path.clone()).collect();
    // Every reviewer and Judge reads a given file again each turn, so only
    // a file more than one reviewer call opened is worth that.
    let ranked: Vec<(String, u32)> = record
        .ranked(&changed)
        .into_iter()
        .filter(|(_, n)| *n >= super::learned::MIN_OPENS)
        .collect();
    let budget = usize::try_from(s.cfg.facts.learned_bytes).unwrap_or(usize::MAX);
    let added = built.add_learned(run.reading(), run.round, prior, &ranked, budget);
    if added.files == 0 && added.left_out == 0 {
        return;
    }
    let left = if added.left_out > 0 {
        format!(
            "; {} more the budget ({} bytes) left out",
            added.left_out, s.cfg.facts.learned_bytes
        )
    } else {
        String::new()
    };
    caller.say(&format!(
        "round {}: the facts carry {} file(s) this repository's reviewers keep opening, {} new, \
         {} bytes{left}",
        run.round, added.files, added.new, added.bytes
    ));
}

/// Adds what this round's reviewers read to the repository's record, once
/// a round: each reviewer call counts each file it read once, whichever of
/// its sessions read it, a relay's legs or a scout's and its verifiers'.
fn learn_round(run: &mut Run, s: &Setting) {
    let Some(path) = learned_record(run, s) else {
        return;
    };
    if run.learned.contains(&run.round) {
        return;
    }
    let repo = run.reading().to_path_buf();
    let mut record = super::learned::Learned::load(&path);
    // Each reviewer call's reading, every session's together.
    let mut calls: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for u in run
        .usage
        .iter()
        .filter(|u| u.round == run.round && u.role == Role::Reviewer)
    {
        let Some(lens) = &u.lens else {
            continue;
        };
        let call = u
            .call
            .clone()
            .or_else(|| u.label.clone())
            .unwrap_or_default();
        calls.entry((lens.clone(), call)).or_default().extend(
            u.opened
                .iter()
                .filter_map(|o| super::learned::in_repo(&repo, o)),
        );
    }
    let mut any = false;
    for ((lens, _), files) in &calls {
        if !files.is_empty() {
            record.add(lens, files);
            any = true;
        }
    }
    run.learned.push(run.round);
    if any {
        // Only a hint for later rounds: a record that can't be written
        // leaves the review as it was.
        let _ = record.save(&path);
    }
}

/// Keeps this round's rulings that didn't block, for a later round's same
/// claim: each finding a Judge ruled on just now, at a place in a file, with
/// its own title and those of the reviewers' findings at that place. A
/// ruling carried over is already kept. A blocker's ruling never is, whatever
/// a person then does with it: folded, waived or deferred to a ticket are
/// decisions, not rulings, so a later round's same claim is judged again and
/// takes the state the rules give it.
fn keep_rulings(
    run: &mut Run,
    lens: &Lens,
    context: &str,
    ratings: &[roles::Rating],
    findings: &[Finding],
    lists: &[Vec<Raw>],
) {
    let tree = run.reading().to_path_buf();
    for (rating, f) in ratings.iter().zip(findings) {
        if rating.reused_from.is_some()
            || !matches!(f.state, FindingState::Logged | FindingState::Dropped)
        {
            continue;
        }
        let (Some(file), Some(line)) = (rating.file.as_deref(), rating.line) else {
            continue;
        };
        let Some(file_hash) = file_hash(&tree, file) else {
            continue;
        };
        let mut claims: Vec<String> = std::iter::once(claim_key(&rating.title))
            .chain(
                lists
                    .iter()
                    .flatten()
                    .filter(|r| {
                        r.file.as_deref() == Some(file)
                            && r.line.is_some_and(|l| l.abs_diff(line) <= SAME_PLACE)
                    })
                    .map(|r| claim_key(&r.title)),
            )
            .filter(|c| !c.is_empty())
            .collect();
        claims.sort();
        claims.dedup();
        if claims.is_empty() {
            continue;
        }
        run.rulings.push(PastRuling {
            lens: lens.id.clone(),
            round: run.round,
            file: file.to_string(),
            line,
            claims,
            file_hash,
            context: context.to_string(),
            rating: rating.clone(),
        });
    }
}

/// One round: attack every open lens, judge what comes back, and settle
/// what's next. Judging stays out of the orchestration's context.
/// Builds the round's facts from the change at its head, reusing the last
/// round's for every file a fix left alone, keeps them in the run's folder,
/// says how much they share with the last round's, and gives them to the
/// caller for every reviewer and Judge. True if the caller gives them.
fn share_facts(
    run: &mut Run,
    s: &Setting,
    change: &Change,
    caller: &dyn Caller,
    gate: &str,
) -> Result<bool> {
    use super::facts;
    let prior = (run.round > 1)
        .then(|| facts::kept(&s.work_dir, run.round - 1))
        .flatten();
    let mut built = facts::Facts::build(
        run.reading(),
        change,
        run.round,
        prior.as_ref().map(|(f, _)| f),
    );
    add_learned(&mut built, run, s, prior.as_ref().map(|(f, _)| f), caller);
    // The pull request's words, fetched again each round, since its
    // comments can grow. gh runs in the checkout, whose remote names the
    // repository.
    let pr = match run.pr {
        None => {
            if run.round == 1 {
                caller.say(&format!(
                    "round {}: the facts carry no pull request's words: the review names no \
                     pull request (--pr)",
                    run.round
                ));
            }
            None
        }
        Some(n) => match facts::fetch_pr(&run.repo, n) {
            Ok(text) => Some(text),
            Err(e) => {
                caller.say(&format!(
                    "round {}: the facts carry no pull request's words: {e:#}",
                    run.round
                ));
                None
            }
        },
    };
    let text = built.render_with(run.round, Some(gate), pr.as_ref());
    let path = facts::keep(&s.work_dir, run.round, &built, &text)?;
    if let Some((_, before)) = &prior {
        let p = facts::Prefix::new(
            run.round,
            format!("the facts, with round {}'s", run.round - 1),
            facts::shared_prefix(before, &text),
        );
        caller.say(&format!("round {}: {}", run.round, p.line()));
        run.prefixes.push(p);
    }
    let with_pr = pr.as_ref().map_or_else(String::new, |p| {
        format!(
            ", with pull request #{}'s words and {} issue(s)",
            p.number,
            p.issues.len()
        )
    });
    caller.say(&format!(
        "round {}: the facts: {} file(s){with_pr}, {} bytes, ahead of every reviewer's and \
         Judge's own text",
        run.round,
        built.current().count(),
        text.len()
    ));
    run.facts.retain(|f| f.round != run.round);
    run.facts.push(FactsRound {
        round: run.round,
        key: facts::Facts::key(&text),
        bytes: text.len() as u64,
        files: built.current().count() as u64,
        given: false,
    });
    let cfg = s.cfg;
    let mut agents: Vec<String> = cfg
        .lenses
        .iter()
        .filter_map(|l| cfg.binding(Role::Reviewer, Some(l)).ok())
        .chain(cfg.binding(Role::Judge, None).ok())
        .map(|b| b.agent.clone())
        .collect();
    agents.sort();
    agents.dedup();
    let given = caller.share(Some(Shared {
        facts: path,
        key: run.id.clone(),
        tools: matched_tools(cfg),
        agents,
    }));
    if let Some(f) = run.facts.iter_mut().find(|f| f.round == run.round) {
        f.given = given;
    }
    Ok(given)
}

/// When roles' tools are matched: for each bridge and model that more than
/// one tool list shares among the reviewers and the Judge, the union of
/// those lists, sorted, so their calls' tool definitions are the same. A
/// group with a binding that names no tools is left as it is.
pub fn matched_tools(cfg: &Config) -> BTreeMap<ToolGroup, Vec<String>> {
    let mut groups: BTreeMap<ToolGroup, Vec<Option<&Vec<String>>>> = BTreeMap::new();
    if !cfg.facts.match_tools {
        return BTreeMap::new();
    }
    let mut names: Vec<&str> = cfg
        .lenses
        .iter()
        .filter_map(|l| cfg.binding_name(Role::Reviewer, Some(l)).ok())
        .collect();
    names.extend(cfg.binding_name(Role::Judge, None).ok());
    names.sort_unstable();
    names.dedup();
    for name in names {
        if let Some(b) = cfg.allowed_bindings.get(name) {
            groups
                .entry((b.agent.clone(), b.model.clone()))
                .or_default()
                .push(b.tools.as_ref());
        }
    }
    let mut out = BTreeMap::new();
    for (key, lists) in groups {
        if lists.iter().any(Option::is_none) {
            continue;
        }
        let distinct: BTreeSet<&Vec<String>> = lists.iter().flatten().copied().collect();
        if distinct.len() < 2 {
            continue;
        }
        let union: BTreeSet<String> = distinct.into_iter().flatten().cloned().collect();
        out.insert(key, union.into_iter().collect());
    }
    out
}

fn review_round(run: &mut Run, s: &Setting, change: &Change, caller: &dyn Caller) -> Result<()> {
    let cfg = s.cfg;
    let artifacts = write_artifact(s, run, change, run.round)?;
    if run.round > 1 {
        choose_lenses(run, s, change, caller, &artifacts.diff)?;
    }
    // Only an Orchestrator may change what the review reads.
    let before = caller.fingerprint(run.reading());
    // This round's copies, made from one copy set up once: the package
    // install and a first build. They stay when it ends, to be looked at.
    let round_dir = Copies::fresh(
        s.copies
            .as_ref()
            .map(|root| root.join(format!("{}-r{}", run.id, run.round))),
    );
    if cfg.tests.setup.is_some() && round_dir.0.is_some() {
        caller.note(&format!(
            "round {}: setting up a copy for the round's reviewers (tests.setup)",
            run.round
        ));
    }
    let warm = warm_copy(run, s, round_dir.0.as_deref());
    caller.say(&format!("round {}: {}", run.round, warm.said));
    let warm_note = warm.note.clone();
    let source = warm
        .copy
        .clone()
        .unwrap_or_else(|| run.reading().to_path_buf());
    let copying = Copying {
        dir: round_dir.0.as_deref(),
        source: &source,
    };
    // What the gate found, for every reviewer this round, after a failed
    // setup, which every reviewer is told of too.
    let mut gate_text = warm.brief.clone().unwrap_or_default();
    // The test gate first. A gap that blocks ends the round before any
    // reviewer runs only if `tests.gate` is `stop`.
    // A round going on after an outage keeps the gate it already ran.
    let gate_done = run
        .outage
        .as_ref()
        .filter(|o| o.round == run.round)
        .and_then(|_| stages::lens_id(Kind::Gate))
        .and_then(|id| run.lenses.get(id))
        .and_then(|l| l.rounds.iter().find(|r| r.round == run.round))
        .cloned();
    if let Some(round) = &gate_done {
        gate_text.push_str(&gate_report(round));
    }
    // The gate runs before this round's facts exist, since they carry its
    // results: its calls get none, rather than the last round's facts and
    // cache key, which describe the change before its fix.
    caller.share(None);
    if let Some(id) = stages::lens_id(Kind::Gate).filter(|id| {
        gate_done.is_none()
            && cfg.stages.contains(&Kind::Gate)
            && run
                .lenses
                .get(*id)
                .is_some_and(|l| l.outcome == Outcome::Open)
    }) {
        caller.note(&format!(
            "round {}: the tests gate runs the change's tests, and its mutants",
            run.round
        ));
        let mut round = gate_round(run, s, caller, &artifacts, change, id, copying)?;
        round.notes.extend(warm_note.clone());
        let blocks = round
            .findings
            .iter()
            .any(|f| f.blocks() && f.state == FindingState::Open);
        for failure in &round.failures {
            escalate_about(
                run,
                format!(
                    "{id} wasn't fully checked in round {}: {failure}",
                    run.round
                ),
                Some(About::Unfinished {
                    lens: id.to_string(),
                }),
            );
        }
        gate_text.push_str(&gate_report(&round));
        let lens = run.lenses.get_mut(id).expect("open");
        if round.failures.is_empty() && !blocks {
            lens.outcome = Outcome::Confirmed;
        }
        lens.rounds.push(round);
        if blocks && cfg.tests.gate.unwrap_or_default() == GateMode::Stop {
            settle(run, cfg);
            return Ok(());
        }
    }
    // The round's facts, from tools, ahead of every reviewer's and Judge's
    // own text. They carry the gate's results, last, so the reviewers' own
    // text doesn't too.
    let shared = cfg.facts.shared && {
        match share_facts(run, s, change, caller, &gate_text) {
            Ok(true) => true,
            Ok(false) => {
                caller.say(&format!(
                    "round {}: the facts are kept, but go to no call: not every agent the \
                     reviewers and the Judge are bound to can take them",
                    run.round
                ));
                false
            }
            Err(e) => {
                caller.say(&format!("round {}: no shared facts: {e:#}", run.round));
                caller.share(None);
                false
            }
        }
    };
    let gate_for_reviewers = if shared { String::new() } else { gate_text };
    let (reviews, mut judgments, mut notes) = attack(
        run,
        s,
        caller,
        &artifacts,
        change,
        copying,
        &gate_for_reviewers,
    )?;
    if let Some(note) = &warm_note {
        for id in reviews.keys() {
            notes.entry(id.clone()).or_default().push(note.clone());
        }
    }
    // Each lens's reviewers, then its Judge, which `attack` ran as soon as
    // that lens's reviewers were done.
    let mut lenses: Vec<(String, &Lens, Round, Vec<Vec<Raw>>)> = Vec::new();
    for (id, results) in reviews {
        let lens = lens_of(cfg, &id)?;
        let name = cfg.binding_name(Role::Reviewer, Some(lens))?;
        let mut round = Round {
            round: run.round,
            notes: notes.remove(&id).unwrap_or_default(),
            ..Round::default()
        };
        let mut lists = Vec::new();
        for (n, result) in results.into_iter().enumerate() {
            let label = format!("Reviewer {} (AI agent, {name}), {id}", n + 1);
            match result {
                Ok(list) => {
                    round.reviewers.push(label);
                    lists.push(list);
                }
                Err(e) => round.failures.push(format!("{label}: {e:#}")),
            }
        }
        lenses.push((id, lens, round, lists));
    }
    let judged: Vec<Judged> = lenses
        .iter()
        .map(|(id, ..)| judgments.remove(id).flatten())
        .collect();
    let changed = before.is_some() && caller.fingerprint(run.reading()) != before;
    let lines = stages::changed_lines(&change.diff);
    let rules = if cfg.savings.reuse_rulings {
        rules_source()
    } else {
        String::new()
    };
    for ((id, lens, mut round, lists), judged) in lenses.into_iter().zip(judged) {
        match judged {
            Some(Ok((usages, ratings))) => {
                for mut usage in usages {
                    usage.round = run.round;
                    run.usage.push(usage);
                }
                let kept = cfg.savings.reuse_rulings.then(|| ratings.clone());
                round.findings = rate(run, cfg, lens, ratings, &lines);
                if let Some(kept) = kept {
                    let context = ruling_context(cfg, lens, &s.not_findings, &rules);
                    keep_rulings(run, lens, &context, &kept, &round.findings, &lists);
                }
            }
            Some(Err(e)) => round.failures.push(format!("the Judge: {e:#}")),
            None => {}
        }
        if changed {
            round.failures.push(
                "the repository changed during the reviewers' or the Judges' turn, and only an \
                 Orchestrator may change it"
                    .into(),
            );
        }
        for failure in &round.failures {
            escalate_about(
                run,
                format!(
                    "{id} wasn't fully reviewed in round {}: {failure}",
                    run.round
                ),
                Some(About::Unfinished {
                    lens: id.to_string(),
                }),
            );
        }
        let confirmed = round.failures.is_empty()
            && !round
                .findings
                .iter()
                .any(|f| f.blocks() && f.state == FindingState::Open);
        let result = run.lenses.get_mut(&id).expect("reviewed because it's open");
        result.rounds.push(round);
        if confirmed {
            result.outcome = Outcome::Confirmed;
        }
    }
    same_defects(run, s, caller);
    settle(run, cfg);
    Ok(())
}

/// Where a finding points: its file and line, its file, or the change.
pub(super) fn place(f: &Finding) -> String {
    match (&f.file, f.line) {
        (Some(file), Some(line)) => format!("{file}:{line}"),
        (Some(file), None) => file.clone(),
        _ => "the change".to_string(),
    }
}

/// Words too common in code and in reviews to say two findings are about
/// one thing, even in a code span: keywords, built-in types, and the
/// everyday names of everyday things.
const COMMON_NAMES: &[&str] = &[
    "append",
    "array",
    "assert",
    "async",
    "await",
    "bool",
    "boolean",
    "break",
    "buffer",
    "bytes",
    "case",
    "chan",
    "channel",
    "class",
    "close",
    "config",
    "const",
    "context",
    "continue",
    "data",
    "default",
    "defer",
    "duration",
    "else",
    "enum",
    "equals",
    "error",
    "errorf",
    "errors",
    "expect",
    "false",
    "file",
    "files",
    "float",
    "func",
    "function",
    "goroutine",
    "hashcode",
    "impl",
    "import",
    "init",
    "input",
    "interface",
    "item",
    "items",
    "json",
    "length",
    "line",
    "lines",
    "list",
    "lock",
    "loop",
    "main",
    "make",
    "match",
    "mutex",
    "name",
    "names",
    "none",
    "null",
    "number",
    "object",
    "open",
    "option",
    "options",
    "output",
    "package",
    "panic",
    "path",
    "print",
    "printf",
    "println",
    "private",
    "public",
    "range",
    "read",
    "result",
    "results",
    "return",
    "select",
    "self",
    "slice",
    "sprintf",
    "state",
    "static",
    "status",
    "string",
    "struct",
    "switch",
    "test",
    "tests",
    "this",
    "time",
    "tostring",
    "true",
    "type",
    "types",
    "uint",
    "undefined",
    "unlock",
    "value",
    "values",
    "void",
    "while",
    "write",
];

/// The names in a finding's words that point at one thing in the code: a
/// symbol, field, variable or function. A word counts when its shape only
/// an identifier has, camelCase or snake_case, or when it's written in a
/// code span (`like_this`); a dotted path counts by its parts. Plain
/// words, common names (`COMMON_NAMES`) and names under four characters
/// don't count. Deterministic: the same words give the same names.
pub(super) fn identifiers(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (n, segment) in text.split('`').enumerate() {
        let spanned = n % 2 == 1;
        for token in segment.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')) {
            for word in token.split('.') {
                let word = word.trim_matches('_');
                if word.chars().count() < 4
                    || word.starts_with(|c: char| c.is_ascii_digit())
                    || COMMON_NAMES.contains(&word.to_lowercase().as_str())
                {
                    continue;
                }
                let camel = word
                    .chars()
                    .zip(word.chars().skip(1))
                    .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
                let snake = word.contains('_');
                if camel || snake || spanned {
                    out.insert(word.to_string());
                }
            }
        }
    }
    out
}

/// A pair of findings, from different lenses at different places, that
/// name the same thing in the code: it may be one defect showing at two
/// places. Only proposed; the dedupe call decides.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Candidate {
    pub a: String,
    pub b: String,
    /// The names both findings use.
    pub shared: Vec<String>,
}

/// Findings that may be one defect at two places: from different lenses,
/// at different places, in the same file or in files one finding names
/// the other's, and naming the same identifier. Findings at the same
/// place are left to the dedupe call as they always were.
pub(super) fn candidates(found: &[(String, Finding)]) -> Vec<Candidate> {
    let names: Vec<BTreeSet<String>> = found
        .iter()
        .map(|(_, f)| identifiers(&format!("{}\n{}", f.title, f.detail)))
        .collect();
    let base = |file: &str| file.rsplit('/').next().unwrap_or(file).to_string();
    let mut out = Vec::new();
    for i in 0..found.len() {
        for j in i + 1..found.len() {
            let ((la, a), (lb, b)) = (&found[i], &found[j]);
            if la == lb || place(a) == place(b) {
                continue;
            }
            let related = match (&a.file, &b.file) {
                (Some(fa), Some(fb)) if fa == fb => true,
                (Some(fa), Some(fb)) => {
                    let (ta, tb) = (
                        format!("{}\n{}", a.title, a.detail),
                        format!("{}\n{}", b.title, b.detail),
                    );
                    ta.contains(&base(fb)) || tb.contains(&base(fa))
                }
                _ => false,
            };
            if !related {
                continue;
            }
            let shared: Vec<String> = names[i].intersection(&names[j]).cloned().collect();
            if !shared.is_empty() {
                out.push(Candidate {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    shared,
                });
            }
        }
    }
    out
}

/// Findings that different lenses made of one defect this round, grouped
/// by one small call on the Sizer's binding, so that a person decides each
/// defect once and whoever fixes reads it once. Each keeps its own ruling:
/// the grouping only says which to decide together. A lens's own findings
/// aren't grouped, and a call that fails groups nothing, and says so.
/// Pairs at different places that name the same thing in the code are
/// put to the call explicitly, since one cause can show at two places; the
/// call still decides.
fn same_defects(run: &mut Run, s: &Setting, caller: &dyn Caller) {
    let round = run.round;
    let found: Vec<(String, Finding)> = run
        .lenses
        .values()
        .filter_map(|l| {
            l.rounds
                .last()
                .filter(|r| r.round == round)
                .map(|r| (l.id.clone(), r))
        })
        .flat_map(|(lens, r)| {
            r.findings
                .iter()
                .filter(|f| {
                    matches!(
                        f.state,
                        FindingState::Open | FindingState::Logged | FindingState::Folded
                    )
                })
                .map(move |f| (lens.clone(), f.clone()))
        })
        .collect();
    let lenses: std::collections::BTreeSet<&str> = found.iter().map(|(l, _)| l.as_str()).collect();
    if lenses.len() < 2 {
        return;
    }
    let listed: Vec<(String, String, String, String, String)> = found
        .iter()
        .map(|(lens, f)| {
            (
                lens.clone(),
                f.id.clone(),
                place(f),
                f.title.clone(),
                f.detail.clone(),
            )
        })
        .collect();
    let proposed = candidates(&found);
    let at = |id: &str| {
        found
            .iter()
            .find(|(_, f)| f.id == id)
            .map(|(_, f)| place(f))
            .unwrap_or_default()
    };
    let pairs: Vec<(String, String, String, String, Vec<String>)> = proposed
        .iter()
        .map(|c| {
            (
                c.a.clone(),
                at(&c.a),
                c.b.clone(),
                at(&c.b),
                c.shared.clone(),
            )
        })
        .collect();
    let what = DEDUPE;
    let groups = ask(
        run,
        s,
        caller,
        Role::Sizer,
        what,
        &roles::same_prompt(&listed, &pairs),
        None,
    )
    .and_then(|text| roles::read_same(&text));
    if !proposed.is_empty() {
        let merged = match &groups {
            Ok(groups) => proposed
                .iter()
                .filter(|c| groups.iter().any(|g| g.contains(&c.a) && g.contains(&c.b)))
                .count(),
            Err(_) => 0,
        };
        if let Some(r) = run.lenses.values_mut().find_map(|l| l.rounds.last_mut()) {
            r.notes.push(format!(
                "{} pair(s) of findings at different places name the same thing in the code, \
                 and were put to the dedupe: {}. It ruled {merged} the same defect.",
                proposed.len(),
                proposed
                    .iter()
                    .map(|c| format!("{} and {} (`{}`)", c.a, c.b, c.shared.join("`, `")))
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }
    let groups = match groups {
        Ok(groups) => groups,
        Err(e) => {
            if let Some(r) = run.lenses.values_mut().find_map(|l| l.rounds.last_mut()) {
                r.notes.push(format!(
                    "findings more than one lens made of one defect weren't grouped: {e:#}"
                ));
            }
            return;
        }
    };
    for group in groups {
        let mut members: Vec<&(String, Finding)> = found
            .iter()
            .filter(|(_, f)| group.contains(&f.id))
            .collect();
        // The one that blocks, and the gravest, stands for the rest.
        members.sort_by_key(|(_, f)| (!f.blocks(), f.severity));
        let Some(((first_lens, first), rest)) = members.split_first().map(|(f, r)| (*f, r)) else {
            continue;
        };
        for (lens, f) in rest.iter().filter(|(l, _)| l != first_lens) {
            if let Some(g) = run
                .lenses
                .get_mut(lens)
                .and_then(|l| l.rounds.last_mut())
                .and_then(|r| r.findings.iter_mut().find(|g| g.id == f.id))
            {
                g.same_as = Some(first.id.clone());
            }
        }
    }
}

fn all_settled(run: &Run) -> bool {
    run.lenses.values().all(|l| l.outcome != Outcome::Open) && run.reported.iter().all(|r| r.fixed)
}

/// What comes after a round.
fn settle(run: &mut Run, cfg: &Config) {
    let open: Vec<String> = run
        .lenses
        .values()
        .filter(|l| l.outcome == Outcome::Open)
        .map(|l| l.id.clone())
        .collect();
    // In a review to convergence, nothing blocks now, but a lens that
    // hasn't attacked the change as it is now must, before it can end: the
    // full sweep.
    if open.is_empty() && run.converge && run.round < cfg.max_rounds {
        let fixed_after = run.steelmans.last().map_or(0, |m| m.round);
        let stale: Vec<String> = run
            .lenses
            .values()
            .filter(|l| {
                l.outcome == Outcome::Confirmed
                    && fixed_after > 0
                    && l.rounds.last().is_none_or(|r| r.round <= fixed_after)
            })
            .map(|l| l.id.clone())
            .collect();
        if !stale.is_empty() {
            for id in stale {
                if let Some(l) = run.lenses.get_mut(&id) {
                    l.outcome = Outcome::Open;
                }
            }
            run.sweep = true;
            run.round += 1;
            run.stage = Stage::Review;
            return;
        }
    }
    if open.is_empty() {
        run.stage = if cfg.human_gate.asks_final() {
            Stage::Final
        } else {
            Stage::Done
        };
        return;
    }
    let blocked = run.lenses.values().any(|l| !l.blockers().is_empty());
    if run.round >= cfg.max_rounds {
        for id in &open {
            escalate_about(
                run,
                format!("{id} still has blockers at max_rounds ({})", cfg.max_rounds),
                Some(About::MaxRounds),
            );
        }
        run.stage = Stage::Done;
        return;
    }
    if !blocked {
        // Open with no blockers: its round failed, and a person has heard.
        // They decide whether it goes again.
        run.round += 1;
        run.stage = Stage::Review;
        return;
    }
    if cfg.human_gate.asks_fold() {
        run.stage = Stage::Decide;
    } else {
        for lens in run.lenses.values_mut() {
            if let Some(round) = lens.rounds.last_mut() {
                for f in round.findings.iter_mut() {
                    if f.blocks() && f.state == FindingState::Open {
                        f.state = FindingState::Folded;
                    }
                }
            }
        }
        run.stage = Stage::Fix;
    }
}

/// Whether the first fix can go in. A review that read a copy of its
/// commit has its fixes go into the checkout all the same, where its
/// builder works, so the checkout must hold that commit, with nothing on
/// top, by then. Until it does, a person is asked to check it out.
fn ready_to_fix(run: &mut Run, s: &Setting) -> bool {
    if run.head_copy.is_none() || run.fixed() {
        return true;
    }
    match super::change::checked_out(
        &run.repo,
        &run.base,
        run.head.as_deref(),
        s.cfg.review_scope,
    ) {
        Ok(()) => true,
        Err(e) => {
            escalate(
                run,
                format!(
                    "The review read a copy of its commit, but its fixes go into the checkout: \
                     {e:#} Then go on."
                ),
            );
            false
        }
    }
}

/// Has the Orchestrator fix what's been folded in: only the blockers, or in
/// a review to convergence, a steelman of everything the Judge upheld.
fn fix(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    reload: &dyn Fn(bool) -> Result<Change>,
) -> Result<()> {
    // Only blockers: what's upheld without blocking goes to a person, since
    // folding it in grows the change, and gives the next round more to attack.
    let folded = to_fix(run);
    let reported: Vec<Reported> = run.reported.iter().filter(|r| !r.fixed).cloned().collect();
    let list = |findings: &[(String, Finding)]| fix_list(findings, &reported);
    let writing = run.converge && s.cfg.stages.contains(&Kind::TestWriting);
    let intent = if run.converge {
        let fixed = run.lenses.values().any(|l| !l.done.is_empty());
        let change = reload(fixed)?;
        super::change::intent(s.description.as_deref(), &change.commits)
    } else {
        String::new()
    };
    // The code as it is before the fixes, for the pins to fail on.
    let before = Copies::fresh(
        s.copies
            .as_ref()
            .filter(|_| writing)
            .map(|root| root.join(format!("{}-r{}-before", run.id, run.round))),
    );
    let snapshot = before.0.as_ref().and_then(|dir| {
        let copy = dir.join(run.repo.file_name().unwrap_or_default());
        super::change::copy_of(&run.repo, &copy).ok().map(|_| copy)
    });
    // The checkout before the fixes, to see what they changed: for the next
    // round's focus and the blast-radius check, when they're on, and the
    // files to flag. A fix in groups tried again keeps the one from before
    // its first group.
    let saving = s.cfg.savings.focus_after_fix || s.cfg.savings.blast_radius;
    let carried = run
        .partial_fix
        .as_ref()
        .filter(|p| p.round == run.round)
        .and_then(|p| p.from.clone());
    // Always, to list what the fix changed and flag what a person should see.
    let before_fix = carried.or_else(|| caller.snapshot(&run.repo));
    let fixed = if run.converge {
        let earlier: String = run
            .steelmans
            .iter()
            .map(|m| format!("- After round {}: {}\n", m.round, m.done))
            .collect();
        let lenses: String = run
            .lenses
            .keys()
            .filter_map(|id| s.cfg.lenses.iter().find(|l| &l.id == id))
            .map(|l| format!("- {}: {}\n", l.id, l.criteria.trim()))
            .collect();
        let prompt = roles::steelman_prompt(&intent, &earlier, &list(&folded), writing, &lenses);
        let label = format!("the steelman, folding in {} finding(s)", folded.len());
        let ids: Vec<String> = folded.iter().map(|(_, f)| f.id.clone()).collect();
        fix_call(run, s, caller, &prompt, label, None, &ids, false)
    } else {
        fix_in_groups(run, s, caller, &folded, &reported, before_fix.as_deref())
    };
    // What the fix changed so far, a failed one too, with the files a person
    // should see flagged.
    flag_fix(run, caller, before_fix.as_deref(), &folded, &reported);
    match fixed {
        Ok(fixed) => {
            // A fix in groups told each group's lenses what it did.
            if run.converge {
                for (id, _) in &folded {
                    let lens = run.lenses.get_mut(id).expect("found above");
                    if !lens.done.contains(&fixed.done) {
                        lens.done.push(fixed.done.clone());
                    }
                }
            }
            for r in run.reported.iter_mut() {
                r.fixed = true;
            }
            if !run.converge && !reported.is_empty() {
                reopen_all(run);
            }
            if run.converge {
                let lens_of = |finding: &str| {
                    folded
                        .iter()
                        .find(|(_, f)| f.id == finding)
                        .map(|(lens, _)| lens.clone())
                };
                for d in &fixed.declined {
                    run.disagreements.push(Disagreement {
                        lens: lens_of(&d.finding).unwrap_or_default(),
                        finding: d.finding.clone(),
                        sides: vec![
                            "The Judge upheld it.".into(),
                            format!("The steelman declined it: {}", d.reason),
                        ],
                    });
                }
                run.steelmans.push(Steelman {
                    round: run.round,
                    blockers: folded.iter().map(|(_, f)| f.id.clone()).collect(),
                    done: fixed.done.clone(),
                    declined: fixed.declined.clone(),
                    pins: Vec::new(),
                    tests_done: String::new(),
                    briefs: fixed.briefs.clone(),
                });
                if writing {
                    let (pins, done) =
                        write_tests(run, s, caller, &list(&folded), &intent, snapshot.as_deref());
                    if let Some(m) = run.steelmans.last_mut() {
                        m.pins = pins;
                        m.tests_done = done;
                    }
                }
                refocus(run, s, caller, before_fix.as_deref());
            } else if saving {
                note_fix(run, s, caller, before_fix.as_deref());
            }
            reattack(run, caller);
            run.round += 1;
            if run.reattacks() == Reattack::Blocked {
                blast_radius(run, s, caller);
            }
            run.stage = Stage::Review;
        }
        // A gateway that's down pauses the review, saved, with what the
        // groups before it did kept for its continuing.
        Err(e) if is_outage(&e) => return Err(e),
        Err(e) => {
            let carries = run
                .partial_fix
                .as_ref()
                .is_some_and(|p| !p.groups.is_empty());
            escalate_about(
                run,
                format!(
                    "the Orchestrator couldn't fix the blockers: {e:#}{}",
                    if carries {
                        ". What the groups before it changed stays in the checkout, and trying \
                         again carries on from the first group not done"
                    } else {
                        ""
                    }
                ),
                Some(About::Unfixed),
            );
            run.stage = Stage::Done;
        }
    }
    Ok(())
}

/// One session's share of a fix: blockers by lens, and defects a person
/// reported the reviewers missed.
#[derive(Debug, Clone, Default)]
struct FixGroup {
    findings: Vec<(String, Finding)>,
    reported: Vec<Reported>,
}

impl FixGroup {
    /// Its blockers' IDs, sorted, to know it again.
    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .findings
            .iter()
            .map(|(_, f)| f.id.clone())
            .chain(self.reported.iter().map(|r| r.id.clone()))
            .collect();
        ids.sort();
        ids
    }

    /// How many blockers it fixes: a defect found by several lenses once.
    fn blockers(&self) -> usize {
        let primary = |f: &Finding| {
            f.same_as
                .as_ref()
                .is_none_or(|p| !self.findings.iter().any(|(_, g)| &g.id == p))
        };
        self.findings.iter().filter(|(_, f)| primary(f)).count() + self.reported.len()
    }
}

/// A fix's blockers in groups, one session each: those that point at the
/// same file together, since one session fixing them can't collide with
/// itself, and every other alone. A finding that's the same defect as
/// another goes with it. A defect a person reported, which points at no
/// file, goes alone, as does a finding that points at none.
fn fix_groups(findings: &[(String, Finding)], reported: &[Reported]) -> Vec<FixGroup> {
    let primary_of = |f: &Finding| -> String {
        f.same_as
            .clone()
            .filter(|p| findings.iter().any(|(_, g)| &g.id == p))
            .unwrap_or_else(|| f.id.clone())
    };
    let mut groups: Vec<FixGroup> = Vec::new();
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_primary: BTreeMap<String, usize> = BTreeMap::new();
    for (lens, f) in findings.iter().filter(|(_, f)| primary_of(f) == f.id) {
        let at = f
            .file
            .as_deref()
            .map(|p| p.trim().trim_start_matches("./").to_string())
            .filter(|p| !p.is_empty());
        let n = match at.as_ref().and_then(|p| by_file.get(p)) {
            Some(&n) => n,
            None => {
                groups.push(FixGroup::default());
                if let Some(p) = at {
                    by_file.insert(p, groups.len() - 1);
                }
                groups.len() - 1
            }
        };
        groups[n].findings.push((lens.clone(), f.clone()));
        by_primary.insert(f.id.clone(), n);
    }
    for (lens, f) in findings.iter().filter(|(_, f)| primary_of(f) != f.id) {
        let n = by_primary[&primary_of(f)];
        groups[n].findings.push((lens.clone(), f.clone()));
    }
    for r in reported {
        groups.push(FixGroup {
            findings: Vec::new(),
            reported: vec![r.clone()],
        });
    }
    groups
}

/// Where a fix group goes under `savings.fixer`, and why: easy, to its
/// binding, only if every blocker in it is at or below `max_severity`, the
/// group cites at most `max_files` files, the Judge stated each one's cause
/// with a file, a line and a detail, none is a person's call (a values call,
/// a split, or theirs to rule), none is a defect a person reported missed,
/// and none is like one an earlier round's fix fixed, the same claim in the
/// same lens. Anything else is hard, for the Orchestrator's binding. The
/// rules guide where a fix goes; the config tunes them.
fn fix_route(run: &Run, fixer: &super::config::FixerConfig, group: &FixGroup) -> (bool, String) {
    let mut hard: Vec<String> = Vec::new();
    if !group.reported.is_empty() {
        hard.push("a defect a person reported missed".into());
    }
    let mut files = BTreeSet::new();
    let mut worst: Option<Severity> = None;
    for (lens, f) in &group.findings {
        if f.severity < fixer.max_severity || f.severity == Severity::Critical {
            hard.push(format!("a {:?} blocker", f.severity));
        }
        worst = Some(worst.map_or(f.severity, |w| w.min(f.severity)));
        let file = f
            .file
            .as_deref()
            .map(|p| p.trim().trim_start_matches("./"))
            .filter(|p| !p.is_empty());
        match (file, f.line) {
            (Some(file), Some(_)) if !f.detail.trim().is_empty() => {
                files.insert(file.to_string());
            }
            (Some(file), _) => {
                files.insert(file.to_string());
                hard.push("a blocker whose cause isn't stated with a line and detail".into());
            }
            (None, _) => hard.push("a blocker whose cause isn't stated with a file".into()),
        }
        let called = run
            .escalations
            .iter()
            .any(|e| matches!(&e.about, Some(About::Finding { finding, .. }) if *finding == f.id));
        if called {
            hard.push("a person's call".into());
        }
        let key = claim_key(&f.title);
        if let Some(before) = run.lenses.get(lens).and_then(|l| {
            l.rounds
                .iter()
                .filter(|r| r.round < run.round)
                .find(|r| {
                    r.findings
                        .iter()
                        .any(|g| g.state == FindingState::Folded && claim_key(&g.title) == key)
                })
                .map(|r| r.round)
        }) {
            hard.push(format!(
                "recurring: like a blocker fixed after round {before}"
            ));
        }
    }
    if files.len() > fixer.max_files as usize {
        hard.push(format!("cites {} files", files.len()));
    }
    hard.dedup();
    if hard.is_empty() {
        let worst = worst.map_or_else(|| "no".to_string(), |w| format!("{w:?}"));
        (
            true,
            format!("easy: {} file(s), {worst}, cause stated", files.len()),
        )
    } else {
        (false, format!("hard: {}", hard.join("; ")))
    }
}

/// A fix outside a review to convergence: in groups when
/// `savings.group_fixes` is on, each a fresh session from the same opening
/// with only its own blockers, one after another in the checkout, so each
/// sees what the ones before it changed. Each group's lenses are told what
/// it did as it finishes, and a group that fails leaves the ones before it
/// done, for a fix tried again to carry on from.
fn fix_in_groups(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    folded: &[(String, Finding)],
    reported: &[Reported],
    from: Option<&str>,
) -> Result<roles::Fixed> {
    let groups = if s.cfg.savings.group_fixes {
        fix_groups(folded, reported)
    } else {
        vec![FixGroup {
            findings: folded.to_vec(),
            reported: reported.to_vec(),
        }]
    };
    let n = groups.len();
    let mut partial = run
        .partial_fix
        .take()
        .filter(|p| p.round == run.round)
        .unwrap_or_else(|| PartialFix {
            round: run.round,
            from: from.map(str::to_string),
            groups: Vec::new(),
        });
    let mut done = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        let ids = group.ids();
        if let Some(g) = partial.groups.iter().find(|g| g.ids == ids) {
            done.push(g.done.clone());
            continue;
        }
        let prompt = roles::fix_prompt(&fix_list(&group.findings, &group.reported));
        let (label, part) = if n > 1 {
            (
                format!(
                    "the Orchestrator, fixing group {} of {n}: {} blocker(s)",
                    i + 1,
                    group.blockers()
                ),
                Some(format!("group {}", i + 1)),
            )
        } else {
            (
                format!("the Orchestrator, fixing {} blocker(s)", group.blockers()),
                None,
            )
        };
        // Easy or hard, by the rules on its blockers' facts, when
        // `savings.fixer` is set: said, and kept, with why.
        let easy = match &s.cfg.savings.fixer {
            Some(fixer) => {
                let (easy, why) = fix_route(run, fixer, group);
                let binding = if easy {
                    fixer.binding.clone()
                } else {
                    s.cfg.binding_name(Role::Orchestrator, None)?.to_string()
                };
                caller.say(&format!("fix group {}: to {binding} ({why})", i + 1));
                run.routes.push(FixRoute {
                    round: run.round,
                    ids: ids.clone(),
                    binding,
                    easy,
                    why,
                });
                easy
            }
            None => false,
        };
        match fix_call(run, s, caller, &prompt, label, part, &ids, easy) {
            Ok(fixed) => {
                for (id, _) in &group.findings {
                    if let Some(lens) = run.lenses.get_mut(id)
                        && !lens.done.contains(&fixed.done)
                    {
                        lens.done.push(fixed.done.clone());
                    }
                }
                partial.groups.push(GroupDone {
                    ids,
                    done: fixed.done.clone(),
                });
                done.push(fixed.done);
            }
            Err(e) => {
                let e = if n > 1 {
                    e.context(format!("group {} of {n} ({})", i + 1, ids.join(", ")))
                } else {
                    e
                };
                if !partial.groups.is_empty() {
                    run.partial_fix = Some(partial);
                }
                return Err(e);
            }
        }
    }
    Ok(roles::Fixed {
        done: done.join("\n"),
        declined: Vec::new(),
        briefs: BTreeMap::new(),
    })
}

/// One fixer's call on the Orchestrator's binding, relayed when its calls
/// relay, each session on the record, marked with `part` when it's one of
/// several groups. One that leaves the checkout as it found it did no work,
/// unless it declined each of its `blockers` with a reason.
/// A fix's session, restarted once, fresh, if it comes back unusable: did
/// no work, failed, or gave an answer that can't be read. `easy`, a group
/// `savings.fixer` routed to its binding, goes there first; its restart
/// goes to the Orchestrator's, since a fix that failed isn't easy.
#[allow(clippy::too_many_arguments)]
fn fix_call(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    prompt: &str,
    label: String,
    part: Option<String>,
    blockers: &[String],
    easy: bool,
) -> Result<roles::Fixed> {
    let cfg = s.cfg;
    let orchestrator = (
        cfg.binding_name(Role::Orchestrator, None)?,
        cfg.binding(Role::Orchestrator, None)?,
    );
    let fixer = match cfg.fixer()? {
        Some((f, b)) if easy => Some((f.binding.as_str(), b)),
        _ => None,
    };
    let start = run.usage.len();
    let restarter = Restarter::new(caller, None);
    let route = cfg
        .savings
        .fixer
        .as_ref()
        .map(|_| if fixer.is_some() { "easy" } else { "hard" });
    // The checkout as the fix found it: what both sessions' work is set
    // against, so a restart that finds the first's edits in place isn't
    // taken for one that did nothing.
    let baseline = caller.snapshot(&run.repo);
    let first = fix_once(
        run,
        s,
        caller,
        prompt,
        label.clone(),
        part.clone(),
        blockers,
        fixer.unwrap_or(orchestrator),
        route,
        baseline.clone(),
    );
    let fixed = restarter.once(&label, first, |again| {
        if fixer.is_some() {
            caller.say(&format!(
                "{again}: to {} (hard: its easy fix failed)",
                orchestrator.0
            ));
        }
        // What the first session left in the working tree, said, so the
        // restart builds on it rather than finding nothing to do.
        let left = baseline.as_deref().and_then(|from| {
            let now = caller.snapshot(&run.repo)?;
            (now != from)
                .then(|| caller.delta(&run.repo, from, &now))
                .flatten()
        });
        let restart_prompt = match &left {
            Some((files, diff)) => format!(
                "{prompt}\n\n# Already in the working tree\n\nA session before you began \
                 this fix and made these edits, to {}, before its answer was lost. Check \
                 them, keep what's right, and finish the fix:\n\n```diff\n{}\n```\n",
                files.join(", "),
                diff.trim_end()
            ),
            None => prompt.to_string(),
        };
        fix_once(
            run,
            s,
            caller,
            &restart_prompt,
            again.to_string(),
            part,
            blockers,
            orchestrator,
            route.map(|_| "hard"),
            baseline,
        )
    });
    restarter.mark(&mut run.usage[start..]);
    fixed
}

/// One fix session, relayed if the Orchestrator's calls relay, on `on`.
#[allow(clippy::too_many_arguments)]
fn fix_once(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    prompt: &str,
    label: String,
    part: Option<String>,
    blockers: &[String],
    on: (&str, &Binding),
    route: Option<&str>,
    baseline: Option<String>,
) -> Result<roles::Fixed> {
    let cfg = s.cfg;
    let repo = run.repo.clone();
    let call = Call {
        role: Role::Orchestrator,
        binding_name: on.0,
        binding: on.1,
        prompt,
        artifact: None,
        writes: true,
        no_tools: false,
        relay_tokens: cfg.sessions.relay(Role::Orchestrator),
        repo: &repo,
        label,
    };
    let used = |c: &Call, a: &Answer| usage(cfg, c, c.prompt.len(), a);
    // Set against the checkout as the fix found it, not as this session
    // did: a restart's own changes and its predecessor's both count.
    let before = baseline.or_else(|| caller.snapshot(&repo));
    let what = call.label.clone();
    let legs = watched(run, s, caller, &what, |_| relayed(caller, &call, &used))?;
    for mut u in legs.usage.iter().cloned() {
        u.round = run.round;
        u.route = route.map(str::to_string);
        u.part = match (&part, u.part.take()) {
            (Some(group), Some(leg)) => Some(format!("{group}, {leg}")),
            (Some(group), None) => Some(group.clone()),
            (None, leg) => leg,
        };
        run.usage.push(u);
    }
    let fixed = fixed_of(&legs)?;
    let Some(before) = before else {
        return Ok(fixed);
    };
    if caller.snapshot(&repo).as_deref() != Some(before.as_str()) {
        return Ok(fixed);
    }
    let declined: Vec<&roles::Declined> = fixed
        .declined
        .iter()
        .filter(|d| !d.reason.trim().is_empty())
        .collect();
    let each = !blockers.is_empty()
        && blockers
            .iter()
            .all(|b| declined.iter().any(|d| d.finding.trim() == b));
    if !each {
        let how = if declined.is_empty() {
            "declined none of its blockers with a reason".to_string()
        } else {
            format!(
                "declined only {} of its {} blockers with a reason",
                declined.len(),
                blockers.len()
            )
        };
        return Err(no_work(
            caller,
            &call.label,
            &format!(
                "it finished with no change to the working tree and {how}, so they went unfixed"
            ),
        ));
    }
    caller.say(&format!(
        "{}: changed nothing, and declined each of its blockers: {}",
        call.label,
        declined
            .iter()
            .map(|d| format!("{}: {}", d.finding.trim(), d.reason.trim()))
            .collect::<Vec<_>>()
            .join("; ")
    ));
    Ok(fixed)
}

/// After a fix, every file it changed, said, and the ones a person should
/// see flagged, to keep or revert at the next gate: each binary, as git
/// tells, and each file no blocker the fix was given points at, by its file
/// or by naming it. Nothing is blocked or reverted for it. A file already
/// flagged for this round's fix keeps its flag and its answer.
fn flag_fix(
    run: &mut Run,
    caller: &dyn Caller,
    from: Option<&str>,
    folded: &[(String, Finding)],
    reported: &[Reported],
) {
    let Some(from) = from else {
        return;
    };
    let Some(changed) = caller
        .snapshot(&run.repo)
        .and_then(|to| caller.changed(&run.repo, from, &to))
    else {
        return;
    };
    let round = run.round;
    if changed.is_empty() {
        caller.say(&format!("round {round}'s fix changed no file"));
        return;
    }
    let norm = |p: &str| p.trim().trim_start_matches("./").to_string();
    let files: Vec<String> = folded
        .iter()
        .filter_map(|(_, f)| f.file.as_deref())
        .map(norm)
        .collect();
    let texts: Vec<&str> = folded
        .iter()
        .flat_map(|(_, f)| [f.title.as_str(), f.detail.as_str(), f.note.as_str()])
        .chain(reported.iter().map(|r| r.text.as_str()))
        .collect();
    let named =
        |path: &str| files.iter().any(|f| f == path) || texts.iter().any(|t| t.contains(path));
    let mut flagged = Vec::new();
    for c in &changed {
        let outside = !named(&c.path);
        if !(c.binary || outside) {
            continue;
        }
        if !run
            .flags
            .iter()
            .any(|f| f.round == round && f.path == c.path)
        {
            run.flags.push(Flag {
                round,
                path: c.path.clone(),
                binary: c.binary,
                outside,
                from: from.to_string(),
                existed: c.existed,
                command: super::change::revert_command(&run.repo, from, &c.path, c.existed),
                state: FlagState::Open,
            });
        }
        let flag = run
            .flags
            .iter()
            .find(|f| f.round == round && f.path == c.path)
            .expect("just pushed");
        flagged.push(format!("{} ({})", c.path, flag.why()));
    }
    let all: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    caller.say(&format!(
        "round {round}'s fix changed {} file(s): {}{}",
        all.len(),
        all.join(", "),
        if flagged.is_empty() {
            String::new()
        } else {
            format!(
                "; flagged for you to keep or revert at the next gate, and the report: {}",
                flagged.join(", ")
            )
        }
    ));
}

/// Reverts each flagged file a person chose to, as its command says. One
/// that can't be is flagged again, and escalated with its command.
fn revert_flagged(run: &mut Run, caller: &dyn Caller) {
    let repo = run.repo.clone();
    let mut failed = Vec::new();
    for f in run
        .flags
        .iter_mut()
        .filter(|f| f.state == FlagState::Reverting)
    {
        match caller.revert(&repo, &f.from, &f.path, f.existed) {
            Ok(()) => {
                f.state = FlagState::Reverted;
                caller.say(&format!(
                    "reverted {}, as you chose: it's as it was before round {}'s fix",
                    f.path, f.round
                ));
            }
            Err(e) => {
                f.state = FlagState::Open;
                failed.push(format!(
                    "{} couldn't be reverted ({e:#}): run `{}` yourself, or keep it",
                    f.path, f.command
                ));
            }
        }
    }
    for reason in failed {
        escalate(run, reason);
    }
}

/// What's to be fixed, as the fixer reads it: each finding with what a
/// person said of it, a finding that's the same defect as another listed
/// once, and the defects a person reported the reviewers missed.
fn fix_list(findings: &[(String, Finding)], reported: &[Reported]) -> String {
    let mut out = String::new();
    for (lens, f) in findings {
        if f.same_as
            .as_ref()
            .is_some_and(|p| findings.iter().any(|(_, g)| &g.id == p))
        {
            continue;
        }
        let at = if f.file.is_some() {
            place(f)
        } else {
            "somewhere in the change".to_string()
        };
        out.push_str(&format!("- {} ({at}): {}\n  {}\n", f.id, f.title, f.detail));
        let same: Vec<&(String, Finding)> = findings
            .iter()
            .filter(|(l, g)| g.same_as.as_deref() == Some(&f.id) && l != lens)
            .collect();
        if !same.is_empty() {
            let also: Vec<String> = same
                .iter()
                .map(|(l, g)| {
                    if place(g) == place(f) {
                        l.clone()
                    } else {
                        format!("{l} (at {})", place(g))
                    }
                })
                .collect();
            out.push_str(&format!(
                "  The same defect was found by: {}\n",
                also.join(", ")
            ));
            // One defect at more than one place: fixed at every one.
            let mut places = vec![place(f)];
            for (_, g) in &same {
                if !places.contains(&place(g)) {
                    places.push(place(g));
                }
            }
            if places.len() > 1 {
                out.push_str(&format!(
                    "  It shows at {} places; fix it at each: {}\n",
                    places.len(),
                    places.join(", ")
                ));
            }
        }
        if !f.note.is_empty() {
            out.push_str(&format!("  Your user says: {}\n", f.note));
        }
    }
    for r in reported {
        out.push_str(&format!(
            "- {} (your user found it; the reviewers missed it): {}\n",
            r.id, r.text
        ));
    }
    out
}

/// Every lens attacks again: what a person asks for, or what a defect
/// nobody's lens found needs, since any lens could own it.
/// After a fix, with `reattack: all`, every lens attacks the new version,
/// not only those that had blockers: a fix for one lens can break another.
/// Said, so a round's size isn't a surprise.
fn reattack(run: &mut Run, caller: &dyn Caller) {
    if run.reattacks() != Reattack::All {
        return;
    }
    let confirmed = run
        .lenses
        .values()
        .filter(|l| l.outcome == Outcome::Confirmed)
        .count();
    reopen_all(run);
    if confirmed > 0 {
        caller.say(&format!(
            "round {}'s fix: every lens attacks the new version, as reattack: all asks, {} \
             that had passed included",
            run.round, confirmed
        ));
    }
}

pub(super) fn reopen_all(run: &mut Run) {
    for lens in run.lenses.values_mut() {
        if lens.outcome == Outcome::Confirmed {
            lens.outcome = Outcome::Open;
        }
    }
}

/// The change is a new version after fixes. In a review to convergence, a
/// lens attacks it again if the fixes touched what it covers, with the
/// fixes as its focus; the rest wait for the sweep before it can end.
/// Without the checkout from before the fixes, every lens attacks again.
/// After a fix, where the next round's reviewers should look first: what
/// it changed, and what could feel it, from the round's change map. None
/// when the last round had no fix, or what it changed wasn't kept.
fn focus_note(run: &Run, s: &Setting) -> Option<String> {
    let fix = run.fix.as_ref().filter(|f| f.round + 1 == run.round)?;
    let diff = fs::read_to_string(s.work_dir.join(format!("round-{}.fix.diff", fix.round))).ok()?;
    let mut affects: BTreeSet<String> = BTreeSet::new();
    if let Some((facts, _)) = super::facts::kept(&s.work_dir, run.round) {
        for f in facts.current().filter(|f| fix.files.contains(&f.path)) {
            affects.extend(f.references.iter().cloned());
            affects.extend(f.tests.iter().cloned());
        }
    }
    const MOST: usize = 60;
    let more = affects.len().saturating_sub(MOST);
    let mut affects: Vec<String> = affects.into_iter().take(MOST).collect();
    if more > 0 {
        affects.push(format!("and {more} more in the change's map"));
    }
    Some(roles::focus_after_fix(
        fix.round, &fix.files, &diff, &affects,
    ))
}

/// What a fix changed, kept beside the round's diff for the next round's
/// focus and the blast-radius check, outside a review to convergence, where
/// `refocus` keeps it.
fn note_fix(run: &mut Run, s: &Setting, caller: &dyn Caller, before: Option<&str>) {
    let delta = before.and_then(|from| {
        let to = caller.snapshot(&run.repo)?;
        caller.delta(&run.repo, from, &to)
    });
    run.fix = delta.map(|(files, diff)| {
        let _ = fs::write(
            s.work_dir.join(format!("round-{}.fix.diff", run.round)),
            &diff,
        );
        FixDelta {
            round: run.round,
            files,
        }
    });
}

/// After a fix, before the next round: one small call, with no tools, on the
/// Sizer's binding, says whether each lens that passed before the fix must
/// attack again. A lens it says the fix can't touch, with a reason, skips
/// the round, and is reported as skipped, never as passing. A lens with
/// blockers of its own, one that failed, and anything the call can't settle
/// attacks as before.
fn blast_radius(run: &mut Run, s: &Setting, caller: &dyn Caller) {
    if !s.cfg.savings.blast_radius {
        return;
    }
    let Some(fix) = run.fix.clone().filter(|f| f.round + 1 == run.round) else {
        return;
    };
    let candidates: Vec<String> = run
        .lenses
        .values()
        .filter(|l| {
            l.outcome == Outcome::Open
                && l.latest().is_some_and(|r| {
                    r.round <= fix.round
                        && r.failures.is_empty()
                        && !r.findings.iter().any(Finding::blocks)
                })
        })
        .map(|l| l.id.clone())
        .collect();
    if candidates.is_empty() {
        return;
    }
    let diff = fs::read_to_string(s.work_dir.join(format!("round-{}.fix.diff", fix.round)))
        .unwrap_or_default();
    let lenses: Vec<roles::Candidate> = candidates
        .iter()
        .filter_map(|id| s.cfg.lenses.iter().find(|l| &l.id == id))
        .map(|l| roles::Candidate {
            id: &l.id,
            criteria: &l.criteria,
        })
        .collect();
    let prompt = roles::blast_radius_prompt(&fix.files, &diff, &lenses);
    let (Ok(name), Ok(binding)) = (
        s.cfg.binding_name(Role::Sizer, None),
        s.cfg.binding(Role::Sizer, None),
    ) else {
        caller.say("the blast-radius check has no Sizer binding, so every lens attacks");
        return;
    };
    let repo = run.reading().to_path_buf();
    let call = Call {
        role: Role::Sizer,
        binding_name: name,
        binding,
        prompt: &prompt,
        artifact: None,
        writes: false,
        no_tools: true,
        relay_tokens: None,
        repo: &repo,
        label: "the blast-radius check".into(),
    };
    let answer = match caller.call(&call) {
        Ok(answer) => answer,
        Err(e) => {
            caller.say(&format!(
                "the blast-radius check failed ({e:#}), so every lens attacks"
            ));
            return;
        }
    };
    record(run, s.cfg, &call, prompt.len(), &answer);
    let said = match roles::read_blast(&answer.text) {
        Ok(said) => said,
        Err(e) => {
            caller.say(&format!(
                "the blast-radius check's answer couldn't be read ({e:#}), so every lens attacks"
            ));
            return;
        }
    };
    for b in said {
        let reason = b.reason.trim();
        if b.attack || reason.is_empty() || !candidates.contains(&b.lens) {
            continue;
        }
        if let Some(l) = run.lenses.get_mut(&b.lens) {
            l.outcome = Outcome::Confirmed;
            l.skipped.push(Skipped {
                round: run.round,
                reason: reason.to_string(),
            });
            caller.say(&format!(
                "{}: skips round {}, by the blast-radius check: {reason}",
                b.lens, run.round
            ));
        }
    }
}

fn refocus(run: &mut Run, s: &Setting, caller: &dyn Caller, before: Option<&str>) {
    let delta = before.and_then(|from| {
        let to = caller.snapshot(&run.repo)?;
        caller.delta(&run.repo, from, &to)
    });
    run.sweep = false;
    match delta {
        Some((files, diff)) => {
            let _ = fs::write(
                s.work_dir.join(format!("round-{}.fix.diff", run.round)),
                &diff,
            );
            let touched = Change {
                files: files.clone(),
                diff,
                ..Change::default()
            };
            run.fix = Some(FixDelta {
                round: run.round,
                files,
            });
            for lens in run.lenses.values_mut() {
                let applies = s
                    .cfg
                    .lenses
                    .iter()
                    .find(|l| l.id == lens.id)
                    .and_then(|l| plan::lens_applies(l, &touched).ok().flatten())
                    .is_some();
                if lens.outcome == Outcome::Confirmed && applies {
                    lens.outcome = Outcome::Open;
                }
            }
        }
        None => {
            run.fix = None;
            reopen_all(run);
        }
    }
}

/// The blockers still to fix, by lens, from rounds no fix has had yet.
/// Whether the review now waits on its builder session to fix: what was
/// chosen to fix goes to the session, not the Orchestrator.
pub fn session_fixes(run: &Run) -> bool {
    run.fixer == Fixer::Session && !to_fix(run).is_empty()
}

fn to_fix(run: &Run) -> Vec<(String, Finding)> {
    let since = run.steelmans.last().map_or(0, |m| m.round);
    run.lenses
        .values()
        .filter_map(|l| l.latest().map(|r| (l.id.clone(), r)))
        .filter(|(_, r)| r.round > since)
        .flat_map(|(id, r)| {
            r.findings
                .iter()
                .filter(|f| f.state == FindingState::Folded)
                .map(move |f| (id.clone(), f.clone()))
        })
        .collect()
}

/// What the session that ran the review is to fix, as it reads it: less
/// what groups of the Orchestrator's fix already did, which it's told.
pub fn fix_brief(run: &Run) -> String {
    let partial = run
        .partial_fix
        .as_ref()
        .filter(|p| p.round == run.round && !p.groups.is_empty());
    let done =
        |id: &str| partial.is_some_and(|p| p.groups.iter().any(|g| g.ids.iter().any(|i| i == id)));
    let reported: Vec<Reported> = run
        .reported
        .iter()
        .filter(|r| !r.fixed && !done(&r.id))
        .cloned()
        .collect();
    let left: Vec<(String, Finding)> = to_fix(run)
        .into_iter()
        .filter(|(_, f)| !done(&f.id))
        .collect();
    let mut out = fix_list(&left, &reported);
    if let Some(p) = partial {
        out.push_str("\nAlready fixed by the Orchestrator, in the working tree:\n");
        for g in &p.groups {
            out.push_str(&format!("- {}: {}\n", g.ids.join(", "), g.done.trim()));
        }
    }
    out
}

/// The session that ran the review has fixed what was folded in, in the
/// working tree: the next round attacks the change as it is now, with the
/// session's briefs for its lenses, if it wrote any.
pub fn fixed_by_session(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    briefs: BTreeMap<String, String>,
) -> Result<()> {
    if run.waiting != Some(Gate::Fix) {
        bail!(
            "run {} isn't waiting for fixes{}",
            run.id,
            run.waiting
                .map(|g| format!(": it's waiting at its {} gate", g.as_str()))
                .unwrap_or_default()
        );
    }
    // A session that says it fixed, with the working tree as it was when
    // the fixes were asked for, did no work: refused, never taken for a fix.
    if let Some(before) = run.fixing.as_ref().and_then(|f| f.from.as_deref())
        && caller.snapshot(&run.repo).as_deref() == Some(before)
    {
        let line = "the session that ran the review: did no work: it said the fixes are in, but \
                    the working tree is as it was when they were asked for";
        caller.say(line);
        bail!(
            "{line}. Fold them into the working tree, then resume with --fixed again, or end \
             the review with --stop"
        );
    }
    let folded = to_fix(run);
    let done = "fixed by the session that ran the review".to_string();
    for (id, _) in &folded {
        if let Some(lens) = run.lenses.get_mut(id)
            && !lens.done.contains(&done)
        {
            lens.done.push(done.clone());
        }
    }
    let unfixed: Vec<Reported> = run.reported.iter().filter(|r| !r.fixed).cloned().collect();
    let reported = !unfixed.is_empty();
    for r in run.reported.iter_mut() {
        r.fixed = true;
    }
    // From before the Orchestrator's first group, if some were done.
    let carried = run
        .partial_fix
        .take()
        .filter(|p| p.round == run.round)
        .and_then(|p| p.from);
    let from = carried.or_else(|| run.fixing.take().and_then(|f| f.from));
    run.fixing = None;
    flag_fix(run, caller, from.as_deref(), &folded, &unfixed);
    if run.converge {
        run.steelmans.push(Steelman {
            round: run.round,
            blockers: folded.iter().map(|(_, f)| f.id.clone()).collect(),
            done,
            declined: Vec::new(),
            pins: Vec::new(),
            tests_done: String::new(),
            briefs,
        });
        refocus(run, s, caller, from.as_deref());
    } else {
        if s.cfg.savings.focus_after_fix || s.cfg.savings.blast_radius {
            note_fix(run, s, caller, from.as_deref());
        }
        if reported {
            reopen_all(run);
        }
        // Its briefs go to the next round's reviewers, as a steelman's do
        // in a review to convergence.
        run.fix_briefs = (!briefs.is_empty()).then(|| FixBriefs {
            round: run.round + 1,
            briefs,
        });
    }
    reattack(run, caller);
    run.round += 1;
    if run.reattacks() == Reattack::Blocked {
        blast_radius(run, s, caller);
    }
    run.stage = Stage::Review;
    run.status = Status::Running;
    run.waiting = None;
    Ok(())
}

fn unseen(run: &Run) -> bool {
    run.escalations.iter().any(|e| !e.seen)
}

fn pause(run: &mut Run, gate: Gate) {
    run.status = Status::Waiting;
    run.waiting = Some(gate);
}

/// Carries a review on until it ends or needs a person. An escalation
/// always stops it, whatever the gate. `reload` gets the change as it
/// stands, which is the working tree once there are fixes in it.
pub fn drive(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    reload: &dyn Fn(bool) -> Result<Change>,
) -> Result<()> {
    run.status = Status::Running;
    run.waiting = None;
    loop {
        let known = failures(run).len();
        let stepped = step(run, s, caller, reload)?;
        if s.stop_on_failure {
            let now = failures(run);
            if now.len() > known {
                run.status = Status::Stopped;
                bail!(
                    "stopped at the first failure, as asked:\n- {}",
                    now[known..].join("\n- ")
                );
            }
        }
        if !stepped {
            return Ok(());
        }
    }
}

/// Everything that didn't finish its work in a run so far, lens by lens and
/// round by round: what `stop_on_failure` stops at.
pub fn failures(run: &Run) -> Vec<String> {
    let mut out = Vec::new();
    for lens in run.lenses.values() {
        for round in &lens.rounds {
            let notes = round.notes.iter().filter(|n| n.starts_with("no brief:"));
            for f in round.failures.iter().chain(notes) {
                out.push(format!("{}, round {}: {f}", lens.id, round.round));
            }
        }
    }
    out
}

/// One step of a run: false once it waits for a person or is done.
fn step(
    run: &mut Run,
    s: &Setting,
    caller: &dyn Caller,
    reload: &dyn Fn(bool) -> Result<Change>,
) -> Result<bool> {
    // What a person chose to revert, before anything reads the checkout.
    revert_flagged(run, caller);
    if unseen(run) {
        pause(run, Gate::Escalation);
        return Ok(false);
    }
    match run.stage {
        Stage::Scope => {
            // A tier raised at the gate brings its lenses, which are asked
            // about before any reviewer runs.
            if run.rescope {
                let change = reload(false)?;
                let artifacts = write_artifact(s, run, &change, 0)?;
                choose_lenses(run, s, &change, caller, &artifacts.diff)?;
                run.rescope = false;
                if unseen(run) {
                    pause(run, Gate::Escalation);
                    return Ok(false);
                }
            }
            pause(run, Gate::Scope);
            return Ok(false);
        }
        Stage::Review => {
            let fixed = run.lenses.values().any(|l| !l.done.is_empty());
            let open: Vec<&str> = run
                .lenses
                .values()
                .filter(|l| l.outcome == Outcome::Open)
                .map(|l| l.id.as_str())
                .collect();
            caller.note(&format!(
                "round {}: {} lens(es) attack the change{}: {}",
                run.round,
                open.len(),
                if fixed { " as fixed" } else { "" },
                open.join(", ")
            ));
            review_round(run, s, &reload(fixed)?, caller)?;
        }
        Stage::Decide => {
            pause(run, Gate::Fold);
            return Ok(false);
        }
        Stage::Fix if run.fixer == Fixer::Session => run.stage = Stage::Fixing,
        Stage::Fix => {
            if !ready_to_fix(run, s) {
                pause(run, Gate::Escalation);
                return Ok(false);
            }
            fix(run, s, caller, reload)?
        }
        Stage::Fixing => {
            if run.fixing.is_none() {
                if !ready_to_fix(run, s) {
                    pause(run, Gate::Escalation);
                    return Ok(false);
                }
                run.fixing = Some(Fixing {
                    from: caller.snapshot(&run.repo),
                });
            }
            pause(run, Gate::Fix);
            return Ok(false);
        }
        Stage::Final => {
            pause(run, Gate::Final);
            return Ok(false);
        }
        Stage::Done => {
            run.status = if all_settled(run) && !run.lenses.is_empty() {
                Status::Confirmed
            } else {
                Status::Stopped
            };
            return Ok(false);
        }
    }
    Ok(true)
}

/// Applies a person's answer at a gate.
/// Ends a review where it stands, waiting or not: what's left undecided
/// stays as it is, and the next step records it as stopped.
pub fn end_here(run: &mut Run) {
    for e in &mut run.escalations {
        e.seen = true;
    }
    run.stage = Stage::Done;
}

pub fn decide(run: &mut Run, decision: &Decision) -> Result<()> {
    if decision.stop {
        end_here(run);
        return Ok(());
    }
    let Some(gate) = run.waiting else {
        bail!("run {} isn't waiting for anyone", run.id);
    };
    match gate {
        Gate::Escalation | Gate::Scope | Gate::Final => {
            if !decision.approve {
                bail!("this needs --approve or --stop");
            }
            for e in &mut run.escalations {
                e.seen = true;
            }
            match run.stage {
                Stage::Scope => run.stage = Stage::Review,
                Stage::Final => run.stage = Stage::Done,
                _ => {}
            }
        }
        Gate::Fold => decide_blockers(run, decision)?,
        Gate::Fix => bail!(
            "this waits for the fixes: once they're in the working tree, resume with --fixed, \
             or --stop"
        ),
    }
    Ok(())
}

/// Every blocker must be folded in, waived or deferred to a ticket, and
/// only what blocks unless waived can be waived.
fn decide_blockers(run: &mut Run, decision: &Decision) -> Result<()> {
    let open: Vec<String> = run
        .lenses
        .values()
        .flat_map(|l| l.blockers())
        .map(|f| f.id.clone())
        .collect();
    let named = |id: &String| {
        decision.fold.contains(id) || decision.waive.contains(id) || decision.defer.contains(id)
    };
    if let Some(id) = open.iter().find(|id| !named(id)) {
        bail!("{id} needs --fold, --waive or --defer");
    }
    if let Some(id) = decision
        .fold
        .iter()
        .chain(&decision.waive)
        .chain(&decision.defer)
        .find(|id| !open.contains(id))
    {
        bail!("{id} isn't an open blocker");
    }
    let mut folding = false;
    for lens in run.lenses.values_mut() {
        let Some(round) = lens.rounds.last_mut() else {
            continue;
        };
        for f in round.findings.iter_mut() {
            if decision.defer.contains(&f.id) {
                f.state = FindingState::Deferred;
                f.reason.push_str(" A person deferred it to a ticket.");
            } else if decision.waive.contains(&f.id) {
                if f.action != Action::BlocksUnlessWaived {
                    bail!(
                        "{} is {:?}, which can't be waived: fold it in, or defer it to a ticket",
                        f.id,
                        f.severity
                    );
                }
                f.state = FindingState::Waived;
            } else if decision.fold.contains(&f.id) {
                f.state = FindingState::Folded;
                folding = true;
            }
        }
        let open_left = round
            .findings
            .iter()
            .any(|f| f.blocks() && matches!(f.state, FindingState::Open | FindingState::Folded));
        if lens.outcome == Outcome::Open && !open_left && round.failures.is_empty() {
            lens.outcome = Outcome::Waived;
        }
    }
    run.stage = if folding { Stage::Fix } else { Stage::Done };
    Ok(())
}

/// The blockers a benchmark knows are in a commit.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Known {
    pub id: String,
    #[serde(default)]
    pub file: Option<String>,
    /// A pattern a finding's title or detail must match.
    pub pattern: String,
}

#[derive(Debug, Serialize)]
pub struct Score {
    pub caught: Vec<String>,
    pub missed: Vec<String>,
    /// Blocking findings that match nothing known.
    pub false_findings: Vec<String>,
    pub usd: Option<f64>,
    pub tokens: u64,
    pub failures: Vec<String>,
}

/// How a run's first round did against what's known to be there.
pub fn score(run: &Run, known: &[Known]) -> Result<Score> {
    let findings: Vec<&Finding> = run
        .lenses
        .values()
        .flat_map(|l| l.rounds.iter())
        .flat_map(|r| r.findings.iter())
        .filter(|f| f.blocks())
        .collect();
    let mut caught = Vec::new();
    let mut missed = Vec::new();
    let mut matched = Vec::new();
    for k in known {
        let re = regex::Regex::new(&k.pattern)?;
        let hit: Vec<&&Finding> = findings
            .iter()
            .filter(|f| {
                let file = k.file.as_ref().is_none_or(|p| f.file.as_ref() == Some(p));
                file && (re.is_match(&f.title) || re.is_match(&f.detail))
            })
            .collect();
        if hit.is_empty() {
            missed.push(k.id.clone());
        } else {
            caught.push(k.id.clone());
            matched.extend(hit.iter().map(|f| f.id.clone()));
        }
    }
    let false_findings = findings
        .iter()
        .filter(|f| !matched.contains(&f.id))
        .map(|f| format!("{}: {}", f.id, f.title))
        .collect();
    Ok(Score {
        caught,
        missed,
        false_findings,
        usd: dollars(&run.usage),
        tokens: run.usage.iter().map(Usage::tokens).sum(),
        failures: run
            .lenses
            .values()
            .flat_map(|l| l.rounds.iter())
            .flat_map(|r| r.failures.iter().cloned())
            .collect(),
    })
}

/// Reads a repository's config and decided non-issues from its base.
pub fn from_base(repo: &Path, base: &str, path: Option<&str>) -> Result<String> {
    Ok(match path {
        Some(path) => change::at_base(repo, base, path)?.unwrap_or_default(),
        None => String::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::config::{Flags, parse_repo, parse_user, resolve};
    use super::super::report;
    use super::*;
    use crate::util::new_id;
    use std::time::{Duration, Instant};

    const USER: &str = include_str!("../../examples/acr/user.yaml");
    const NOTHING: &str = r#"{"lenses": [], "escalate": []}"#;
    const CLEAN: &str = r#"{"findings": []}"#;
    const ONE: &str = r#"{"findings": [{"title": "a defect", "detail": "why", "file": "a.src"}]}"#;

    /// A small repository's config: two lenses, and two tiers.
    fn config(gate: &str, rounds: u32, fan_out: u32, extra: &str, top: &str) -> Config {
        let text = format!(
            r#"
version: 1
human_gate: {gate}
max_rounds: {rounds}
{top}
tiers:
  - {{id: small, match: {{description: "small things"}}, fan_out: 1}}
  - {{id: big, match: {{paths: ["**/*.src"], description: "big things"}}, fan_out: {fan_out}}}
lenses:
  - {{id: alpha, criteria: "alpha criteria", triggers: {{paths: ["**/*.src"]}}}}
  - {{id: beta, criteria: "beta criteria", triggers: {{paths: ["**/*.src"]}}}}
{extra}
"#
        );
        let user = parse_user(USER).unwrap();
        let mut cfg = resolve(&user, parse_repo(&text).unwrap(), &Flags::default()).unwrap();
        // These tests count lenses and calls: the universal lens, which
        // every real config gets, is tested on its own.
        cfg.lenses
            .retain(|l| l.id != super::super::config::ELEGANCE);
        // Followers start at once, unless a test asks for a stagger.
        if !top.contains("stagger_secs") {
            cfg.facts.stagger_secs = 0;
        }
        cfg
    }

    fn plain(gate: &str, rounds: u32, fan_out: u32) -> Config {
        config(gate, rounds, fan_out, "", "")
    }

    fn change(files: &[&str]) -> Change {
        Change {
            files: files.iter().map(|f| f.to_string()).collect(),
            diff: "+let x = 1;\n".to_string(),
            ..Change::default()
        }
    }

    /// Answers each call from a function, and keeps a record of the calls.
    struct Fake<F> {
        answer: F,
        calls: Mutex<Vec<(Role, String, bool, String)>>,
        /// What every call says it used, as a bridge with `usage` would.
        usage: Option<RunUsage>,
        /// A role whose calls change the checkout, which none may but an
        /// Orchestrator fixing.
        tamper: Option<Role>,
        changes: std::sync::atomic::AtomicU32,
        /// The roles that got the change as a file.
        with_diff: Mutex<Vec<Role>>,
        /// What each of them got in it.
        sent: Mutex<Vec<(Role, String)>>,
        /// Where each call worked, and whether the change's file was there.
        repos: Mutex<Vec<(Role, PathBuf, bool)>>,
        /// Whether each call's working directory held what a setup leaves.
        warmed: Mutex<Vec<bool>>,
        /// What a fix changes, as the checkout's snapshots would say.
        fixes: Option<Vec<String>>,
        /// A file every prober writes in its copy.
        probe_file: Option<&'static str>,
        /// Whether each prober's copy held that file when it started.
        probed: Mutex<Vec<bool>>,
        /// A binding whose every call runs out of time.
        slow: Option<String>,
        /// Each call's label, and each note, in order.
        told: Mutex<Vec<String>>,
        /// The roles of the calls made with no tools.
        bare: Mutex<Vec<Role>>,
        /// Each call's role and where it was asked to hand off.
        relays: Mutex<Vec<(Role, Option<u64>)>>,
        /// What a fix changed, file by file, as the checkout would say.
        changed: Option<Vec<super::super::change::Changed>>,
        /// Each file reverted, from where, and whether it was there.
        reverted: Mutex<Vec<(String, String, bool)>>,
        /// Reverting fails.
        stuck: bool,
        /// The checkout never changes, as its snapshots would say: a fixer
        /// that did nothing.
        still: bool,
        /// The labels of calls restarted with a new cache key.
        rekeyed: Mutex<Vec<String>>,
        /// The checkout reads "base" until a fixer's first call, then
        /// "edited": a fix's edits, left whatever its answer.
        edits_once: bool,
        edited: std::sync::atomic::AtomicBool,
    }

    impl<F: Fn(Role, &str) -> Result<String> + Sync> Fake<F> {
        fn new(answer: F) -> Fake<F> {
            Fake {
                answer,
                calls: Mutex::new(Vec::new()),
                usage: None,
                tamper: None,
                changes: Default::default(),
                with_diff: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
                repos: Mutex::new(Vec::new()),
                warmed: Mutex::new(Vec::new()),
                fixes: None,
                probe_file: None,
                probed: Mutex::new(Vec::new()),
                slow: None,
                told: Mutex::new(Vec::new()),
                bare: Mutex::new(Vec::new()),
                relays: Mutex::new(Vec::new()),
                changed: None,
                reverted: Mutex::new(Vec::new()),
                stuck: false,
                still: false,
                rekeyed: Mutex::new(Vec::new()),
                edits_once: false,
                edited: Default::default(),
            }
        }

        fn count(&self, role: Role) -> usize {
            let calls = self.calls.lock().unwrap();
            calls.iter().filter(|c| c.0 == role).count()
        }
    }

    impl<F: Fn(Role, &str) -> Result<String> + Sync> Caller for Fake<F> {
        fn call(&self, call: &Call) -> Result<Answer> {
            if self.edits_once && call.writes {
                self.edited.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.calls.lock().unwrap().push((
                call.role,
                call.binding_name.to_string(),
                call.writes,
                call.prompt.to_string(),
            ));
            self.told
                .lock()
                .unwrap()
                .push(format!("call: {}", call.label));
            self.relays
                .lock()
                .unwrap()
                .push((call.role, call.relay_tokens));
            if call.no_tools {
                self.bare.lock().unwrap().push(call.role);
            }
            if let Some(path) = call.artifact {
                self.with_diff.lock().unwrap().push(call.role);
                let text = fs::read_to_string(path).unwrap_or_default();
                self.sent.lock().unwrap().push((call.role, text));
            }
            self.repos.lock().unwrap().push((
                call.role,
                call.repo.to_path_buf(),
                call.repo.join("a.src").is_file(),
            ));
            if call.role == Role::Reviewer {
                self.warmed
                    .lock()
                    .unwrap()
                    .push(call.repo.join("warmed").is_file());
            }
            if self.slow.as_deref() == Some(call.binding_name) {
                bail!("{} timeout: it ran out of time", call.binding_name);
            }
            if let Some(file) = self.probe_file.filter(|_| {
                call.role == Role::Reviewer && call.prompt.contains("You probe a change")
            }) {
                let path = call.repo.join(file);
                self.probed.lock().unwrap().push(path.is_file());
                fs::write(path, "true\n").unwrap();
            }
            if self.tamper == Some(call.role) {
                self.changes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            let text = (self.answer)(call.role, call.prompt)?;
            Ok(Answer {
                text,
                run_id: new_id(),
                usage: self.usage.clone(),
                secs: None,
            })
        }

        fn note(&self, what: &str) {
            self.told.lock().unwrap().push(format!("note: {what}"));
        }

        fn say(&self, what: &str) {
            self.told.lock().unwrap().push(format!("say: {what}"));
        }

        fn rekey(&self, label: &str) {
            self.rekeyed.lock().unwrap().push(label.to_string());
        }

        fn snapshot(&self, _repo: &Path) -> Option<String> {
            if self.still {
                return Some("the same tree".into());
            }
            if self.edits_once {
                let edited = self.edited.load(std::sync::atomic::Ordering::SeqCst);
                return Some(if edited { "edited" } else { "base" }.into());
            }
            (self.fixes.is_some() || self.changed.is_some()).then(new_id)
        }

        fn changed(
            &self,
            _repo: &Path,
            _from: &str,
            _to: &str,
        ) -> Option<Vec<super::super::change::Changed>> {
            self.changed.clone()
        }

        fn revert(&self, _repo: &Path, from: &str, path: &str, existed: bool) -> Result<()> {
            if self.stuck {
                bail!("the file is locked");
            }
            self.reverted
                .lock()
                .unwrap()
                .push((path.to_string(), from.to_string(), existed));
            Ok(())
        }

        fn delta(&self, _repo: &Path, _from: &str, _to: &str) -> Option<(Vec<String>, String)> {
            self.fixes
                .clone()
                .map(|files| (files, "+the fix\n".to_string()))
        }

        fn fingerprint(&self, _repo: &Path) -> Option<String> {
            self.tamper.map(|_| {
                self.changes
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .to_string()
            })
        }
    }

    #[test]
    fn a_call_its_agent_counted_is_recorded_as_counted() {
        let prices = "costs: {prices: {example/small-model: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}, \
                      example/medium-model: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}, \
                      example/large-model: {input_per_mtok: 1, output_per_mtok: 2, cached_input_per_mtok: 0.1}}, \
                      per_call: {reviewer: {input_tokens: 99999}}}";
        let cfg = config("exception", 1, 1, "", prices);
        let mut fake = Fake::new(defect_in("alpha", 99));
        fake.usage = Some(RunUsage {
            calls: 4,
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 1_000_000,
            usd: Some(9.0),
            ..RunUsage::default()
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(!run.usage.is_empty());
        for u in &run.usage {
            assert!(u.measured);
            // The agent's count, never the config's guess on top of it.
            assert_eq!(
                (u.tokens_in, u.cache_read, u.tokens_out),
                (1_000_000, 1_000_000, 1_000_000)
            );
            assert!((u.usd.unwrap() - 3.1).abs() < 1e-9, "priced by the config");
            assert_eq!(u.agent_usd, Some(9.0));
        }
        let json = serde_json::to_value(report::report(&run, &cfg)).unwrap();
        assert_eq!(json["calls_measured"], run.usage.len());
        assert_eq!(json["calls_estimated"], 0);
        let calls = run.usage.len() as f64;
        assert!((json["agent_usd"].as_f64().unwrap() - 9.0 * calls).abs() < 1e-9);
        // On a subscription, neither the config's price nor the agent's
        // counts: only tokens.
        let mut subscribed = cfg.clone();
        for b in subscribed.allowed_bindings.values_mut() {
            b.subscription = true;
        }
        let (run, _) = go(&subscribed, &["a.src"], &fake);
        assert!(
            run.usage
                .iter()
                .all(|u| u.subscription && u.usd.is_none() && u.agent_usd.is_none())
        );
        assert!(run.usage.iter().all(|u| u.tokens() == 3_000_000));
        let json = serde_json::to_value(report::report(&run, &subscribed)).unwrap();
        assert_eq!(json["usd"], serde_json::Value::Null);
        assert_eq!(json["calls_on_subscription"], run.usage.len());
        // With no price in the config, the agent's own is used.
        let bare = plain("exception", 1, 1);
        let (run, _) = go(&bare, &["a.src"], &fake);
        assert!(run.usage.iter().all(|u| u.usd == Some(9.0)));
    }

    fn rating(severity: &str) -> String {
        format!(
            r#"{{"findings": [{{"title": "a defect", "file": "a.src", "severity": "{severity}"}}]}}"#
        )
    }

    fn setting(cfg: &Config) -> Setting<'_> {
        Setting {
            cfg,
            not_findings: "- the decided thing".into(),
            work_dir: std::env::temp_dir().join(format!("tb-acr-run-{}", new_id())),
            description: None,
            converge: false,
            stop_on_failure: false,
            outage_wait_secs: 1,
            copies: None,
            head_copy: None,
            pr: None,
            learned: None,
            builder_brief: None,
        }
    }

    fn go<F: Fn(Role, &str) -> Result<String> + Sync>(
        cfg: &Config,
        files: &[&str],
        fake: &Fake<F>,
    ) -> (Run, Change) {
        let s = setting(cfg);
        let c = change(files);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, fake, &|_| Ok(again.clone())).unwrap();
        (run, c)
    }

    fn resume<F: Fn(Role, &str) -> Result<String> + Sync>(
        run: &mut Run,
        cfg: &Config,
        c: &Change,
        fake: &Fake<F>,
        decision: Decision,
    ) -> Result<()> {
        decide(run, &decision)?;
        let s = setting(cfg);
        let again = c.clone();
        drive(run, &s, fake, &|_| Ok(again.clone()))
    }

    /// Reviewers find a defect in `lens` until it's been fixed enough times.
    fn defect_in(
        lens: &'static str,
        fixes_needed: usize,
    ) -> impl Fn(Role, &str) -> Result<String> + Sync {
        let fixes = Mutex::new(0);
        move |role, prompt| {
            Ok(match role {
                Role::Reviewer
                    if prompt.contains(lens) && *fixes.lock().unwrap() < fixes_needed =>
                {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("Fix these") => {
                    *fixes.lock().unwrap() += 1;
                    r#"{"done": "fixed it"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        }
    }

    #[test]
    fn a_clean_change_is_confirmed_in_one_round() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 1);
        assert!(run.lenses.values().all(|l| l.outcome == Outcome::Confirmed));
        assert_eq!(fake.count(Role::Reviewer), 2);
        assert_eq!(fake.count(Role::Judge), 0, "nothing to judge");
    }

    #[test]
    fn a_lens_that_takes_generated_files_gets_the_whole_diff() {
        let cfg = plain("exception", 1, 1);
        let s = setting(&cfg);
        let mut c = change(&["a.src"]);
        let fake = Fake::new(defect_in("alpha", 99));
        let run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let lean = write_artifact(&s, &run, &c, 1).unwrap();
        assert!(lean.full.is_none());
        c.full = Some("the whole diff".into());
        let both = write_artifact(&s, &run, &c, 2).unwrap();
        let mut lens = cfg.lenses[0].clone();
        assert_eq!(both.for_lens(&lens), both.diff.as_path());
        lens.include_generated = true;
        let whole = both.for_lens(&lens);
        assert!(
            fs::read_to_string(whole)
                .unwrap()
                .ends_with("# The diff\n\nthe whole diff")
        );
    }

    #[test]
    fn every_role_gets_what_the_change_says_its_for_ahead_of_the_diff() {
        let cfg = plain("exception", 1, 1);
        let mut s = setting(&cfg);
        s.description = Some("Fixes B-1: an edited reminder keeps the user's date.".into());
        let mut c = change(&["a.src"]);
        c.commits = vec![super::super::change::Commit {
            id: "abc1234".into(),
            message: "Keep the edited date\n\nThe save path dropped it.".into(),
        }];
        let fake = Fake::new(defect_in("alpha", 0));
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, &fake, &|_| Ok(again.clone())).unwrap();
        // Kept, so a resumed review sends the same.
        assert_eq!(run.description, s.description);
        let sent = fake.sent.lock().unwrap();
        assert!(sent.iter().any(|(r, _)| *r == Role::Reviewer));
        for (role, text) in sent.iter() {
            assert!(
                text.starts_with(super::super::change::SAID),
                "{role:?}: {text}"
            );
            let said = text
                .find("an edited reminder keeps the user's date")
                .unwrap();
            let commit = text.find("Keep the edited date").unwrap();
            let diff = text.find("# The diff\n\n+let x = 1;").unwrap();
            assert!(said < commit && commit < diff, "{role:?}: {text}");
        }
    }

    #[test]
    fn a_judge_gets_the_diff_and_may_not_change_the_checkout() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 9));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(fake.with_diff.lock().unwrap().contains(&Role::Judge));
        assert!(
            run.escalations
                .iter()
                .all(|e| !e.reason.contains("changed"))
        );
        // A Judge that writes to the repository, rather than its scratch
        // space, fails the round and escalates.
        let mut tampering = Fake::new(defect_in("alpha", 9));
        tampering.tamper = Some(Role::Judge);
        let (run, _) = go(&cfg, &["a.src"], &tampering);
        let alpha = &run.lenses["alpha"];
        assert_eq!(alpha.outcome, Outcome::Open);
        assert!(
            alpha.rounds[0]
                .failures
                .iter()
                .any(|f| f.contains("only an Orchestrator"))
        );
        assert!(
            run.escalations
                .iter()
                .any(|e| e.reason.contains("only an Orchestrator"))
        );
    }

    #[test]
    fn each_copy_gets_a_folder_no_earlier_copy_left() {
        let root = Path::new("/state/acr-copies/r1-r2");
        let (a, b) = (
            Copies::fresh(Some(root.join("gate"))),
            Copies::fresh(Some(root.join("gate"))),
        );
        let (a, b) = (a.0.unwrap(), b.0.unwrap());
        assert_ne!(a, b);
        for dir in [&a, &b] {
            assert_eq!(dir.parent(), Some(root));
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            assert!(crate::util::unstamped(&name).starts_with("gate-"), "{name}");
            assert_ne!(
                crate::util::unstamped(&name),
                name,
                "it starts with its time"
            );
        }
        assert!(Copies::fresh(None).0.is_none());
    }

    #[test]
    fn a_finished_review_is_noted_once_and_kept_to_be_looked_at() {
        let cfg = plain("exception", 3, 1);
        let (run, _) = go(&cfg, &["a.src"], &Fake::new(defect_in("alpha", 1)));
        let home = std::env::temp_dir().join(format!("tb-done-{}", &new_id()[14..]));
        let paths = crate::config::Paths {
            config: home.join("config.toml"),
            state: home.clone(),
        };
        let review = super::super::keep::dir(&paths).join(&run.id);
        fs::create_dir_all(&review).unwrap();
        fs::write(review.join("round-1.diff"), "+x").unwrap();
        fs::write(review.join("run.json"), "{}").unwrap();
        super::super::keep::finished(&paths, &run, true).unwrap();
        super::super::keep::finished(&paths, &run, true).unwrap();
        // Kept, packed once, under the time it finished.
        let kept = super::super::keep::kept(&paths, &run.id).unwrap();
        let name = kept.file_name().unwrap().to_string_lossy().into_owned();
        let base = name.trim_end_matches(".tar.gz");
        assert_eq!(
            crate::util::packed_file(&kept, &format!("{base}/round-1.diff")).unwrap(),
            b"+x"
        );
        assert!(!review.exists());
        let archives = fs::read_dir(super::super::keep::dir(&paths))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tar.gz")
            })
            .count();
        assert_eq!(archives, 1, "packed once");
        let noted = super::super::keep::history(&paths, &run.repo);
        assert_eq!(noted.len(), 1, "noted once");
        assert_eq!(noted[0].lenses["alpha"], 2);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn history_is_the_rounds_finished_reviews_took() {
        let prices = "costs: {prices: {example/small-model: {input_per_mtok: 1, output_per_mtok: 2}, \
                      example/medium-model: {input_per_mtok: 1, output_per_mtok: 2}, \
                      example/large-model: {input_per_mtok: 1, output_per_mtok: 2}}}";
        let cfg = config("exception", 3, 1, "", prices);
        let fake = Fake::new(defect_in("alpha", 1));
        let (done, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(done.status, Status::Confirmed);
        let waiting = config("per-round", 3, 1, "", "");
        let (unfinished, _) = go(&waiting, &["a.src"], &Fake::new(defect_in("alpha", 9)));
        assert_eq!(unfinished.status, Status::Waiting);
        assert!(plan::Record::of(&unfinished).is_none());
        let record = plan::Record::of(&done).unwrap();
        assert_eq!(record.lenses["alpha"], 2);
        assert_eq!(record.lenses["beta"], 1);
        assert_eq!(record.fixes, 1);
        let history = plan::History::of([record]);
        assert_eq!(history.runs, 1, "only a finished review counts");
        let plan = plan::dry_run(&cfg, &change(&["a.src"]), &history).unwrap();
        let (low, expected, worst) = (
            plan.least.usd.unwrap(),
            plan.expected.unwrap().usd.unwrap(),
            plan.most.usd.unwrap(),
        );
        assert!(
            low < expected && expected < worst,
            "{low} {expected} {worst}"
        );
        assert_eq!(plan.history_runs, 1);
    }

    #[test]
    fn only_lenses_with_blockers_are_attacked_again_by_new_reviewers() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 2);
        // Both in the first round, and only alpha in the second.
        assert_eq!(fake.count(Role::Reviewer), 3);
        assert_eq!(run.lenses["alpha"].rounds.len(), 2);
        assert_eq!(run.lenses["beta"].rounds.len(), 1);
        assert_eq!(run.lenses["alpha"].done, ["fixed it"]);
        let first = &run.lenses["alpha"].rounds[0].findings[0];
        assert_eq!(first.state, FindingState::Folded);
        let calls = fake.calls.lock().unwrap();
        let fix = calls.iter().find(|c| c.3.contains("Fix these")).unwrap();
        assert_eq!(fix.0, Role::Orchestrator);
        assert!(fix.2, "the Orchestrator's fix writes");
        let others = calls.iter().filter(|c| !c.3.contains("Fix these"));
        assert!(others.into_iter().all(|c| !c.2), "nothing else may write");
    }

    /// A review to convergence of a change to a.src.
    fn converging<F: Fn(Role, &str) -> Result<String> + Sync>(cfg: &Config, fake: &Fake<F>) -> Run {
        let mut s = setting(cfg);
        s.converge = true;
        s.description = Some("Makes x one.".into());
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, fake, &|_| Ok(again.clone())).unwrap();
        run
    }

    #[test]
    fn a_review_to_convergence_attacks_every_lens_after_each_steelman() {
        let cfg = plain("exception", 20, 1);
        let fixes = Mutex::new(0);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && *fixes.lock().unwrap() < 2 => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixes.lock().unwrap() += 1;
                    r#"{"done": "made it stronger"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let run = converging(&cfg, &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 3);
        // beta had nothing to fix, but the change changed under it twice.
        assert_eq!(fake.count(Role::Reviewer), 6);
        assert_eq!(run.lenses["beta"].rounds.len(), 3);
        assert_eq!(run.steelmans.len(), 2);
        assert_eq!(run.steelmans[0].blockers, ["alpha-r1-1"]);
        assert_eq!(run.steelmans[1].round, 2);
        let calls = fake.calls.lock().unwrap();
        let steelmans: Vec<_> = calls
            .iter()
            .filter(|c| c.3.contains("You synthesize"))
            .collect();
        assert_eq!(steelmans.len(), 2);
        assert!(
            steelmans.iter().all(|c| c.0 == Role::Orchestrator && c.2),
            "it writes"
        );
        // It holds the thread: what the change is for, and what came before.
        assert!(steelmans[0].3.contains("Makes x one."));
        assert!(steelmans[0].3.contains("Nothing yet"));
        assert!(steelmans[1].3.contains("After round 1: made it stronger"));
        assert!(!calls.iter().any(|c| c.3.contains("Fix these")));
        drop(calls);
        // Each round's tokens are kept apart.
        assert!(run.usage.iter().any(|u| u.round == 3));
    }

    #[test]
    fn a_writer_pins_the_fixes_and_each_pin_must_fail_before_and_pass_after() {
        use super::super::change::tests::repo_with;
        let top = "stages: [test-writing]\ntests: {command: 'sh {tests}'}";
        let cfg = config("exception", 20, 1, "", top);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-pins-{}", new_id()));
        let mut s = setting(&cfg);
        s.converge = true;
        s.copies = Some(root.clone());
        let fixed = Mutex::new(false);
        let checkout = repo.clone();
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    assert!(prompt.contains("Leave the tests that pin these fixes"));
                    fs::write(checkout.join("a.src"), "let x = 2;\n").unwrap();
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "x is two now"}"#.to_string()
                }
                Role::Orchestrator if prompt.contains("You write tests for a change") => {
                    assert!(prompt.contains("alpha-r1-1"));
                    fs::write(checkout.join("pin.sh"), "grep -q 'x = 2' a.src\n").unwrap();
                    fs::write(checkout.join("idle.sh"), "true\n").unwrap();
                    r#"{"tests": [{"file": "pin.sh", "pins": ["alpha-r1-1"]}, {"file": "idle.sh", "pins": ["alpha-r1-1"]}], "done": "pinned"}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        // The pin that pins nothing stops the review for a person; with
        // nobody in the loop, it's taken as seen and the review goes on.
        assert_eq!(run.waiting, Some(Gate::Escalation));
        for e in &mut run.escalations {
            e.seen = true;
        }
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(run.status, Status::Confirmed);
        let m = &run.steelmans[0];
        assert_eq!(m.tests_done, "pinned");
        let pin = |f: &str| m.pins.iter().find(|p| p.file == f).unwrap().clone();
        assert_eq!(pin("pin.sh").fails_before, Some(true));
        assert_eq!(pin("pin.sh").passes_after, Some(true));
        // A test that passes on the old code pins nothing, and a person hears.
        assert_eq!(pin("idle.sh").fails_before, Some(false));
        assert!(
            run.escalations
                .iter()
                .any(|e| e.reason.contains("idle.sh pins nothing"))
        );
    }

    #[test]
    fn after_a_fix_the_probers_run_the_earlier_rounds_probes_again() {
        use super::super::change::tests::repo_with;
        let top = "stages: [probes]\ntests: {command: 'sh {tests}'}";
        let mut cfg = config("exception", 20, 1, "", top);
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-reprobe-{}", new_id()));
        let mut s = setting(&cfg);
        s.converge = true;
        s.copies = Some(root.clone());
        let fixed = Mutex::new(false);
        let checkout = repo.clone();
        let mut fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("You probe a change") => CLEAN.to_string(),
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    fs::write(checkout.join("a.src"), "let x = 2;\n").unwrap();
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "x is two now"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        fake.probe_file = Some("probe_test.sh");
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&s.work_dir);
        assert_eq!(run.status, Status::Confirmed);
        // Round 1's prober starts clean; every one after the fix starts with
        // the probe it wrote, to run again.
        let probed = fake.probed.lock().unwrap().clone();
        assert!(probed.len() >= 2, "{probed:?}");
        assert!(!probed[0]);
        assert!(probed[1..].iter().all(|had| *had), "{probed:?}");
        let calls = fake.calls.lock().unwrap();
        let probers: Vec<_> = calls
            .iter()
            .filter(|c| c.3.contains("You probe a change"))
            .collect();
        assert!(!probers[0].3.contains("Earlier rounds' probes"));
        assert!(
            probers[1]
                .3
                .contains("Earlier rounds' probes are in your copy already, in `probe_test.sh`")
        );
    }

    #[test]
    fn a_prober_that_writes_no_probe_did_no_work() {
        use super::super::change::tests::repo_with;
        let top = "stages: [probes]\ntests: {command: 'sh {tests}'}";
        let mut cfg = config("exception", 3, 1, "", top);
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        for wrote in [false, true] {
            let repo = repo_with(&[("a.src", "let x = 1;\n")]);
            let root = std::env::temp_dir().join(format!("tb-acr-noprobe-{}", new_id()));
            let mut s = setting(&cfg);
            s.copies = Some(root.clone());
            let mut fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
            if wrote {
                fake.probe_file = Some("probe_test.sh");
            }
            let c = change(&["a.src"]);
            let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
            drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
            let _ = fs::remove_dir_all(&root);
            let _ = fs::remove_dir_all(&s.work_dir);
            let told = said(&fake);
            assert_eq!(
                told.contains("did no work: it wrote no probe in its copy"),
                !wrote,
                "{told}"
            );
            assert_eq!(run.status == Status::Confirmed, wrote, "{told}");
        }
    }

    #[test]
    fn a_pin_in_the_file_it_tests_is_checked_without_the_fix() {
        use super::super::change::tests::repo_with;
        // The pin passes only on the fixed code, so it must fail before.
        let top = "stages: [test-writing]\ntests: {command: \"grep -q 'let x = 2' {tests}\"}";
        let cfg = config("exception", 20, 1, "", top);
        let code = |x: u32, tests: &str| {
            format!("let x = {x};\n\n#[cfg(test)]\nmod tests {{\n{tests}}}\n")
        };
        let repo = repo_with(&[("a.src", "let x = 1;\n"), ("a.rs", &code(1, ""))]);
        let root = std::env::temp_dir().join(format!("tb-acr-inline-pin-{}", new_id()));
        let mut s = setting(&cfg);
        s.converge = true;
        s.copies = Some(root.clone());
        let fixed = Mutex::new(false);
        let checkout = repo.clone();
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    fs::write(checkout.join("a.src"), "let x = 2;\n").unwrap();
                    fs::write(checkout.join("a.rs"), code(2, "")).unwrap();
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "x is two now"}"#.to_string()
                }
                Role::Orchestrator if prompt.contains("You write tests for a change") => {
                    fs::write(checkout.join("a.rs"), code(2, "    // pins x being two\n")).unwrap();
                    r#"{"tests": [{"file": "a.rs", "pins": ["alpha-r1-1"]}], "done": "pinned"}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let c = change(&["a.src", "a.rs"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        let pin = run.steelmans[0].pins[0].clone();
        assert_eq!(pin.file, "a.rs");
        assert_eq!(pin.fails_before, Some(true), "checked on the old code");
        assert_eq!(pin.passes_after, Some(true));
        assert!(
            !run.escalations
                .iter()
                .any(|e| e.reason.contains("pins nothing"))
        );
    }

    fn gated(mode: Option<&str>) -> (Run, Vec<(Role, String, bool, String)>) {
        use super::super::change::tests::repo_with;
        let top = match mode {
            Some(mode) => format!("stages: [gate]\ntests: {{command: 'true', gate: {mode}}}"),
            None => "stages: [gate]\ntests: {command: 'true'}".to_string(),
        };
        let cfg = config("exception", 1, 1, "", &top);
        assert!(cfg.lenses.iter().any(|l| l.id == "tests"));
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-gate-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        // `true` passes whatever the code, so the mutant survives.
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("proposing small breaks") => {
                    r#"{"mutants": [{"file": "a.src", "find": "x = 1", "replace": "x = 2", "why": "x is one", "tests": []}]}"#
                        .to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        let calls = fake.calls.lock().unwrap().clone();
        (run, calls)
    }

    #[test]
    fn the_gates_mutant_proposer_and_judge_are_reminded_put_in_form_and_restarted() {
        use super::super::change::tests::repo_with;
        let cfg = config(
            "exception",
            1,
            1,
            "",
            "stages: [gate]\ntests: {command: 'true', gate: warn}",
        );
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-gate-form-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let judged = Mutex::new(0);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                // The proposer answers in words, which the Sizer puts in form.
                Role::Reviewer if prompt.contains("proposing small breaks") => {
                    assert!(prompt.ends_with(roles::MUTANTS_REMINDER));
                    "I'd change x = 1 to x = 2 in a.src.".to_string()
                }
                Role::Sizer if prompt.contains("proposed these small breaks") => {
                    r#"{"mutants": [{"file": "a.src", "find": "x = 1", "replace": "x = 2", "why": "x is one", "tests": []}]}"#
                        .to_string()
                }
                // The gate's Judge: unreadable even in form the first time,
                // then fresh, it answers.
                Role::Judge if prompt.contains("[G0]") => {
                    let mut n = judged.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        "no idea".to_string()
                    } else {
                        r#"{"findings": [{"title": "[G0] a mutant every test passes", "file": "a.src", "severity": "High"}]}"#
                            .to_string()
                    }
                }
                Role::Sizer => "unreadable".to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&root);
        let gate = &run.lenses["tests"].rounds[0];
        assert!(gate.failures.is_empty(), "{:?}", gate.failures);
        assert_eq!(gate.findings.len(), 1);
        assert_eq!(*judged.lock().unwrap(), 2, "restarted once");
        let told = said(&fake);
        assert!(
            told.contains("restarted fresh: tests: the Judge, attempt 2"),
            "{told}"
        );
        assert!(run.usage.iter().any(|u| u.repaired.is_some()));
        assert!(run.usage.iter().any(|u| u.restart.is_some()));
    }

    #[test]
    fn a_test_gap_that_blocks_stops_the_round_before_the_reviewers() {
        let (run, calls) = gated(Some("stop"));
        let gate = &run.lenses["tests"].rounds[0];
        assert_eq!(gate.findings.len(), 1);
        assert!(gate.findings[0].blocks());
        assert!(
            gate.notes[0].contains("1 mutants: 0 caught, 1 missed"),
            "{:?}",
            gate.notes
        );
        // Only the mutants' proposer ran: no lens's reviewer spent a thing.
        let reviewers: Vec<_> = calls.iter().filter(|c| c.0 == Role::Reviewer).collect();
        assert_eq!(reviewers.len(), 1);
        assert!(reviewers[0].3.contains("proposing small breaks"));
        assert!(run.lenses["alpha"].rounds.is_empty() && run.lenses["beta"].rounds.is_empty());
    }

    #[test]
    fn a_warning_gate_lets_the_reviewers_run_and_is_the_default() {
        for mode in [Some("warn"), None] {
            let (run, calls) = gated(mode);
            assert!(run.lenses["tests"].rounds[0].findings[0].blocks());
            assert_eq!(
                calls.iter().filter(|c| c.0 == Role::Reviewer).count(),
                3,
                "{mode:?}"
            );
            assert_eq!(run.lenses["alpha"].rounds.len(), 1, "{mode:?}");
            // With no coverage command, the reviewers aren't told it ran one.
            let told: Vec<_> = calls
                .iter()
                .filter(|c| c.0 == Role::Reviewer && c.3.contains("What the test gate found"))
                .collect();
            assert!(!told.is_empty(), "{mode:?}");
            assert!(
                told.iter().all(|c| !c.3.contains("tests with coverage")),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn a_rounds_judges_run_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cfg = plain("exception", 1, 1);
        let (now, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Reviewer => ONE.to_string(),
                Role::Judge => {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(n, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    now.fetch_sub(1, Ordering::SeqCst);
                    rating("Low")
                }
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(fake.count(Role::Judge), 2);
        assert_eq!(
            most.load(Ordering::SeqCst),
            2,
            "alpha's and beta's Judges overlap"
        );
        assert!(run.lenses.values().all(|l| l.rounds[0].findings.len() == 1));
        assert_eq!(
            run.usage.iter().filter(|u| u.role == Role::Judge).count(),
            2
        );
    }

    #[test]
    fn a_runs_builder_brief_is_kept_so_a_resume_after_the_scope_gate_still_briefs_round_one() {
        let cfg = plain("ends", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let mut s = setting(&cfg);
        s.builder_brief = Some(stages::BuilderBrief {
            scope: Some("It makes x one, in a.src.".into()),
            lenses: BTreeMap::from([("alpha".to_string(), "Break the lookup.".to_string())]),
        });
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Scope));
        // Saved and read back, as `acr resume` does, with a Setting made
        // afresh, which has no brief of its own.
        let mut run: Run = serde_json::from_value(serde_json::to_value(&run).unwrap()).unwrap();
        assert!(run.builder_brief.is_some());
        let mut later = setting(&cfg);
        later.builder_brief = run.builder_brief.clone();
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "Approve".into()),
                ("Fixer", "The Orchestrator".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &later, &fake, &|_| Ok(c.clone())).unwrap();
        let alpha = prompts(&fake, Role::Reviewer, "alpha");
        assert!(alpha[0].contains("Break the lookup."), "round 1 is briefed");
    }

    #[test]
    fn the_builder_briefs_round_one_and_the_steelman_the_next() {
        let cfg = config("exception", 20, 1, "", "stages: [brief]");
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                    // Only the lens the builder left out.
                    assert!(prompt.contains("- beta:") && !prompt.contains("- alpha:"));
                    r#"{"briefs": {"beta": "Read the callers."}}"#.to_string()
                }
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    assert!(prompt.contains("# Brief the next round"));
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed", "briefs": {"alpha": "Check my fix in a.src at midnight."}}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let mut s = setting(&cfg);
        s.converge = true;
        s.builder_brief = Some(stages::BuilderBrief {
            scope: Some("It makes x one, in a.src.".into()),
            lenses: BTreeMap::from([("alpha".to_string(), "Break the lookup.".to_string())]),
        });
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let alpha: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Reviewer && c.3.contains("reviewer for alpha."))
            .map(|c| &c.3)
            .collect();
        assert!(alpha[0].contains("Break the lookup."));
        assert!(alpha[1].contains("Check my fix in a.src at midnight."));
        assert!(
            calls
                .iter()
                .any(|c| c.0 == Role::Reviewer && c.3.contains("Read the callers."))
        );
        drop(calls);
        let sent = fake.sent.lock().unwrap();
        assert!(
            sent.iter()
                .all(|(_, text)| text.starts_with(super::super::change::SAID)
                    && text.contains("## What its builder says it does"))
        );
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    #[test]
    fn the_brief_stage_briefs_each_lens_and_only_runners_get_the_runbook() {
        let top = "stages: [brief]\nrunbook: Run tests with scripts/test.sh.";
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", attack: "Run it in another time zone.", triggers: {paths: ["**/*.src"]}}"#;
        let cfg = config("exception", 1, 1, extra, top);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                    assert!(prompt.contains(
                        "- gamma: gamma criteria\n  How it's attacked: Run it in another time zone."
                    ));
                    assert!(prompt.contains("Run tests with scripts/test.sh."));
                    r#"{"briefs": {"alpha": "Break the lookup in a.src.", "gamma": "Try midnight."}}"#
                        .to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let brief_of = |id: &str| {
            calls
                .iter()
                .find(|c| c.0 == Role::Reviewer && c.3.contains(&format!("reviewer for {id}.")))
                .map(|c| c.3.clone())
                .unwrap()
        };
        let alpha = brief_of("alpha");
        assert!(
            alpha.contains("# Where to attack in this change")
                && alpha.contains("Break the lookup in a.src.")
        );
        let gamma = brief_of("gamma");
        assert!(gamma.contains("# How to attack in this lens\n\nRun it in another time zone."));
        assert!(gamma.contains("Try midnight."));
        // beta had no brief, and these reviewers can't run things, so none
        // gets the runbook.
        assert!(!brief_of("beta").contains("# Where to attack"));
        assert!(
            calls
                .iter()
                .all(|c| c.0 != Role::Reviewer || !c.3.contains("scripts/test.sh"))
        );
        drop(calls);
        assert_eq!(fake.count(Role::Orchestrator), 1, "the brief, once a round");
    }

    #[test]
    fn a_call_out_of_time_is_a_failure_and_nothing_tries_it_elsewhere() {
        // A call is never cut off while it works, so one that's out of time
        // hit its bridge's cap or stalled: a person hears of it, and
        // nothing quietly redoes the work on another binding.
        let mut cfg = plain("exception", 1, 1);
        let reviewer = cfg.bindings[&Role::Reviewer].clone();
        let other = cfg.bindings[&Role::Judge].clone();
        // A fallback from a config of before is read, and not used.
        cfg.allowed_bindings.get_mut(&reviewer).unwrap().fallback = Some(other.clone());
        let answers = |role, _: &str| {
            Ok(match role {
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        };
        let mut fake = Fake::new(answers);
        fake.slow = Some(reviewer.clone());
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_ne!(run.status, Status::Confirmed);
        let failed = failures(&run);
        assert!(
            failed.iter().any(|f| f.contains("ran out of time")),
            "{failed:?}"
        );
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .filter(|c| c.0 == Role::Reviewer)
                .all(|c| c.1 == reviewer),
            "a reviewer's call went to another binding"
        );
    }

    #[test]
    fn a_person_following_the_review_is_told_each_round_and_whose_call_each_is() {
        let cfg = plain("exception", 1, 1);
        let answers = |role, _: &str| {
            Ok(match role {
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        };
        let fake = Fake::new(answers);
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
        let told = fake.told.lock().unwrap();
        let lens = run.lenses.keys().next().unwrap().clone();
        let round = told
            .iter()
            .position(|t| t.starts_with("note: round 1: ") && t.contains(&lens))
            .unwrap_or_else(|| panic!("no round note: {told:?}"));
        let review = told
            .iter()
            .position(|t| t == &format!("call: {lens}: reviewer"))
            .unwrap_or_else(|| panic!("no labelled reviewer: {told:?}"));
        assert!(
            round < review,
            "the round is told before its calls: {told:?}"
        );
        assert!(
            told.iter().all(|t| t.len() > "call: ".len()),
            "a call with no label: {told:?}"
        );
    }

    #[test]
    fn an_orchestrator_that_picks_lenses_in_prose_is_put_in_form() {
        let extra = r#"  - {id: gamma, criteria: g, triggers: {description: "touches storage"}}"#;
        let top = "always_escalate: [\"a change to storage\"]";
        for (form, picked) in [
            (
                r#"{"lenses": ["gamma"], "escalate": ["a change to storage"]}"#,
                true,
            ),
            // A form that leaves a list out passes for nothing, so it's none.
            (r#"{"lenses": ["gamma"]}"#, false),
            ("unreadable", false),
        ] {
            let cfg = config("exception", 1, 1, extra, top);
            let fake = Fake::new(|role, prompt: &str| {
                Ok(match role {
                    Role::Orchestrator if prompt.contains("say which of these apply") => {
                        // A stray object first, then prose: neither is the answer.
                        "Note {\"x\": 1}. gamma applies: it's a change to storage.".to_string()
                    }
                    Role::Sizer if prompt.contains("An Orchestrator said which lenses") => {
                        assert!(prompt.contains("from these: gamma;"), "{prompt}");
                        assert!(prompt.contains("a change to storage"), "{prompt}");
                        form.to_string()
                    }
                    Role::Reviewer => CLEAN.to_string(),
                    _ => NOTHING.to_string(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert!(
                run.usage.iter().any(|u| u.role == Role::Sizer
                    && u.repaired
                        .as_ref()
                        .is_some_and(|r| r.role == Role::Orchestrator)),
                "{form}: {:?}",
                run.usage
            );
            let reasons: Vec<&str> = run.escalations.iter().map(|e| e.reason.as_str()).collect();
            if picked {
                assert!(run.lenses.contains_key("gamma"), "{form}");
                assert!(
                    reasons.contains(&"always_escalate: a change to storage"),
                    "{reasons:?}"
                );
                assert!(
                    !reasons.iter().any(|r| r.contains("couldn't check")),
                    "{reasons:?}"
                );
            } else {
                let unread = reasons.iter().any(|r| {
                    r.contains("couldn't check this change's descriptions")
                        && r.contains("didn't work either")
                });
                assert!(unread, "{form}: {reasons:?}");
            }
        }
    }

    #[test]
    fn a_lens_the_briefs_leave_out_is_no_brief_and_says_so() {
        // Put in form with a lens missing, or nothing at all: those lenses
        // were never briefed, which is a failure, not a pass.
        for form in [
            r#"{"briefs": {"alpha": "Break the lookup."}}"#,
            r#"{"briefs": {}}"#,
        ] {
            let cfg = config("exception", 1, 1, "", "stages: [brief]");
            let fake = Fake::new(|role, prompt: &str| {
                Ok(match role {
                    Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                        "In prose, as briefers sometimes answer.".to_string()
                    }
                    Role::Sizer if prompt.contains("A briefer wrote briefs") => form.to_string(),
                    Role::Reviewer => CLEAN.to_string(),
                    _ => NOTHING.to_string(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            let failed = failures(&run);
            assert!(
                failed
                    .iter()
                    .any(|f| f.starts_with("beta") && f.contains("has none for it")),
                "{form}: {failed:?}"
            );
        }
    }

    #[test]
    fn briefs_written_in_prose_are_put_in_form_not_lost() {
        let top = "stages: [brief]";
        for (form, briefed) in [
            (
                r#"{"briefs": {"alpha": "Break the lookup in a.src.", "beta": "Try none."}}"#,
                true,
            ),
            ("unreadable", false),
        ] {
            let cfg = config("exception", 1, 1, "", top);
            let fake = Fake::new(|role, prompt: &str| {
                Ok(match role {
                    Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                        "alpha: break the lookup in a.src.".to_string()
                    }
                    Role::Sizer if prompt.contains("A briefer wrote briefs") => {
                        assert!(prompt.contains("Lenses: alpha, beta."), "{prompt}");
                        assert!(prompt.contains("alpha: break the lookup in a.src."));
                        form.to_string()
                    }
                    Role::Reviewer => CLEAN.to_string(),
                    _ => NOTHING.to_string(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            let failed = failures(&run);
            let calls = fake.calls.lock().unwrap();
            let alpha = calls
                .iter()
                .find(|c| c.0 == Role::Reviewer && c.3.contains("reviewer for alpha."))
                .unwrap();
            assert_eq!(alpha.3.contains("Break the lookup in a.src."), briefed);
            // Only a briefer still unreadable after that is a failure.
            assert_eq!(failed.is_empty(), briefed, "{failed:?}");
            drop(calls);
            if !briefed {
                assert!(failed[0].contains("didn't work either"), "{failed:?}");
            }
            // The extra call is counted with the rest.
            let counted = run.usage.iter().filter(|u| u.role == Role::Sizer).count();
            assert_eq!(counted, fake.count(Role::Sizer));
            assert!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| c.3.contains("A briefer wrote"))
            );
        }
    }

    #[test]
    fn stages_join_as_lenses_and_each_runs_its_own_way() {
        use super::super::change::tests::repo_with;
        let top = "stages: [context, claims, mutation, probes]\ntests: {command: 'true'}";
        let cfg = config("exception", 1, 1, "", top);
        for id in ["claims", "mutation", "probes"] {
            assert!(cfg.lenses.iter().any(|l| l.id == id), "{id}");
        }
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let s = setting(&cfg);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                // The mutation reviewer proposes nothing.
                Role::Reviewer if prompt.contains("proposing small breaks") => {
                    r#"{"mutants": []}"#.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let mut c = change(&["a.src"]);
        c.diff = "+++ b/a.src\n@@ -0,0 +1 @@\n+// Sets x to one, as the spec says.\n".into();
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let calls = fake.calls.lock().unwrap();
        let claims = calls
            .iter()
            .find(|c| c.3.contains("You check what a change claims"))
            .expect("a claims reviewer");
        assert!(
            claims
                .3
                .contains("[a.src:1] comment: Sets x to one, as the spec says.")
        );
        drop(calls);
        // Read-only reviewers can't run scenarios, and with nowhere for
        // copies the mutation stage can't run either: each says so.
        let failed = |id: &str| run.lenses[id].rounds[0].failures.join(" ");
        assert!(
            failed("probes").contains("needs a shell"),
            "{}",
            failed("probes")
        );
        assert!(
            failed("mutation").contains("place for copies"),
            "{}",
            failed("mutation")
        );
        // Every role got the history pack ahead of the diff, among what's
        // said about the change. This repository has no tests or callers to
        // point to, so there are no leads.
        let sent = fake.sent.lock().unwrap();
        assert!(sent.iter().all(|(_, text)| {
            let said = text.find("## Earlier commits to the changed files");
            said.is_some_and(|at| text.starts_with(super::super::change::SAID) && at > 0)
                && !text.contains("# Leads in the repository")
        }));
    }

    #[test]
    fn reviewers_copies_come_from_one_warm_copy_set_up_once() {
        use super::super::change::tests::repo_with;
        let mut cfg = config("exception", 1, 1, "", "tests: {setup: 'touch warmed'}");
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-warm-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let fake = Fake::new(defect_in("alpha", 99));
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let warmed = fake.warmed.lock().unwrap().clone();
        assert_eq!(
            warmed,
            [true, true],
            "each reviewer's copy has the setup's work"
        );
        // The setup ran in a copy, never in the checkout, and the round's
        // copies are gone.
        assert!(!repo.join("warmed").exists());
        assert!(!root.join("r-r1").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_reviewer_that_can_change_files_works_in_a_copy_of_its_own() {
        use super::super::change::tests::repo_with;
        let mut cfg = plain("exception", 1, 1);
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-copies-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let fake = Fake::new(defect_in("alpha", 99));
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let repos = fake.repos.lock().unwrap();
        let reviewers: Vec<_> = repos.iter().filter(|r| r.0 == Role::Reviewer).collect();
        assert_eq!(reviewers.len(), 2);
        for (_, dir, had) in &reviewers {
            assert!(dir.starts_with(&root), "{}", dir.display());
            assert!(had, "{} held the change", dir.display());
        }
        assert_ne!(reviewers[0].1, reviewers[1].1, "each its own");
        // Every other role works in the checkout.
        let checkout = repo.canonicalize().unwrap();
        assert!(
            repos
                .iter()
                .filter(|r| r.0 != Role::Reviewer)
                .all(|r| r.1 == checkout)
        );
        drop(repos);
        let calls = fake.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.0 == Role::Reviewer && c.3.contains("your own copy"))
        );
        drop(calls);
        // The round's copies go with it.
        assert!(!root.join("r-r1").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn after_a_fix_only_what_it_touched_attacks_then_a_sweep_covers_the_rest() {
        let extra =
            r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.md"]}}"#;
        let cfg = config("exception", 20, 1, extra, "");
        let fixed = Mutex::new(false);
        let mut fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed alpha"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        // The fix touches only the code, not the docs gamma covers.
        fake.fixes = Some(vec!["a.src".into()]);
        let mut s = setting(&cfg);
        s.converge = true;
        let c = change(&["a.src", "README.md"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        // Round 1 all; round 2 alpha and beta, which the fix touched, with
        // the fix as their focus; round 3 the sweep: gamma, on the version
        // it hadn't seen.
        assert_eq!(run.round, 3);
        let rounds =
            |id: &str| -> Vec<u32> { run.lenses[id].rounds.iter().map(|r| r.round).collect() };
        assert_eq!(rounds("alpha"), [1, 2]);
        assert_eq!(rounds("beta"), [1, 2]);
        assert_eq!(rounds("gamma"), [1, 3]);
        assert_eq!(
            run.fix.as_ref().map(|f| f.files.clone()),
            Some(vec!["a.src".to_string()])
        );
        let sent = fake.sent.lock().unwrap();
        let focused = sent
            .iter()
            .filter(|(r, text)| {
                *r == Role::Reviewer && text.contains("What the fixes after round 1 changed")
            })
            .count();
        assert_eq!(
            focused, 2,
            "round 2's two reviewers got the fix as their focus"
        );
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    #[test]
    fn a_lens_a_later_version_of_the_change_needs_joins_and_none_leaves() {
        let extra =
            r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.db"]}}"#;
        let cfg = config("exception", 20, 1, extra, "");
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "moved it into the store"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let mut s = setting(&cfg);
        s.converge = true;
        let first = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &first, &fake).unwrap();
        assert!(!run.lenses.contains_key("gamma"));
        // The steelman's version touches only the store: no lens that ran
        // matches it now, and gamma does.
        let moved = change(&["store.db"]);
        drive(&mut run, &s, &fake, &|fixed| {
            Ok(if fixed { moved.clone() } else { first.clone() })
        })
        .unwrap();
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(run.round, 2);
        let gamma = &run.lenses["gamma"];
        assert_eq!(gamma.rounds.len(), 1);
        assert_eq!(gamma.joined, "round 2: its path store.db matched");
        for id in ["alpha", "beta"] {
            assert_eq!(run.lenses[id].rounds.len(), 2, "{id} stays on");
            assert!(run.lenses[id].joined.is_empty());
        }
    }

    #[test]
    fn a_steelman_folds_only_blockers_and_its_declines_are_disagreements() {
        let cfg = plain("exception", 20, 1);
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && !*fixed.lock().unwrap() => {
                    ONE.to_string()
                }
                Role::Reviewer if prompt.contains("beta") => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge if prompt.contains("alpha criteria") => rating("High"),
                Role::Judge => rating("Low"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed alpha", "declined": [{"finding": "alpha-r1-1", "reason": "it's a style call"}]}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let run = converging(&cfg, &fake);
        assert_eq!(run.status, Status::Confirmed);
        // beta's Low finding never blocks, so round 2 ends it.
        assert_eq!(run.round, 2);
        assert_eq!(run.steelmans.len(), 1);
        assert_eq!(run.steelmans[0].blockers, ["alpha-r1-1"]);
        let calls = fake.calls.lock().unwrap();
        let steelman = calls
            .iter()
            .find(|c| c.3.contains("You synthesize"))
            .unwrap();
        // What's upheld without blocking goes to a person, not into the work.
        assert!(steelman.3.contains("alpha-r1-1") && !steelman.3.contains("beta-r1-1"));
        drop(calls);
        let d = run
            .disagreements
            .iter()
            .find(|d| d.finding == "alpha-r1-1")
            .unwrap();
        assert_eq!(d.lens, "alpha");
        assert!(d.sides[1].contains("style call"));
        for round in &run.lenses["beta"].rounds {
            assert_eq!(round.findings[0].state, FindingState::Logged);
        }
    }

    #[test]
    fn a_blocker_at_max_rounds_stops_and_escalates() {
        let cfg = plain("exception", 2, 1);
        let fake = Fake::new(defect_in("alpha", 99));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Waiting);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(run.escalations[0].reason.contains("max_rounds"));
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
        assert_eq!(run.lenses["alpha"].rounds.len(), 2);
        let approve = Decision {
            approve: true,
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, approve).unwrap();
        assert_eq!(run.status, Status::Stopped, "seeing it doesn't pass it");
    }

    #[test]
    fn a_failed_reviewer_is_never_a_clean_pass() {
        let cfg = plain("exception", 3, 1);
        for fail in [true, false] {
            let fake = Fake::new(|role, prompt: &str| match role {
                Role::Reviewer if prompt.contains("alpha") && fail => bail!("the bridge is down"),
                Role::Reviewer if prompt.contains("alpha") => Ok("It all looks fine!".into()),
                Role::Reviewer => Ok(CLEAN.into()),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_ne!(run.status, Status::Confirmed);
            assert_eq!(run.waiting, Some(Gate::Escalation));
            assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
            assert_eq!(run.lenses["beta"].outcome, Outcome::Confirmed);
            assert!(run.escalations[0].reason.contains("wasn't fully reviewed"));
        }
    }

    #[test]
    fn stop_on_failure_ends_the_run_at_the_first_reviewer_or_judge_failure() {
        let cfg = plain("exception", 3, 1);
        let mut s = setting(&cfg);
        s.stop_on_failure = true;
        let c = change(&["a.src"]);
        let reviewer = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => bail!("the bridge is down"),
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let judge = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok("couldn't say".into()),
            _ => Ok(NOTHING.into()),
        });
        let stops = |fake: &dyn Caller, says: &str| {
            let mut run = start("r", Path::new("."), "main", None, &s, &c, fake).unwrap();
            let err = drive(&mut run, &s, fake, &|_| Ok(c.clone()))
                .unwrap_err()
                .to_string();
            assert!(
                err.starts_with("stopped at the first failure") && err.contains(says),
                "{err}"
            );
            assert_eq!(run.status, Status::Stopped);
            assert!(
                run.lenses.values().all(|l| l.rounds.len() == 1),
                "no round after the failure"
            );
        };
        stops(&reviewer, "the bridge is down");
        stops(&judge, "the Judge");
        // Without it, the same failure is recorded and the run goes on to
        // a person.
        s.stop_on_failure = false;
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &reviewer).unwrap();
        drive(&mut run, &s, &reviewer, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(failures(&run)[0].contains("alpha, round 1: "));
    }

    #[test]
    fn a_binding_whose_answers_keep_needing_repair_is_said_in_progress() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer => Ok("No findings, in prose.".into()),
            Role::Sizer if prompt.contains("A reviewer wrote this report") => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let counted = super::super::report::repairs(&run.usage);
        assert_eq!(counted.len(), 1);
        assert_eq!(counted[0].repairs, counted[0].calls, "{counted:?}");
        assert!(counted[0].repairs >= REPAIR_WARN_MIN);
        let told = fake.told.lock().unwrap();
        assert!(
            told.iter()
                .any(|t| t.contains("format repairs") && t.contains("format instructions")),
            "{told:?}"
        );
    }

    #[test]
    fn a_review_reads_a_copy_of_its_commit_until_the_first_fix() {
        let cfg = plain("exception", 2, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let copy = std::env::temp_dir().join(format!("tb-acr-head-copy-{}", new_id()));
        fs::create_dir_all(&copy).unwrap();
        let mut s = setting(&cfg);
        s.head_copy = Some(copy.clone());
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", Some("change"), &s, &c, &fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, &fake, &|_| Ok(again.clone())).unwrap();
        assert_eq!(run.head_copy.as_deref(), Some(copy.as_path()));
        // Every call worked in the copy, none in the checkout.
        {
            let repos = fake.repos.lock().unwrap();
            assert!(
                repos.iter().any(|(r, ..)| *r == Role::Reviewer),
                "{repos:?}"
            );
            for (role, repo, _) in repos.iter() {
                assert_eq!(repo, &copy, "{role:?} worked in {}", repo.display());
            }
        }
        // After a fix, the review is of the checkout, where the fixes are.
        assert_eq!(run.reading(), copy.as_path());
        run.lenses
            .values_mut()
            .next()
            .unwrap()
            .done
            .push("fixed".into());
        assert_eq!(run.reading(), run.repo.as_path());
        // Without a copy, it reads the checkout throughout.
        run.head_copy = None;
        run.lenses.values_mut().for_each(|l| l.done.clear());
        assert_eq!(run.reading(), run.repo.as_path());
        fs::remove_dir_all(&copy).ok();
    }

    #[test]
    fn fixes_go_into_the_checkout_which_must_hold_the_reviewed_commit_first() {
        use super::super::change::tests::{branch, commit, repo_with};
        let cfg = plain("exception", 1, 1);
        let s = setting(&cfg);
        let dir = repo_with(&[("a.txt", "one\n")]);
        branch(&dir, "change");
        commit(&dir, &[("a.txt", "two\n")], "two");
        let checkout = |r: &str| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["checkout", "-q", r])
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        checkout("main");
        let mut run: Run = serde_json::from_value(serde_json::json!({
            "id": "r", "repo": dir, "base": "main", "head": "change",
            "tier": "a", "tier_why": "", "fan_out": 1, "round": 1, "stage": "fix",
            "status": "running", "waiting": null, "lenses": {}, "escalations": [],
            "disagreements": [], "usage": [], "head_copy": "/elsewhere/copy"
        }))
        .unwrap();
        // The checkout is on another branch: a person is asked to check it out.
        assert!(!ready_to_fix(&mut run, &s));
        let said = run
            .escalations
            .last()
            .map(|e| e.reason.clone())
            .unwrap_or_default();
        assert!(said.contains("its fixes go into the checkout"), "{said}");
        assert!(
            said.contains("change has to be what's checked out"),
            "{said}"
        );
        // Checked out, the fixes can go in.
        checkout("change");
        assert!(ready_to_fix(&mut run, &s));
        // A review that read the checkout all along was checked when it started.
        checkout("main");
        run.head_copy = None;
        assert!(ready_to_fix(&mut run, &s));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_reviewer_that_reports_in_prose_is_put_in_form_not_failed() {
        let cfg = plain("exception", 3, 1);
        let prose = "Every scenario passed, so I have no findings.";
        let reformats = &Mutex::new(0);
        let put_in_form = |answer: &'static str| {
            Fake::new(move |role, prompt: &str| match role {
                Role::Reviewer if prompt.contains("alpha") => Ok(prose.into()),
                Role::Reviewer => Ok(CLEAN.into()),
                Role::Sizer if prompt.contains("A reviewer wrote this report") => {
                    assert!(prompt.contains(prose), "the report goes as it was");
                    *reformats.lock().unwrap() += 1;
                    Ok(answer.into())
                }
                _ => Ok(NOTHING.into()),
            })
        };
        // Its report says it found nothing: the lens is confirmed.
        let fake = put_in_form(CLEAN);
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert!(failures(&run).is_empty(), "{:?}", failures(&run));
        assert_eq!(*reformats.lock().unwrap(), 1);
        // The repair is a call like any other, counted against the reviewer
        // whose answer it put in form.
        let repairs: Vec<&Usage> = run.usage.iter().filter(|u| u.repaired.is_some()).collect();
        assert_eq!(repairs.len(), 1, "{:?}", run.usage);
        assert_eq!(repairs[0].role, Role::Sizer);
        assert!(
            repairs[0]
                .label
                .as_deref()
                .unwrap()
                .ends_with("put in form by the Sizer")
        );
        let of = repairs[0].repaired.as_ref().unwrap();
        assert_eq!(of.role, Role::Reviewer);
        let counted = super::super::report::repairs(&run.usage);
        let reviewer_calls = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Reviewer && u.binding == of.binding)
            .count();
        assert_eq!(counted.len(), 1);
        assert_eq!((counted[0].repairs, counted[0].calls), (1, reviewer_calls));
        // One in several isn't worth a word.
        assert!(
            !fake
                .told
                .lock()
                .unwrap()
                .iter()
                .any(|t| t.contains("format repairs")),
            "{:?}",
            fake.told.lock().unwrap()
        );
        // A report that can't be put in form is asked for again, in a
        // fresh session, and one that's no better is still a failure.
        let fake = put_in_form("unreadable");
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_ne!(run.status, Status::Confirmed);
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
        let said = failures(&run).join(" ");
        assert!(said.contains("didn't work either"), "{said}");
        assert_eq!(prompts(&fake, Role::Reviewer, "alpha").len(), 2);
        assert_eq!(
            *reformats.lock().unwrap(),
            3,
            "once, then once for each session"
        );
        assert!(said.contains("came back unusable twice"), "{said}");
        let told = super::tests::said(&fake);
        assert!(
            told.contains("restarted fresh: alpha: reviewer, attempt 2, because"),
            "{told}"
        );
    }

    #[test]
    fn a_reviewer_whose_answer_cant_be_read_even_put_in_form_is_asked_again_fresh() {
        let cfg = plain("exception", 3, 1);
        let asked = &Mutex::new(0);
        let fake = Fake::new(move |role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => {
                let mut n = asked.lock().unwrap();
                *n += 1;
                Ok(if *n == 1 {
                    "I looked.".into()
                } else {
                    CLEAN.into()
                })
            }
            Role::Reviewer => Ok(CLEAN.into()),
            Role::Sizer if prompt.contains("A reviewer wrote this report") => {
                Ok("still prose".into())
            }
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        // The fresh session answered, so the lens was reviewed.
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
        assert!(failures(&run).is_empty(), "{:?}", failures(&run));
        let alpha = prompts(&fake, Role::Reviewer, "alpha");
        assert_eq!(alpha.len(), 2);
        assert_eq!(
            alpha[0], alpha[1],
            "the same brief, in a session of its own"
        );
        let told = said(&fake);
        assert!(
            told.contains(
                "restarted fresh: alpha: reviewer, attempt 2, because the answer has no JSON, and \
                 putting its report in the form asked didn't work either"
            ),
            "{told}"
        );
        // Both sessions are counted, each as its own call, the restart's
        // marked with why, and said apart from repairs.
        let alpha: Vec<&Usage> = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Reviewer && u.lens.as_deref() == Some("alpha"))
            .collect();
        let calls: Vec<&str> = alpha.iter().filter_map(|u| u.call.as_deref()).collect();
        assert_eq!(calls, vec!["alpha: reviewer", "alpha: reviewer, restarted"]);
        assert!(alpha[0].restart.is_none());
        assert!(
            alpha[1]
                .restart
                .as_deref()
                .unwrap()
                .contains("the answer has no JSON")
        );
        let restarts = report::restarts(&run.usage);
        assert_eq!(restarts.len(), 1);
        assert_eq!(restarts[0].calls, 1);
        assert_eq!(restarts[0].units[0].0, "alpha: reviewer, restarted");
        let line = restarts[0].line();
        assert!(
            line.starts_with("round 1: restarted fresh: 1 unit(s), 1 call(s)"),
            "{line}"
        );
        assert!(told.contains(&line), "said at the round's end: {told}");
        // The prompt's last words say the answer's form.
        assert!(alpha_prompt_ends_with_reminder(&fake));
        // One that failed for a reason a fresh session would only repeat,
        // such as its config, isn't restarted.
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => bail!("the bridge is down"),
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
        assert_eq!(prompts(&fake, Role::Reviewer, "alpha").len(), 1);
    }

    /// Whether every alpha reviewer's prompt ends saying its answer's form.
    fn alpha_prompt_ends_with_reminder(
        fake: &Fake<impl Fn(Role, &str) -> Result<String> + Sync>,
    ) -> bool {
        prompts(fake, Role::Reviewer, "alpha")
            .iter()
            .all(|p| p.ends_with(roles::FINDINGS_REMINDER))
    }

    /// A call's failure as a bridge would report it.
    fn failed(failure: Failure, message: &str) -> anyhow::Error {
        anyhow::Error::new(CallFailed {
            failure,
            message: message.to_string(),
        })
    }

    #[test]
    fn a_call_that_lost_its_requests_is_restarted_fresh_with_a_new_cache_key() {
        let cfg = plain("exception", 3, 1);
        let asked = &Mutex::new(0);
        let fake = Fake::new(move |role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => {
                let mut n = asked.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    Err(failed(Failure::NoResponse, "pi exit 75: lost requests"))
                } else {
                    Ok(CLEAN.into())
                }
            }
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
        assert_eq!(prompts(&fake, Role::Reviewer, "alpha").len(), 2);
        // A replica that didn't answer isn't asked again: a new key.
        assert_eq!(
            *fake.rekeyed.lock().unwrap(),
            vec!["alpha: reviewer, restarted".to_string()]
        );
        assert!(
            said(&fake).contains(
                "restarted fresh: alpha: reviewer, attempt 2, because pi exit 75: lost requests"
            ),
            "{}",
            said(&fake)
        );
        // An answer it couldn't read keeps its key: the replica answered.
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => Err(failed(Failure::Failed, "exit 1")),
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(prompts(&fake, Role::Reviewer, "alpha").len(), 2);
        assert!(fake.rekeyed.lock().unwrap().is_empty());
        let reason = &run.escalations[0].reason;
        assert!(
            reason.contains("came back unusable twice: first exit 1; then"),
            "{reason}"
        );
        // A refusal, a cancel, or a failure a fresh session would only
        // repeat isn't restarted.
        for failure in [Failure::Refused, Failure::Cancelled] {
            let fake = Fake::new(move |role, prompt: &str| match role {
                Role::Reviewer if prompt.contains("alpha") => Err(failed(failure, "no")),
                Role::Reviewer => Ok(CLEAN.into()),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(
                prompts(&fake, Role::Reviewer, "alpha").len(),
                1,
                "{failure:?}"
            );
            assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
        }
    }

    #[test]
    fn a_restarted_prober_gets_a_fresh_copy_without_the_first_ones_probes() {
        use super::super::change::tests::repo_with;
        let top = "stages: [probes]\ntests: {command: 'sh {tests}'}";
        let mut cfg = config("exception", 3, 1, "", top);
        let name = cfg.bindings[&Role::Reviewer].clone();
        cfg.allowed_bindings.get_mut(&name).unwrap().tools =
            Some(vec!["Read".into(), "Bash".into()]);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-restart-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let probes = &Mutex::new(0);
        let mut fake = Fake::new(move |role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("You probe a change") => {
                    let mut n = probes.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        "I probed it.".into()
                    } else {
                        CLEAN.into()
                    }
                }
                Role::Reviewer => CLEAN.into(),
                Role::Sizer if prompt.contains("A reviewer wrote this report") => {
                    "unreadable".into()
                }
                _ => NOTHING.into(),
            })
        });
        fake.probe_file = Some("probe_test.sh");
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let _ = fs::remove_dir_all(&s.work_dir);
        let told = said(&fake);
        assert_eq!(run.status, Status::Confirmed, "{told}");
        // Its fresh copy didn't hold the probe the first wrote.
        assert_eq!(*fake.probed.lock().unwrap(), vec![false, false]);
        let copies: Vec<PathBuf> = fake
            .repos
            .lock()
            .unwrap()
            .iter()
            .filter(|r| {
                r.0 == Role::Reviewer
                    && r.1.starts_with(&root)
                    && r.1
                        .strip_prefix(&root)
                        .is_ok_and(|p| p.to_string_lossy().contains("probe"))
            })
            .map(|r| r.1.clone())
            .collect();
        assert_eq!(copies.len(), 2, "{copies:?}");
        assert_ne!(copies[0], copies[1]);
        assert!(
            copies[1].to_string_lossy().contains("-restarted"),
            "{copies:?}"
        );
        assert!(told.contains(": prober, attempt 2, because"), "{told}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_judges_answer_is_put_in_form_and_one_that_cant_be_is_restarted() {
        let cfg = plain("exception", 3, 1);
        for (repair, judges) in [(true, 1), (false, 2)] {
            let judged = &Mutex::new(0);
            let repairs = &Mutex::new(Vec::<String>::new());
            let fake = Fake::new(move |role, prompt: &str| {
                Ok(match role {
                    Role::Reviewer if prompt.contains("alpha") => ONE.into(),
                    Role::Reviewer => CLEAN.into(),
                    Role::Judge => {
                        let mut n = judged.lock().unwrap();
                        *n += 1;
                        if *n == 1 {
                            "It's a real defect, High.".into()
                        } else {
                            rating("Low")
                        }
                    }
                    Role::Sizer if prompt.contains("A Judge wrote this ruling") => {
                        repairs.lock().unwrap().push(prompt.to_string());
                        if repair {
                            rating("Low")
                        } else {
                            "unreadable".into()
                        }
                    }
                    _ => NOTHING.into(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
            assert_eq!(*judged.lock().unwrap(), judges, "repair: {repair}");
            let asked = repairs.lock().unwrap().clone();
            // Told why it couldn't be read, and the form asked.
            assert!(
                asked[0].contains("Reading it as asked failed with:"),
                "{}",
                asked[0]
            );
            assert!(asked[0].contains("\"severity\""));
            assert!(asked[0].contains("It's a real defect, High."));
            // The Judge's prompt ends saying its answer's form.
            assert!(
                prompts(&fake, Role::Judge, "alpha")
                    .iter()
                    .all(|p| p.ends_with(roles::RATINGS_REMINDER))
            );
            let counted = report::repairs(&run.usage);
            assert!(counted.iter().any(|r| r.role == Role::Judge), "{counted:?}");
            assert_eq!(
                said(&fake).contains("restarted fresh: alpha: the Judge, attempt 2"),
                !repair,
                "{}",
                said(&fake)
            );
        }
    }

    #[test]
    fn an_outage_pauses_the_round_and_resuming_runs_only_what_didnt_finish() {
        let cfg = plain("exception", 3, 2);
        let down = &std::sync::atomic::AtomicBool::new(true);
        let fake = Fake::new(move |role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => {
                if down.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(150));
                    Err(failed(Failure::NoResponse, "pi exit 75: lost requests"))
                } else {
                    Ok(CLEAN.into())
                }
            }
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(rating("Low")),
            _ => Ok(NOTHING.into()),
        });
        let s = setting(&cfg);
        fs::create_dir_all(&s.work_dir).unwrap();
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let e = drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap_err();
        assert!(is_outage(&e), "{e:#}");
        assert_eq!(run.outage.as_ref().map(|o| o.round), Some(1));
        assert_eq!(run.round, 1);
        // What finished is kept as it finished: beta's reviewers and Judge.
        let kept = fs::read_to_string(s.work_dir.join("round-1.units.jsonl")).unwrap();
        assert_eq!(kept.matches("\"kind\":\"reviewer\"").count(), 2, "{kept}");
        assert_eq!(kept.matches("\"kind\":\"judge\"").count(), 1, "{kept}");
        // An outage restarts nothing: the round pauses instead.
        assert!(!said(&fake).contains("restarted fresh"), "{}", said(&fake));
        assert!(said(&fake).contains("round 1: paused, since its calls can't reach their model"));
        let before = |has: &str, role: Role| prompts(&fake, role, has).len();
        assert_eq!(before("beta", Role::Reviewer), 2);
        assert_eq!(before("beta", Role::Judge), 1);
        // Back up: the round goes on, and only alpha's reviewers run again.
        down.store(false, std::sync::atomic::Ordering::SeqCst);
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
        assert!(run.outage.is_none());
        assert_eq!(before("beta", Role::Reviewer), 2, "kept, not asked again");
        assert_eq!(before("beta", Role::Judge), 1, "kept, not asked again");
        assert_eq!(before("alpha", Role::Reviewer), 4);
        assert_eq!(run.lenses["beta"].rounds.len(), 1);
        assert_eq!(run.lenses["beta"].rounds[0].findings.len(), 1);
        // Its kept calls are counted once, when the round goes on.
        let beta_reviews = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Reviewer && u.lens.as_deref() == Some("beta"))
            .count();
        assert_eq!(beta_reviews, 2);
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    #[test]
    fn calls_that_cant_reach_their_model_together_are_an_outage_and_one_alone_isnt() {
        let watch = OutageWatch::new(0);
        for _ in 0..4 {
            watch.started();
        }
        let lost = failed(Failure::NoResponse, "lost");
        watch.failed(&lost);
        assert!(watch.declared().is_none(), "one of four");
        watch.failed(&failed(Failure::Failed, "exit 1"));
        assert!(watch.declared().is_none(), "not a connection's");
        watch.failed(&lost);
        assert!(watch.declared().is_some(), "two of four");
        // An answer in between says the model is reachable.
        let watch = OutageWatch::new(0);
        for _ in 0..3 {
            watch.started();
        }
        watch.failed(&lost);
        watch.answered();
        watch.failed(&lost);
        assert!(watch.declared().is_none());
        // One call alone is never an outage: it's restarted.
        let watch = OutageWatch::new(0);
        watch.started();
        watch.failed(&lost);
        assert!(watch.declared().is_none());
        assert!(!watch.settle());
    }

    /// A blocker as the Judge ruled it, for routing.
    fn blocker(id: &str, severity: Severity, file: Option<&str>, line: Option<u64>) -> Finding {
        Finding {
            id: id.into(),
            title: format!("defect {id}"),
            detail: "the cause".into(),
            file: file.map(str::to_string),
            line,
            severity,
            action: Action::Blocks,
            reason: String::new(),
            state: FindingState::Folded,
            note: String::new(),
            same_as: None,
            reused_from: None,
            ruled_by: None,
        }
    }

    #[test]
    fn a_fix_group_is_easy_only_if_every_rule_holds() {
        let fixer = super::super::config::FixerConfig {
            binding: "small".into(),
            max_severity: Severity::Medium,
            max_files: 1,
        };
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.into()));
        let (mut run, _) = go(&cfg, &["a.src"], &fake);
        run.round = 2;
        let group = |findings: Vec<Finding>| FixGroup {
            findings: findings
                .into_iter()
                .map(|f| ("alpha".to_string(), f))
                .collect(),
            reported: Vec::new(),
        };
        let easy = group(vec![blocker(
            "a1",
            Severity::Medium,
            Some("a.src"),
            Some(3),
        )]);
        assert_eq!(
            fix_route(&run, &fixer, &easy),
            (true, "easy: 1 file(s), Medium, cause stated".to_string())
        );
        let hard = |run: &Run, g: &FixGroup, why: &str| {
            let (easy, said) = fix_route(run, &fixer, g);
            assert!(!easy, "{said}");
            assert!(said.starts_with("hard: ") && said.contains(why), "{said}");
        };
        hard(
            &run,
            &group(vec![blocker("a1", Severity::High, Some("a.src"), Some(3))]),
            "a High blocker",
        );
        hard(
            &run,
            &group(vec![
                blocker("a1", Severity::Medium, Some("a.src"), Some(3)),
                blocker("a2", Severity::Low, Some("b.src"), Some(4)),
            ]),
            "cites 2 files",
        );
        hard(
            &run,
            &group(vec![blocker("a1", Severity::Medium, Some("a.src"), None)]),
            "cause isn't stated with a line and detail",
        );
        let mut no_detail = blocker("a1", Severity::Medium, Some("a.src"), Some(3));
        no_detail.detail = " ".into();
        hard(&run, &group(vec![no_detail]), "cause isn't stated");
        hard(
            &run,
            &group(vec![blocker("a1", Severity::Medium, None, None)]),
            "cause isn't stated with a file",
        );
        hard(
            &run,
            &FixGroup {
                findings: Vec::new(),
                reported: vec![Reported {
                    id: "missed-1".into(),
                    round: 1,
                    text: "it misses a case".into(),
                    fixed: false,
                }],
            },
            "a defect a person reported missed",
        );
        // A person's call: a values call or a split.
        run.escalations.push(Escalation {
            reason: "a values call".into(),
            seen: true,
            about: Some(About::Finding {
                lens: "alpha".into(),
                finding: "called".into(),
            }),
        });
        hard(
            &run,
            &group(vec![blocker(
                "called",
                Severity::Medium,
                Some("a.src"),
                Some(3),
            )]),
            "a person's call",
        );
        // Like one an earlier round's fix fixed: the same claim, the same lens.
        run.lenses.get_mut("alpha").unwrap().rounds[0]
            .findings
            .push(blocker("old", Severity::Medium, Some("a.src"), Some(9)));
        let mut again = blocker("new", Severity::Medium, Some("a.src"), Some(3));
        again.title = "defect old".into();
        hard(
            &run,
            &group(vec![again]),
            "recurring: like a blocker fixed after round 1",
        );
    }

    /// Two blockers with their causes stated: alpha's High in a.src, and
    /// beta's Critical in b.src; and a fixer whose first fix may fail.
    fn routed_blockers<'a>(
        fixed: &'a Mutex<bool>,
        fixes: &'a Mutex<u32>,
        first_fails: bool,
    ) -> impl Fn(Role, &str) -> Result<String> + Sync + 'a {
        move |role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if !*fixed.lock().unwrap() => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge if prompt.contains("alpha criteria") => r#"{"findings": [
                    {"title": "alpha defect", "detail": "the cause", "file": "a.src", "line": 3, "severity": "High"}]}"#
                    .to_string(),
                Role::Judge => r#"{"findings": [
                    {"title": "beta defect", "detail": "the cause", "file": "b.src", "line": 4, "severity": "Critical"}]}"#
                    .to_string(),
                Role::Orchestrator if prompt.contains("Fix these") => {
                    let mut n = fixes.lock().unwrap();
                    *n += 1;
                    if first_fails && *n == 1 {
                        return Ok("I couldn't tell what to change.".into());
                    }
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed it"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        }
    }

    #[test]
    fn savings_fixer_sends_easy_groups_to_its_binding_and_hard_ones_to_the_orchestrators() {
        let orchestrator = |cfg: &Config| cfg.bindings[&Role::Orchestrator].clone();
        let with_fixer = |max: Severity| {
            let mut cfg = plain("exception", 3, 1);
            cfg.allowed_bindings.get_mut("small").unwrap().writes = true;
            cfg.savings.fixer = Some(super::super::config::FixerConfig {
                binding: "small".into(),
                max_severity: max,
                max_files: 1,
            });
            assert!(cfg.fixer().unwrap().is_some());
            cfg
        };
        fn fix_calls(fake: &Fake<impl Fn(Role, &str) -> Result<String> + Sync>) -> Vec<String> {
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == Role::Orchestrator && c.3.contains("Fix these"))
                .map(|c| c.1.clone())
                .collect()
        }
        // Alpha's High is easy under High; beta's Critical is hard.
        let cfg = with_fixer(Severity::High);
        let (fixed, fixes) = (Mutex::new(false), Mutex::new(0));
        let fake = Fake::new(routed_blockers(&fixed, &fixes, false));
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
        let mut on = fix_calls(&fake);
        on.sort();
        let mut want = vec!["small".to_string(), orchestrator(&cfg)];
        want.sort();
        assert_eq!(on, want);
        let told = said(&fake);
        assert!(
            told.contains("to small (easy: 1 file(s), High, cause stated)"),
            "{told}"
        );
        assert!(
            told.contains(&format!(
                "to {} (hard: a Critical blocker)",
                orchestrator(&cfg)
            )),
            "{told}"
        );
        // The Orchestrator's other calls keep its binding.
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == Role::Orchestrator && !c.3.contains("Fix these"))
                .all(|c| c.1 == orchestrator(&cfg))
        );
        assert_eq!(run.routes.len(), 2);
        // Fix cost and groups by binding, and whether their blockers held.
        let by = report::fix_routes(&run);
        let easy = by.iter().find(|b| b.easy).unwrap();
        assert_eq!(
            (easy.binding.as_str(), easy.groups, easy.calls),
            ("small", 1, 1)
        );
        assert_eq!((easy.judged, easy.held), (1, 1));
        assert!(
            easy.line()
                .starts_with("fixes on small (easy): 1 group(s), 1 call(s)")
        );
        let fix_usage: Vec<&Usage> = run.usage.iter().filter(|u| u.route.is_some()).collect();
        assert!(fix_usage.iter().all(|u| u.role == Role::Orchestrator));
        assert!(fix_usage.iter().any(|u| u.binding == "small"));

        // An easy group whose fix failed isn't easy: its restart goes to
        // the Orchestrator's binding.
        let cfg = with_fixer(Severity::High);
        let (fixed, fixes) = (Mutex::new(false), Mutex::new(0));
        let fake = Fake::new(routed_blockers(&fixed, &fixes, true));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
        let on = fix_calls(&fake);
        assert_eq!(on[0], "small");
        assert_eq!(on[1], orchestrator(&cfg));
        assert!(
            said(&fake).contains("(hard: its easy fix failed)"),
            "{}",
            said(&fake)
        );

        // Without it, every group goes to the Orchestrator's binding, as
        // before, and nothing is said of routes.
        let cfg = plain("exception", 3, 1);
        let (fixed, fixes) = (Mutex::new(false), Mutex::new(0));
        let fake = Fake::new(routed_blockers(&fixed, &fixes, false));
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert!(fix_calls(&fake).iter().all(|b| *b == orchestrator(&cfg)));
        assert!(run.routes.is_empty());
        assert!(!said(&fake).contains("fix group"));
        assert!(run.usage.iter().all(|u| u.route.is_none()));
    }

    #[test]
    fn savings_fixer_names_a_binding_that_may_write_and_defaults_its_rules() {
        let resolved = |fixer: &str| {
            let text = format!(
                "version: 1\nsavings: {{fixer: {fixer}}}\ntiers:\n  - {{id: big, match: {{paths: [\"**/*.src\"]}}, fan_out: 1}}\nlenses:\n  - {{id: alpha, criteria: \"alpha criteria\", triggers: {{paths: [\"**/*.src\"]}}}}\n"
            );
            resolve(
                &parse_user(USER).unwrap(),
                parse_repo(&text)?,
                &Flags::default(),
            )
        };
        let cfg = resolved("{binding: large}").unwrap();
        let f = cfg.savings.fixer.clone().unwrap();
        assert_eq!((f.max_severity, f.max_files), (Severity::High, 1));
        assert!(
            format!(
                "{:#}",
                resolved("{binding: large, max_severity: Critical}").unwrap_err()
            )
            .contains("can't be Critical")
        );
        assert!(cfg.chosen_bindings().iter().any(|(n, _)| *n == "large"));
        let cfg = resolved("{binding: large, max_severity: High, max_files: 2}").unwrap();
        let f = cfg.savings.fixer.unwrap();
        assert_eq!((f.max_severity, f.max_files), (Severity::High, 2));
        let e = resolved("{binding: small}").unwrap_err();
        assert!(format!("{e:#}").contains("can't apply fixes"), "{e:#}");
        let e = resolved("{binding: nowhere}").unwrap_err();
        assert!(
            format!("{e:#}").contains("isn't in your allowed_bindings"),
            "{e:#}"
        );
        assert!(resolved("{binding: large, max_rounds: 2}").is_err());
    }

    #[test]
    fn one_failed_reviewer_of_several_leaves_the_lens_open() {
        let cfg = plain("exception", 3, 3);
        let tries = Mutex::new(0);
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("alpha") => {
                let mut tries = tries.lock().unwrap();
                *tries += 1;
                if *tries == 2 {
                    bail!("one of three failed");
                }
                Ok(CLEAN.into())
            }
            Role::Reviewer => Ok(CLEAN.into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
    }

    #[test]
    fn a_failed_judge_leaves_the_lens_open() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok("couldn't say".into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(run.lenses.values().all(|l| l.outcome == Outcome::Open));
    }

    #[test]
    fn fan_out_merges_into_one_result_for_the_lens() {
        let cfg = plain("exception", 3, 3);
        let fake = Fake::new(defect_in("alpha", 1));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        // Three reviewers for each of two lenses, then three for alpha.
        assert_eq!(fake.count(Role::Reviewer), 9);
        let first = &run.lenses["alpha"].rounds[0];
        assert_eq!(first.reviewers.len(), 3);
        assert_eq!(first.findings.len(), 1, "the Judge merges them");
        assert!(first.reviewers.iter().all(|r| r.contains("AI agent")));
    }

    #[test]
    fn severities_do_what_they_say() {
        for (severity, status, state) in [
            ("Critical", Status::Waiting, FindingState::Open),
            ("High", Status::Waiting, FindingState::Open),
            ("Medium", Status::Confirmed, FindingState::Logged),
            ("Low", Status::Confirmed, FindingState::Logged),
            ("Rejected", Status::Confirmed, FindingState::Dropped),
        ] {
            let cfg = plain("per-round", 3, 1);
            let fake = Fake::new(|role, _: &str| match role {
                Role::Reviewer => Ok(ONE.into()),
                Role::Judge => Ok(rating(severity)),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(run.status, status, "{severity}");
            assert_eq!(run.lenses["alpha"].rounds[0].findings[0].state, state);
        }
    }

    #[test]
    fn a_real_finding_another_lens_owns_is_rated_like_any_and_noise_is_dropped() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(r#"{"findings": [{"title": "t", "severity": "Critical",
                "on_task": false, "reason": "another lens's job"},
                {"title": "n", "severity": "High", "real_defect": false, "reason": "not so"}]}"#
                .into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let f = &run.lenses["alpha"].rounds[0].findings;
        // A real Critical blocks, whichever lens finds it.
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(
            f[0].reason,
            "another lens's job (outside this lens's criteria)"
        );
        assert_eq!(f[1].severity, Severity::Rejected);
        assert_eq!(f[1].state, FindingState::Dropped);
    }

    #[test]
    fn every_combination_of_facts_has_one_ruling_and_none_hides_a_defect() {
        let cfg = plain("exception", 3, 1);
        let lens = cfg.lenses[0].clone();
        let severities = [
            Severity::Critical,
            Severity::High,
            Severity::Medium,
            Severity::Low,
            Severity::Rejected,
        ];
        let rank = |s: Severity| match s {
            Severity::Critical => 4,
            Severity::High => 3,
            Severity::Medium => 2,
            Severity::Low => 1,
            Severity::Rejected => 0,
        };
        let mut cases = 0;
        for bits in 0u32..512 {
            let fact = |i: u32| bits & (1 << i) != 0;
            for &severity in &severities {
                let mut r: roles::Rating = serde_json::from_value(serde_json::json!({
                    "title": "t", "severity": format!("{severity:?}")
                }))
                .unwrap();
                r.relevant = fact(0);
                r.on_task = fact(1);
                r.pre_existing = fact(2);
                r.claims_fix = fact(3);
                r.worsens = fact(4);
                r.weak_pin = fact(5);
                r.misleads = fact(6);
                r.equivalent = fact(7);
                r.meets_blocker = fact(8);
                let ruled = ruling(&cfg, &lens, &r);
                cases += 1;
                let real = r.relevant && !r.equivalent && severity != Severity::Rejected;
                // Only what isn't a real defect is dropped.
                assert_eq!(ruled.action == Action::Drop, !real, "{r:?} -> {ruled:?}");
                if !real {
                    continue;
                }
                let raised = r.weak_pin
                    || r.misleads
                    || (r.pre_existing && (r.claims_fix || r.worsens))
                    || (r.meets_blocker && !r.pre_existing);
                if raised {
                    assert!(rank(ruled.severity) >= 3, "{r:?} -> {ruled:?}");
                    assert!(rank(ruled.severity) >= rank(severity));
                } else if r.pre_existing {
                    // The one lowering rule for a real defect: logged, and a
                    // Critical one goes to a person.
                    assert_eq!(ruled.action, Action::NeverBlocks);
                    assert_eq!(ruled.escalate, severity == Severity::Critical);
                } else {
                    // Otherwise the Judge's severity stands, on task or not.
                    assert_eq!(ruled.severity, severity, "{r:?} -> {ruled:?}");
                }
            }
        }
        assert_eq!(cases, 512 * 5);
    }

    #[test]
    fn the_diff_settles_that_a_finding_in_an_added_line_isnt_pre_existing() {
        let changed = stages::changed_lines(
            "+++ b/test/a_test.dart\n@@ -3,1 +5,3 @@\n-moved();\n+x\n+y\n+  moved();\n",
        );
        // A line the diff only moves isn't new.
        assert!(!changed["test/a_test.dart"].contains_key(&7));
        let rating = |line: u64| -> roles::Rating {
            roles::read_ratings(&format!(
                r#"{{"findings": [{{"title": "t", "severity": "Low", "file": "test/a_test.dart", "line": {line}, "pre_existing": true, "worsens": true}}]}}"#
            ))
            .unwrap()
            .remove(0)
        };
        assert!(!grounded(rating(6), &changed, None).pre_existing);
        assert!(grounded(rating(40), &changed, None).pre_existing);
        // So a new test's flaw isn't raised as a worsened old defect.
        let cfg = plain("exception", 3, 1);
        let ruled = ruling(&cfg, &cfg.lenses[0], &grounded(rating(6), &changed, None));
        assert_eq!(ruled.severity, Severity::Low);
    }

    #[test]
    fn a_weak_pin_or_misleading_claim_holds_only_with_a_quote_the_change_adds() {
        let changed = stages::changed_lines(
            &[
                "+++ b/test/edit_test.dart",
                "@@ -1,2 +1,4 @@",
                "-  // old words that were here before",
                "+  testWidgets('an edit keeps the rest of it', (t) async {",
                "+  // Notices for the old date are",
                "+  // cancelled when the date moves.",
                "   unchanged context line about a claim",
            ]
            .join("\n"),
        );
        let judged = |facts: &str| -> roles::Rating {
            let r = roles::read_ratings(&format!(
                r#"{{"findings": [{{"title": "t", "severity": "Low", {facts}}}]}}"#
            ))
            .unwrap()
            .remove(0);
            grounded(r, &changed, None)
        };
        // The test's name, quoted with or without its marks and case.
        assert!(
            judged(r#""weak_pin": true, "weak_pin_test": "An edit keeps the rest of it""#).weak_pin
        );
        // A claim over two comment lines, and one that skips words.
        assert!(
            judged(
                r#""misleads": true, "misleading_claim": "notices for the old date are cancelled""#
            )
            .misleads
        );
        assert!(
            judged(r#""misleads": true, "misleading_claim": ["old date ... when the date moves"]"#)
                .misleads
        );
        // No quote, a quote the change doesn't add, or one too short: false.
        assert!(!judged(r#""weak_pin": true"#).weak_pin);
        assert!(
            !judged(r#""misleads": true, "misleading_claim": "old words that were here before""#)
                .misleads
        );
        assert!(
            !judged(r#""misleads": true, "misleading_claim": "unchanged context line""#).misleads
        );
        assert!(!judged(r#""weak_pin": true, "weak_pin_test": "edit""#).weak_pin);
        // So the rules raise only what the change itself says.
        let cfg = plain("exception", 3, 1);
        let raised = |facts: &str| ruling(&cfg, &cfg.lenses[0], &judged(facts)).severity;
        assert_eq!(raised(r#""misleads": true"#), Severity::Low);
        assert_eq!(
            raised(r#""misleads": true, "misleading_claim": "cancelled when the date moves""#),
            Severity::High
        );
    }

    #[test]
    fn a_finding_blocks_by_the_lenss_definition_only_quoting_it() {
        let changed = stages::changed_lines("+++ b/lib/a.dart\n@@ -1,1 +1,1 @@\n+x\n");
        let blocker = "a wrong result, a wrong or overstated claim, or a test that passes for \
                       the wrong reason; not style";
        let judged = |facts: &str, def: Option<&str>| -> roles::Rating {
            let r = roles::read_ratings(&format!(
                r#"{{"findings": [{{"title": "t", "severity": "Low", {facts}}}]}}"#
            ))
            .unwrap()
            .remove(0);
            grounded(r, &changed, def)
        };
        let meets = r#""meets_blocker": true, "blocker_clause": "a wrong or overstated claim""#;
        assert!(judged(meets, Some(blocker)).meets_blocker);
        // No quote, words the definition doesn't have, or a lens with no
        // definition: false.
        assert!(!judged(r#""meets_blocker": true"#, Some(blocker)).meets_blocker);
        assert!(
            !judged(
                r#""meets_blocker": true, "blocker_clause": "anything that looks untidy""#,
                Some(blocker)
            )
            .meets_blocker
        );
        assert!(!judged(meets, None).meets_blocker);
        // Meeting it raises a Low to High, so it blocks; a defect that was
        // there before the change stays logged.
        let cfg = plain("exception", 3, 1);
        let ruled = ruling(&cfg, &cfg.lenses[0], &judged(meets, Some(blocker)));
        assert_eq!(ruled.severity, Severity::High);
        let old = judged(&format!("{meets}, \"pre_existing\": true"), Some(blocker));
        assert_eq!(
            ruling(&cfg, &cfg.lenses[0], &old).action,
            Action::NeverBlocks
        );
        // The Judge and the reviewers are given the definition; without
        // one, neither is asked about it.
        let mut lens = cfg.lenses[0].clone();
        lens.blocker = Some(blocker.into());
        let prompt = roles::judge_prompt(&lens, &[Vec::new()], "").unwrap();
        assert!(prompt.contains(blocker) && prompt.contains("\"blocker_clause\""));
        let plain_prompt = roles::judge_prompt(&cfg.lenses[0], &[Vec::new()], "").unwrap();
        assert!(!plain_prompt.contains("meets_blocker"));
        let review = roles::reviewer_prompt(&lens, "the change", "", false, "");
        assert!(review.contains(&format!("What blocks here: {blocker}")));
    }

    #[test]
    fn a_calibration_fingerprints_every_rule() {
        let rules = rules_source();
        assert!(
            !rules.contains(super::super::calibration::MISSING),
            "a rule named for the fingerprint isn't where it's named"
        );
        for f in [
            "fn ruling(",
            "fn grounded(",
            "fn changed_lines(",
            "fn adds_text(",
            "fn quotes_text(",
            "fn quote_parts(",
            "fn in_the_change(",
            "fn changed_tests(",
            "fn added_lines(",
            "fn is_doc(",
            "fn is_test_path(",
            "fn test_modules(",
            "fn closing_line(",
            "fn run_gate(",
            "fn gate_ruling(",
        ] {
            assert!(
                rules.contains(f),
                "{f} is missing from the rules' fingerprint"
            );
        }
    }

    #[test]
    fn rules_decide_what_blocks_from_the_judges_facts() {
        let cfg = plain("exception", 3, 1);
        let lens = cfg.lenses[0].clone();
        let rated = |json: &str| -> (Severity, Action) {
            let r = roles::read_ratings(&format!("{{\"findings\": [{json}]}}")).unwrap();
            let ruled = ruling(&cfg, &lens, &r[0]);
            (ruled.severity, ruled.action)
        };
        // A test the change adds that can't fail when what it pins breaks.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Low", "weak_pin": true}"#),
            (Severity::High, Action::Blocks)
        );
        // A claim that would lead someone to ship a defect.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium", "misleads": true}"#).0,
            Severity::High
        );
        // There before, neither claimed fixed nor made worse: logged.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium", "pre_existing": true}"#),
            (Severity::Low, Action::NeverBlocks)
        );
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium", "pre_existing": true, "worsens": true}"#)
                .0,
            Severity::High
        );
        // A break nobody could see, and a Critical stays Critical.
        assert_eq!(
            rated(r#"{"title": "t", "severity": "High", "equivalent_mutant": true}"#).1,
            Action::Drop
        );
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Critical", "weak_pin": true}"#).0,
            Severity::Critical
        );
        assert_eq!(
            rated(r#"{"title": "t", "severity": "Medium"}"#),
            (Severity::Medium, Action::NeverBlocks)
        );
    }

    #[test]
    fn a_lens_can_change_what_a_medium_does() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, severity: {Medium: never-blocks}}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("gamma") => Ok(ONE.into()),
            Role::Reviewer => Ok(CLEAN.into()),
            Role::Judge => Ok(rating("Medium")),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn per_round_lets_a_person_decide_what_goes_in() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Fold));
        let id = run.lenses["alpha"].rounds[0].findings[0].id.clone();
        let fixes = |fake: &Fake<_>| {
            let calls: &Mutex<Vec<(Role, String, bool, String)>> = &fake.calls;
            let calls = calls.lock().unwrap();
            calls.iter().filter(|c| c.2).count()
        };
        assert_eq!(fixes(&fake), 0, "nothing's fixed until they say");

        // Every blocker needs an answer, and a real one.
        let err = resume(&mut run.clone(), &cfg, &c, &fake, Decision::default()).unwrap_err();
        assert!(
            err.to_string().contains("--fold, --waive or --defer"),
            "{err}"
        );
        let wrong = Decision {
            fold: vec![id.clone(), "no-such".into()],
            ..Decision::default()
        };
        assert!(resume(&mut run.clone(), &cfg, &c, &fake, wrong).is_err());
        // A High can't be waived.
        let waive = Decision {
            waive: vec![id.clone()],
            ..Decision::default()
        };
        let err = resume(&mut run.clone(), &cfg, &c, &fake, waive).unwrap_err();
        assert!(err.to_string().contains("can't be waived"), "{err}");

        let fold = Decision {
            fold: vec![id],
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, fold).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(fixes(&fake), 1);
    }

    #[test]
    fn a_medium_a_lens_makes_block_can_be_waived() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, severity: {Medium: blocks-unless-waived}}"#;
        let cfg = config("per-round", 3, 1, extra, "");
        let fake = Fake::new(|role, prompt: &str| match role {
            Role::Reviewer if prompt.contains("reviewer for gamma") => Ok(ONE.into()),
            Role::Reviewer => Ok(CLEAN.into()),
            Role::Judge => Ok(rating("Medium")),
            _ => Ok(NOTHING.into()),
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        let id = run.lenses["gamma"].rounds[0].findings[0].id.clone();
        let waive = Decision {
            waive: vec![id],
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, waive).unwrap();
        assert_eq!(run.lenses["gamma"].outcome, Outcome::Waived);
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn the_ends_gate_asks_about_the_scope_and_the_result() {
        let cfg = plain("ends", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Scope));
        assert_eq!(fake.count(Role::Reviewer), 0);
        let approve = || Decision {
            approve: true,
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, approve()).unwrap();
        assert_eq!(run.waiting, Some(Gate::Final));
        resume(&mut run, &cfg, &c, &fake, approve()).unwrap();
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn a_person_can_stop_a_review() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(defect_in("alpha", 9));
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        let stop = Decision {
            stop: true,
            ..Decision::default()
        };
        resume(&mut run, &cfg, &c, &fake, stop).unwrap();
        assert_eq!(run.status, Status::Stopped);
    }

    /// A person's answers to what the review asks now, by each question's
    /// header, keyed as AskUserQuestion keys them: by the question's text.
    fn answered(
        run: &Run,
        cfg: &Config,
        by_header: &[(&str, serde_json::Value)],
    ) -> BTreeMap<String, serde_json::Value> {
        let ask = super::super::carousel::ask(run, cfg, "acrbus");
        ask.carousels
            .iter()
            .flat_map(|c| &c.questions)
            .filter_map(|q| {
                by_header
                    .iter()
                    .find(|(h, _)| *h == q.header)
                    .map(|(_, v)| (q.question.clone(), v.clone()))
            })
            .collect()
    }

    fn headers(run: &Run, cfg: &Config) -> Vec<String> {
        let ask = super::super::carousel::ask(run, cfg, "acrbus");
        assert!(
            ask.carousels
                .iter()
                .all(|c| (1..=4).contains(&c.questions.len())),
            "a carousel holds 1 to 4 questions"
        );
        ask.carousels
            .iter()
            .flat_map(|c| &c.questions)
            .inspect(|q| {
                assert!(q.header.chars().count() <= 12, "{}", q.header);
                assert!((2..=4).contains(&q.options.len()), "{}", q.question);
            })
            .map(|q| q.header.clone())
            .collect()
    }

    #[test]
    fn a_person_in_the_loop_approves_the_scope_decides_each_blocker_and_the_result() {
        let cfg = plain("in-the-loop", 3, 1);
        let fixed = std::sync::atomic::AtomicBool::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer
                    if prompt.contains("alpha")
                        && !fixed.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let again = c.clone();
        let reload = move |_| Ok(again.clone());
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        // The scope first, who fixes, and, with nothing chosen, which lenses
        // attack again after a fix, before any reviewer runs.
        assert_eq!(run.waiting, Some(Gate::Scope));
        assert_eq!(headers(&run, &cfg), ["Scope", "Fixer", "Re-attack"]);
        assert_eq!(fake.count(Role::Reviewer), 0);
        // The default fixer comes first, marked: an Orchestrator that may
        // write is it.
        assert_eq!(run.fixer, Fixer::Orchestrator);
        let fixer_options = |run: &Run| -> Vec<(String, String)> {
            super::super::carousel::ask(run, &cfg, "acrbus")
                .carousels
                .iter()
                .flat_map(|c| &c.questions)
                .find(|q| q.header == "Fixer")
                .map(|q| {
                    q.options
                        .iter()
                        .map(|o| (o.label.clone(), o.description.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let offered = fixer_options(&run);
        assert_eq!(offered[0].0, "The Orchestrator", "{offered:?}");
        assert!(offered[0].1.starts_with("The default."), "{offered:?}");
        assert!(!offered[1].1.contains("default"), "{offered:?}");
        // With the session as the default, it comes first instead.
        let mut theirs = run.clone();
        theirs.fixer = Fixer::Session;
        let offered = fixer_options(&theirs);
        assert_eq!(offered[0].0, "This session", "{offered:?}");
        assert!(offered[0].1.starts_with("The default."), "{offered:?}");
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "Approve".into()),
                ("Fixer", "This session".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        assert_eq!(run.fixer, Fixer::Session);
        drive(&mut run, &s, &fake, &reload).unwrap();
        // Then each blocker, and what the reviewers missed.
        assert_eq!(run.waiting, Some(Gate::Fold));
        assert_eq!(headers(&run, &cfg), ["Blocker 1", "Missed"]);
        let a = answered(
            &run,
            &cfg,
            &[
                ("Blocker 1", "guard it with a lock".into()),
                ("Missed", "the README still names the old flag".into()),
            ],
        );
        let applied = super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        // Each verdict and each missed defect is feedback for the calibration.
        assert_eq!(applied.feedback.len(), 2, "{:?}", applied.feedback);
        assert_eq!(
            applied.feedback[0].note.as_deref(),
            Some("To the fixer: guard it with a lock")
        );
        assert_eq!(
            applied.feedback[1].missed.as_deref(),
            Some("the README still names the old flag")
        );
        drive(&mut run, &s, &fake, &reload).unwrap();
        // The session fixes, with the person's note and the missed defect.
        assert_eq!(run.waiting, Some(Gate::Fix));
        assert_eq!(fake.count(Role::Orchestrator), 0, "nobody else fixes");
        let brief = fix_brief(&run);
        assert!(
            brief.contains("alpha-r1-1")
                && brief.contains("Your user says: guard it with a lock")
                && brief.contains("you-1 (your user found it")
                && brief.contains("the README still names the old flag"),
            "{brief}"
        );
        assert!(
            super::super::carousel::ask(&run, &cfg, "acrbus")
                .then
                .contains("acr resume r --fixed")
        );
        fixed.store(true, std::sync::atomic::Ordering::SeqCst);
        fixed_by_session(&mut run, &s, &fake, BTreeMap::new()).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        // Every lens attacks the fix, since a missed defect is nobody's lens's.
        assert_eq!(run.round, 2);
        assert!(run.lenses.values().all(|l| l.rounds.len() == 2));
        assert!(run.reported.iter().all(|r| r.fixed));
        // And the result is the person's to approve.
        assert_eq!(run.waiting, Some(Gate::Final));
        let a = answered(&run, &cfg, &[("Result", "Approve".into())]);
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        assert_eq!(run.status, Status::Confirmed);
    }

    #[test]
    fn a_values_call_is_the_persons_and_their_ruling_stands() {
        let cfg = plain("exception", 3, 1);
        let judged = std::sync::atomic::AtomicU32::new(0);
        let fake =
            Fake::new(|role, prompt: &str| match role {
                Role::Reviewer if judged.load(std::sync::atomic::Ordering::SeqCst) < 2 => {
                    Ok(ONE.into())
                }
                Role::Reviewer => Ok(CLEAN.into()),
                Role::Judge => {
                    judged.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(r#"{"findings": [{"title": "t", "severity": "Rejected", "values_call": true,
                    "sides": ["speed", "safety"]}]}"#
                    .into())
                }
                Role::Orchestrator if prompt.contains("Fix these") => {
                    Ok(r#"{"done": "fixed it"}"#.into())
                }
                _ => Ok(NOTHING.into()),
            });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert_eq!(headers(&run, &cfg), ["Values call", "Values call"]);
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        let q = &ask.carousels[0].questions[0];
        assert!(
            q.question.contains("speed") && q.question.contains("safety"),
            "{}",
            q.question
        );
        assert!(
            q.question.contains("It's ruled not to block"),
            "{}",
            q.question
        );
        // The person rules alpha's blocks, and beta's as the Judge did.
        let mut a = BTreeMap::new();
        for q in ask.carousels.iter().flat_map(|c| &c.questions) {
            let call = if q.question.contains("alpha-r1-1") {
                "It blocks"
            } else {
                "As ruled"
            };
            a.insert(q.question.clone(), serde_json::json!(call));
        }
        let applied = super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        let verdicts: Vec<Option<super::super::configure::Verdict>> =
            applied.feedback.iter().map(|f| f.verdict).collect();
        assert!(verdicts.contains(&Some(super::super::configure::Verdict::ShouldBlock)));
        assert!(verdicts.contains(&Some(super::super::configure::Verdict::Right)));
        let raised = &run.lenses["alpha"].rounds[0].findings[0];
        assert!(
            raised.blocks() && raised.state == FindingState::Folded,
            "{raised:?}"
        );
        assert_eq!(
            run.lenses["beta"].rounds[0].findings[0].state,
            FindingState::Dropped
        );
        // It's fixed, and the next round attacks the fix.
        let s = setting(&cfg);
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert!(
            fake.count(Role::Orchestrator) > 0,
            "the Orchestrator fixes it"
        );
        assert!(run.round >= 2, "{:?}", run.status);
    }

    #[test]
    fn the_person_can_stop_anywhere_and_must_answer_whats_asked() {
        // Sized small, below the tier its paths would choose.
        let cfg = {
            let mut cfg = plain("in-the-loop", 3, 1);
            for t in cfg.tiers.iter_mut() {
                t.rule.paths.clear();
            }
            cfg
        };
        let fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Sizer => r#"{"tier": "small", "sure": true, "reason": "small"}"#.into(),
                _ => CLEAN.to_string(),
            })
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Scope));
        assert_eq!(run.tier, "small");
        // What's needed is said, by its header.
        let err = format!(
            "{:#}",
            super::super::carousel::apply(&mut run, &cfg, &BTreeMap::new(), 1).unwrap_err()
        );
        assert!(err.contains("[Scope]") && err.contains("[Fixer]"), "{err}");
        // Words of their own don't stand for a choice where none is offered.
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "sure, why not".into()),
                ("Fixer", "This session".into()),
            ],
        );
        let err = format!(
            "{:#}",
            super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap_err()
        );
        assert!(err.contains("choose one of Approve"), "{err}");
        // A raised tier brings its lenses, and is asked about again.
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "One tier higher".into()),
                ("Fixer", "This session".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        assert!(run.rescope);
        assert_eq!((run.tier.as_str(), run.fan_out), ("big", 1));
        let s = setting(&cfg);
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Scope));
        assert!(!run.rescope);
        // At the top tier, there's no higher one to offer.
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        assert!(
            ask.carousels[0].questions[0]
                .options
                .iter()
                .all(|o| o.label != "One tier higher")
        );
        // And stopping ends it, with no reviewer run.
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "Stop the review".into()),
                ("Fixer", "This session".into()),
            ],
        );
        let applied = super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        assert!(applied.stopped);
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_ne!(run.status, Status::Waiting);
        assert_eq!(fake.count(Role::Reviewer), 0);
    }

    #[test]
    fn a_lens_is_judged_as_soon_as_its_own_reviewers_finish() {
        // beta's reviewer answers only once alpha's Judge has started: with
        // every Judge waiting for the slowest lens, it would wait in vain.
        let cfg = plain("exception", 1, 1);
        let alpha_judged = std::sync::atomic::AtomicBool::new(false);
        let saw = std::sync::atomic::AtomicBool::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            use std::sync::atomic::Ordering::SeqCst;
            Ok(match role {
                Role::Reviewer if prompt.contains("beta criteria") => {
                    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !alpha_judged.load(SeqCst) && std::time::Instant::now() < until {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    saw.store(alpha_judged.load(SeqCst), SeqCst);
                    CLEAN.to_string()
                }
                Role::Reviewer => ONE.to_string(),
                Role::Judge => {
                    if prompt.contains("alpha criteria") {
                        alpha_judged.store(true, SeqCst);
                    }
                    rating("Low")
                }
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(
            saw.load(std::sync::atomic::Ordering::SeqCst),
            "alpha's Judge waited for beta's reviewer"
        );
        assert_eq!(run.lenses["alpha"].rounds[0].findings.len(), 1);
    }

    #[test]
    fn one_defect_two_lenses_found_is_decided_once_and_fixed_once() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer => ONE.to_string(),
                Role::Judge => rating("High"),
                Role::Sizer if prompt.contains("the same defect") => {
                    r#"{"same": [["alpha-r1-1", "beta-r1-1"]]}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Fold));
        let beta = &run.lenses["beta"].rounds[0].findings[0];
        assert_eq!(beta.same_as.as_deref(), Some("alpha-r1-1"));
        assert!(beta.blocks(), "it keeps its own ruling");
        // Asked about once, as found by both.
        assert_eq!(headers(&run, &cfg), ["Blocker 1", "Missed"]);
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        let q = &ask.carousels[0].questions[0];
        assert!(q.question.contains("also found by beta"), "{}", q.question);
        // One answer decides both, and the fixer reads it once.
        let a = answered(&run, &cfg, &[("Blocker 1", "Fix it".into())]);
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        for lens in ["alpha", "beta"] {
            assert_eq!(
                run.lenses[lens].rounds[0].findings[0].state,
                FindingState::Folded,
                "{lens}"
            );
        }
        let brief = fix_list(&to_fix(&run), &[]);
        assert_eq!(brief.matches("a defect").count(), 1, "{brief}");
        assert!(
            brief.contains("The same defect was found by: beta"),
            "{brief}"
        );
        let _ = c;
    }

    fn named(id: &str, file: &str, line: u64, title: &str, detail: &str) -> Finding {
        Finding {
            id: id.into(),
            title: title.into(),
            detail: detail.into(),
            file: Some(file.into()),
            line: Some(line),
            severity: Severity::High,
            action: Action::Blocks,
            reason: String::new(),
            state: FindingState::Open,
            note: String::new(),
            same_as: None,
            reused_from: None,
            ruled_by: None,
        }
    }

    #[test]
    fn identifiers_are_distinctive_names_not_common_words() {
        let ids = identifiers(
            "Unbounded `pendingFireErrs` growth: each fire_err appends, the `error` and `ctx` \
             stay, Registry.drainAll never runs, and the host doesn't drain",
        );
        for name in ["pendingFireErrs", "fire_err", "drainAll"] {
            assert!(ids.contains(name), "{name} in {ids:?}");
        }
        for word in ["error", "ctx", "growth", "host", "Registry", "drain"] {
            assert!(!ids.contains(word), "{word} in {ids:?}");
        }
    }

    #[test]
    fn one_defect_at_two_places_is_proposed_by_the_name_both_findings_use() {
        let found = vec![
            (
                "alpha".to_string(),
                named(
                    "alpha-r1-1",
                    "core/registry.go",
                    806,
                    "Unbounded pendingFireErrs growth",
                    "Each fire appends to `pendingFireErrs`, and a host that doesn't drain keeps it all",
                ),
            ),
            (
                "beta".to_string(),
                named(
                    "beta-r1-2",
                    "core/registry.go",
                    158,
                    "pendingFireErrs is never drained",
                    "Hosts that don't drain let `pendingFireErrs` grow without bound",
                ),
            ),
        ];
        assert_eq!(
            candidates(&found),
            [Candidate {
                a: "alpha-r1-1".into(),
                b: "beta-r1-2".into(),
                shared: vec!["pendingFireErrs".into()],
            }]
        );
    }

    #[test]
    fn a_shared_common_word_or_different_names_make_no_candidate() {
        let pair = |a: (&str, &str), b: (&str, &str)| {
            vec![
                (
                    "alpha".to_string(),
                    named("alpha-r1-1", "core/x.go", 10, a.0, a.1),
                ),
                (
                    "beta".to_string(),
                    named("beta-r1-1", "core/x.go", 90, b.0, b.1),
                ),
            ]
        };
        // Only a common word in common.
        assert!(
            candidates(&pair(
                (
                    "The error is returned unwrapped",
                    "the `error` loses its context"
                ),
                (
                    "The error path ignores the context",
                    "an `error` here is dropped"
                ),
            ))
            .is_empty()
        );
        // Different names in the same file.
        assert!(
            candidates(&pair(
                (
                    "pendingFireErrs grows",
                    "`pendingFireErrs` is never drained"
                ),
                (
                    "retryBudget is reset",
                    "`retryBudget` goes to zero on reload"
                ),
            ))
            .is_empty()
        );
        // Different files that don't name each other.
        let apart = vec![
            (
                "alpha".to_string(),
                named(
                    "alpha-r1-1",
                    "core/x.go",
                    10,
                    "pendingFireErrs grows",
                    "unbounded",
                ),
            ),
            (
                "beta".to_string(),
                named(
                    "beta-r1-1",
                    "other/y.go",
                    10,
                    "pendingFireErrs grows",
                    "unbounded",
                ),
            ),
        ];
        assert!(candidates(&apart).is_empty());
    }

    /// Two lenses' findings of one defect at two places of a.src, both
    /// naming `pendingFireErrs`; the dedupe groups them only when the pair
    /// is put to it, and only if `merge` says so.
    fn at_two_places(merge: bool) -> (Run, Config) {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(move |role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha criteria") => r#"{"findings": [{"title": "Unbounded pendingFireErrs growth", "detail": "Each fire appends to `pendingFireErrs` and nothing drains it", "file": "a.src", "line": 3}]}"#.to_string(),
                Role::Reviewer => r#"{"findings": [{"title": "pendingFireErrs never drained", "detail": "Hosts that don't drain let `pendingFireErrs` grow", "file": "a.src", "line": 40}]}"#.to_string(),
                // The Judge restates each finding as it rules it.
                Role::Judge if prompt.contains("alpha criteria") => r#"{"findings": [{"title": "Unbounded pendingFireErrs growth", "detail": "Each fire appends to `pendingFireErrs` and nothing drains it", "file": "a.src", "line": 3, "severity": "High"}]}"#.to_string(),
                Role::Judge => r#"{"findings": [{"title": "pendingFireErrs never drained", "detail": "Hosts that don't drain let `pendingFireErrs` grow", "file": "a.src", "line": 40, "severity": "High"}]}"#.to_string(),
                Role::Sizer
                    if prompt.contains(
                        "alpha-r1-1 at a.src:3 and beta-r1-1 at a.src:40: both name `pendingFireErrs`",
                    ) =>
                {
                    if merge {
                        r#"{"same": [["alpha-r1-1", "beta-r1-1"]]}"#.to_string()
                    } else {
                        r#"{"same": []}"#.to_string()
                    }
                }
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        (run, cfg)
    }

    #[test]
    fn one_defect_two_lenses_found_at_two_places_is_one_blocker_with_both_places() {
        let (mut run, cfg) = at_two_places(true);
        let beta = &run.lenses["beta"].rounds[0].findings[0];
        assert_eq!(
            beta.same_as.as_deref(),
            Some("alpha-r1-1"),
            "{:?}",
            run.lenses
        );
        // Asked once, with both places and both lenses.
        assert_eq!(headers(&run, &cfg), ["Blocker 1", "Missed"]);
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        let q = &ask.carousels[0].questions[0].question;
        assert!(q.contains("also found by beta at a.src:40"), "{q}");
        assert!(q.contains("found by 2 lenses at 2 places"), "{q}");
        assert!(q.contains("At a.src:3."), "{q}");
        // The fixer reads it once, with every place to fix.
        let a = answered(&run, &cfg, &[("Blocker 1", "Fix it".into())]);
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        let brief = fix_list(&to_fix(&run), &[]);
        assert_eq!(brief.matches("- alpha-r1-1").count(), 1, "{brief}");
        assert!(!brief.contains("- beta-r1-1"), "{brief}");
        assert!(brief.contains("beta (at a.src:40)"), "{brief}");
        assert!(
            brief.contains("fix it at each: a.src:3, a.src:40"),
            "{brief}"
        );
        let note = run
            .lenses
            .values()
            .flat_map(|l| l.rounds.iter().flat_map(|r| r.notes.iter()))
            .find(|n| n.contains("put to the dedupe"))
            .expect("the pair is noted");
        assert!(note.contains("ruled 1 the same defect"), "{note}");
    }

    #[test]
    fn a_pair_the_dedupe_rules_different_stays_two_blockers() {
        let (run, cfg) = at_two_places(false);
        assert!(
            run.lenses
                .values()
                .all(|l| l.rounds[0].findings.iter().all(|f| f.same_as.is_none()))
        );
        assert_eq!(headers(&run, &cfg), ["Blocker 1", "Blocker 2", "Missed"]);
        let note = run
            .lenses
            .values()
            .flat_map(|l| l.rounds.iter().flat_map(|r| r.notes.iter()))
            .find(|n| n.contains("put to the dedupe"))
            .expect("the pair is noted");
        assert!(note.contains("ruled 0 the same defect"), "{note}");
        // It was the dedupe's ruling on the pair that kept them apart, not
        // an answer it couldn't read.
        assert!(
            run.lenses
                .values()
                .flat_map(|l| l.rounds.iter().flat_map(|r| r.notes.iter()))
                .all(|n| !n.contains("weren't grouped")),
            "{:?}",
            run.lenses
        );
    }

    #[test]
    fn a_blocker_deferred_to_a_ticket_neither_blocks_nor_is_waived_and_gets_a_draft() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Fold));
        // Deferring is a choice, beside fixing; a High blocker can't be
        // waived, so the question says nothing of waiving.
        assert_eq!(headers(&run, &cfg), ["Blocker 1", "Missed"]);
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        let q = &ask.carousels[0].questions[0];
        let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["Fix it", "Defer to a ticket", "Not a defect"]);
        assert!(
            q.question.contains("\"Defer to a ticket: <your note>\""),
            "{}",
            q.question
        );
        assert!(!q.question.contains("Waive it"), "{}", q.question);
        // Typed "Waive it" on what can't be waived is refused, not fixed.
        let mut tried = run.clone();
        let waived = answered(&tried, &cfg, &[("Blocker 1", "Waive it".into())]);
        let err = super::super::carousel::apply(&mut tried, &cfg, &waived, 1).unwrap_err();
        assert!(format!("{err:#}").contains("can't be waived"), "{err:#}");
        // Deferred with a note.
        let a = answered(
            &run,
            &cfg,
            &[(
                "Blocker 1",
                "Defer to a ticket: after the release, with the migration".into(),
            )],
        );
        let applied = super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        let f = &run.lenses["alpha"].rounds[0].findings[0];
        assert_eq!(f.state, FindingState::Deferred);
        assert_eq!(f.note, "after the release, with the migration");
        assert!(f.reason.ends_with("A person deferred it to a ticket."));
        assert_eq!(
            applied.feedback[0].note.as_deref(),
            Some("Deferred to a ticket: after the release, with the migration")
        );
        // It doesn't block: nothing is fixed, and the review ends.
        let again = c.clone();
        drive(&mut run, &setting(&cfg), &fake, &|_| Ok(again.clone())).unwrap();
        assert_eq!(fake.count(Role::Orchestrator), 0, "nothing to fix");
        assert_ne!(run.status, Status::Waiting);
        assert_ne!(run.lenses["alpha"].outcome, Outcome::Open);
        // And a draft says what's owed.
        let drafts = super::super::report::tickets(&run);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].finding, "alpha-r1-1");
        assert_eq!(drafts[0].title, "a defect");
        for want in [
            "Deferred from review r (round 1, lens alpha)",
            "`a.src`",
            "**What was found:**\n\na defect",
            "after the release, with the migration",
        ] {
            assert!(drafts[0].body.contains(want), "{want}: {}", drafts[0].body);
        }
        let report = super::super::report::report(&run, &cfg);
        assert_eq!(report.tickets.len(), 1);
        assert!(
            !drafts[0].body.contains("Why it blocked"),
            "no reason, no section: {}",
            drafts[0].body
        );
    }

    #[test]
    fn defer_decides_any_blocker_and_waive_still_only_what_can_be() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let mut waived = run.clone();
        let err = decide(
            &mut waived,
            &Decision {
                waive: vec!["alpha-r1-1".into()],
                ..Decision::default()
            },
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("defer it to a ticket"),
            "{err:#}"
        );
        let mut undecided = run.clone();
        let err = decide(&mut undecided, &Decision::default()).unwrap_err();
        assert!(format!("{err:#}").contains("--defer"), "{err:#}");
        let mut deferred = run;
        decide(
            &mut deferred,
            &Decision {
                defer: vec!["alpha-r1-1".into()],
                ..Decision::default()
            },
        )
        .unwrap();
        assert_eq!(
            deferred.lenses["alpha"].rounds[0].findings[0].state,
            FindingState::Deferred
        );
        assert_eq!(deferred.stage, Stage::Done, "nothing to fix");
    }

    #[test]
    fn a_review_ends_where_it_stands_waiting_or_not() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let (mut run, c) = go(&cfg, &["a.src"], &fake);
        // As a process that was stopped mid-round leaves it: running, and
        // waiting for nobody.
        run.status = Status::Running;
        run.waiting = None;
        run.stage = Stage::Review;
        run.escalations.push(Escalation {
            reason: "unseen".into(),
            seen: false,
            about: None,
        });
        let err = decide(
            &mut run.clone(),
            &Decision {
                approve: true,
                ..Decision::default()
            },
        );
        assert!(format!("{:#}", err.unwrap_err()).contains("isn't waiting"));
        decide(
            &mut run,
            &Decision {
                stop: true,
                ..Decision::default()
            },
        )
        .unwrap();
        let again = c.clone();
        drive(&mut run, &setting(&cfg), &fake, &|_| Ok(again.clone())).unwrap();
        assert_eq!(run.status, Status::Stopped);
    }

    #[test]
    fn findings_go_ungrouped_and_say_so_when_the_grouping_fails() {
        let cfg = plain("per-round", 3, 1);
        let fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Reviewer => ONE.to_string(),
                Role::Judge => rating("High"),
                _ => "no idea".to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let notes: Vec<&String> = run
            .lenses
            .values()
            .flat_map(|l| &l.rounds[0].notes)
            .collect();
        assert!(
            notes.iter().any(|n| n.contains("weren't grouped")),
            "{notes:?}"
        );
        assert!(
            run.lenses
                .values()
                .all(|l| l.rounds[0].findings.iter().all(|f| f.same_as.is_none()))
        );
        assert_eq!(headers(&run, &cfg), ["Blocker 1", "Blocker 2", "Missed"]);
    }

    #[test]
    fn split_reviewers_and_values_calls_escalate_in_any_mode() {
        for body in [
            r#"{"findings": [{"title": "t", "severity": "Low", "split": true,
                "sides": ["it's a bug", "it's fine"]}]}"#,
            r#"{"findings": [{"title": "t", "severity": "Rejected", "values_call": true,
                "sides": ["speed", "safety"]}]}"#,
        ] {
            let cfg = plain("exception", 3, 1);
            let fake = Fake::new(|role, _: &str| match role {
                Role::Reviewer => Ok(ONE.into()),
                Role::Judge => Ok(body.to_string()),
                _ => Ok(NOTHING.into()),
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(run.waiting, Some(Gate::Escalation));
            assert_eq!(run.disagreements[0].sides.len(), 2);
        }
    }

    #[test]
    fn a_plain_rejection_doesnt_escalate() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(rating("Rejected")),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert!(run.escalations.is_empty());
    }

    #[test]
    fn always_escalate_reaches_a_person_whatever_the_gate() {
        let top = "always_escalate: [\"a public API change\"]";
        let cfg = config("exception", 3, 1, "", top);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Orchestrator => {
                Ok(r#"{"lenses": [], "escalate": ["a public API change", "made up"]}"#.into())
            }
            _ => Ok(CLEAN.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert_eq!(run.escalations.len(), 1, "only what the config names");
        assert!(run.escalations[0].reason.contains("public API"));
    }

    #[test]
    fn changing_the_config_always_escalates() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Sizer => Ok(r#"{"tier": "big", "sure": true}"#.into()),
            _ => Ok(CLEAN.into()),
        });
        let (run, _) = go(&cfg, &[super::super::config::REPO_CONFIG], &fake);
        assert_eq!(run.waiting, Some(Gate::Escalation));
        assert!(run.escalations.iter().any(|e| e.reason.contains("edits")));
    }

    #[test]
    fn an_unsure_sizer_means_the_higher_tier() {
        let cfg = plain("exception", 3, 2);
        for (answer, tier) in [
            (
                r#"{"tier": "small", "sure": true, "reason": "tiny"}"#,
                "small",
            ),
            (r#"{"tier": "small", "sure": false}"#, "big"),
            (r#"{"tier": "made-up", "sure": true}"#, "big"),
            ("no idea", "big"),
        ] {
            let fake = Fake::new(|role, _: &str| match role {
                Role::Sizer => Ok(answer.to_string()),
                _ => Ok(CLEAN.into()),
            });
            let s = setting(&cfg);
            let c = change(&["x.other"]);
            let run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
            assert_eq!(run.tier, tier, "{answer}");
        }
    }

    #[test]
    fn a_lens_only_a_description_triggers_needs_the_orchestrator() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {description: "when it's gamma"}}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let says = |text: &'static str| {
            Fake::new(move |role, _: &str| match role {
                Role::Orchestrator => Ok(text.to_string()),
                _ => Ok(CLEAN.into()),
            })
        };
        let yes = says(r#"{"lenses": ["gamma"]}"#);
        assert!(go(&cfg, &["a.src"], &yes).0.lenses.contains_key("gamma"));
        let no = says(r#"{"lenses": []}"#);
        assert!(!go(&cfg, &["a.src"], &no).0.lenses.contains_key("gamma"));
        // If it can't say, the lens stays in, and a person hears.
        let broken = says("no idea");
        let (run, _) = go(&cfg, &["a.src"], &broken);
        assert!(run.lenses.contains_key("gamma"));
        assert_eq!(run.waiting, Some(Gate::Escalation));
    }

    #[test]
    fn a_lens_can_have_its_own_binding() {
        let extra = r#"  - {id: gamma, criteria: "gamma criteria", triggers: {paths: ["**/*.src"]}, binding: small}"#;
        let cfg = config("exception", 3, 1, extra, "");
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        let gamma = calls
            .iter()
            .find(|c| c.3.contains("gamma criteria"))
            .unwrap();
        assert_eq!(gamma.1, "small");
        let alpha = calls
            .iter()
            .find(|c| c.3.contains("alpha criteria"))
            .unwrap();
        assert_eq!(alpha.1, "medium");
    }

    #[test]
    fn the_brief_and_the_judge_get_the_decided_non_issues() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        for role in [Role::Reviewer, Role::Judge] {
            let call = calls.iter().find(|c| c.0 == role).unwrap();
            assert!(call.3.contains("the decided thing"));
        }
    }

    #[test]
    fn the_report_can_be_read_by_a_machine() {
        let top = "integrations:\n  comment: full\n  labels: {not_ready: nr, ready: rd, waiting: \"wait:{lens}\", confirmed: \"ok:{lens}\"}\n\
                   costs: {prices: {example/small-model: {input_per_mtok: 0.1, output_per_mtok: 0.4}, \
                   example/medium-model: {input_per_mtok: 0.1, output_per_mtok: 0.4}, \
                   example/large-model: {input_per_mtok: 0.1, output_per_mtok: 0.4}}, \
                   per_call: {reviewer: {input_tokens: 1000, output_tokens: 500}}}";
        let cfg = config("exception", 2, 1, "", top);
        let fake = Fake::new(defect_in("alpha", 99));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let json = serde_json::to_value(report::report(&run, &cfg)).unwrap();
        assert_eq!(json["status"], "waiting");
        assert_eq!(json["waiting_for"], "escalation");
        let lenses = json["lenses"].as_array().unwrap();
        let alpha = lenses.iter().find(|l| l["id"] == "alpha").unwrap();
        assert_eq!(alpha["criteria"], "alpha criteria");
        assert_eq!(alpha["outcome"], "open");
        assert_eq!(alpha["rounds"][0]["findings"][0]["severity"], "High");
        assert_eq!(alpha["done"][0], "fixed it");
        let used = json["bindings_used"].as_array().unwrap();
        assert!(
            used.iter()
                .any(|b| b["role"] == "reviewer" && b["binding"] == "medium")
        );
        assert!(json["tokens"].as_u64().unwrap() > 0);
        assert!(json["usd"].as_f64().unwrap() > 0.0);
        assert_eq!(json["calls_measured"], 0);
        assert_eq!(json["agent_usd"], serde_json::Value::Null);
        // A real call counts what the config says its role takes, too.
        for u in run.usage.iter().filter(|u| u.role == Role::Reviewer) {
            assert!(u.tokens_in >= 1000 && u.tokens_out >= 500, "{u:?}");
        }
        assert!(run.usage.iter().all(|u| u.usd.is_some()));
        let labels = report::labels(&run, &cfg);
        assert!(labels.contains(&"nr".to_string()));
        assert!(labels.contains(&"wait:alpha".to_string()));
        assert!(labels.contains(&"ok:beta".to_string()));
        assert!(!labels.contains(&"rd".to_string()));
        let text = report::comment(&run, &cfg).unwrap();
        assert!(text.contains("AI agent"), "{text}");
        assert!(text.contains("Escalated"), "{text}");
    }

    #[test]
    fn a_repository_that_turned_nothing_on_gets_no_labels_or_comment() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert!(report::labels(&run, &cfg).is_empty());
        assert!(report::comment(&run, &cfg).is_none());
    }

    #[test]
    fn a_benchmark_counts_caught_missed_and_false() {
        let cfg = plain("exception", 1, 1);
        let fake = Fake::new(|role, _: &str| match role {
            Role::Reviewer => Ok(ONE.into()),
            Role::Judge => Ok(r#"{"findings": [
                {"title": "off by one in the loop", "file": "a.src", "severity": "High"},
                {"title": "a phantom", "file": "a.src", "severity": "High"},
                {"title": "a nit", "severity": "Low"}]}"#
                .into()),
            _ => Ok(NOTHING.into()),
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let known: Vec<Known> = serde_yaml_ng::from_str(
            "- {id: k1, file: a.src, pattern: 'off by one'}\n- {id: k2, pattern: 'a leak'}\n",
        )
        .unwrap();
        let score = score(&run, &known).unwrap();
        assert_eq!(score.caught, ["k1"]);
        assert_eq!(score.missed, ["k2"]);
        assert_eq!(score.false_findings.len(), 2, "one phantom in each lens");
        assert!(score.false_findings.iter().all(|f| f.contains("phantom")));
        assert!(score.tokens > 0);
    }

    fn diffed(files: &[&str]) -> Change {
        let diff: String = files
            .iter()
            .map(|f| {
                format!("diff --git a/{f} b/{f}\n--- a/{f}\n+++ b/{f}\n@@ -1,1 +1,2 @@\n+one\n+two\n-old\n")
            })
            .collect();
        Change {
            files: files.iter().map(|f| f.to_string()).collect(),
            diff,
            ..Change::default()
        }
    }

    #[test]
    fn the_sizer_decides_from_the_file_list_with_no_tools() {
        let cfg = plain("exception", 1, 1);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Sizer if prompt.contains("you have no tools and no diff") => {
                    r#"{"tier": "small", "sure": true, "reason": "notes only"}"#.into()
                }
                Role::Sizer => r#"{"tier": "big", "sure": true, "reason": "read it"}"#.into(),
                Role::Reviewer => CLEAN.into(),
                _ => NOTHING.into(),
            })
        });
        let s = setting(&cfg);
        let c = diffed(&["notes.txt", "more.txt"]);
        let run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        assert_eq!(run.tier, "small");
        assert!(
            run.tier_why.contains("from the file list"),
            "{}",
            run.tier_why
        );
        assert_eq!(fake.count(Role::Sizer), 1, "one quick call is enough");
        assert_eq!(*fake.bare.lock().unwrap(), [Role::Sizer], "with no tools");
        assert!(
            !fake.with_diff.lock().unwrap().contains(&Role::Sizer),
            "and no diff"
        );
        let calls = fake.calls.lock().unwrap();
        let prompt = &calls.iter().find(|c| c.0 == Role::Sizer).unwrap().3;
        assert!(
            prompt.contains("- notes.txt (+2 -1)") && prompt.contains("- more.txt (+2 -1)"),
            "{prompt}"
        );
        assert!(run.usage.iter().any(|u| u.role == Role::Sizer));
    }

    #[test]
    fn the_sizer_reads_the_change_only_when_the_file_list_cant_tell() {
        let cfg = plain("exception", 1, 1);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Sizer if prompt.contains("you have no tools and no diff") => {
                    r#"{"tier": "unknown", "sure": false, "reason": "need the code"}"#.into()
                }
                Role::Sizer => r#"{"tier": "big", "sure": true, "reason": "read it"}"#.into(),
                Role::Reviewer => CLEAN.into(),
                _ => NOTHING.into(),
            })
        });
        let s = setting(&cfg);
        let c = diffed(&["notes.txt"]);
        let run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        assert_eq!(run.tier, "big");
        assert!(
            run.tier_why.contains("having read the change"),
            "{}",
            run.tier_why
        );
        assert_eq!(fake.count(Role::Sizer), 2);
        assert!(fake.with_diff.lock().unwrap().contains(&Role::Sizer));
        let told = fake.told.lock().unwrap();
        assert!(
            told.iter().any(|t| t
                .starts_with("say: the Sizer couldn't tell from the file list (need the code)")),
            "{told:?}"
        );
    }

    /// Calls answered after a wait set by lens, timed: reviewers of later
    /// lenses take longer, and one lens's reviewer finds nothing.
    struct Staggered {
        times: Mutex<Vec<(String, Instant, Instant)>>,
        said: Mutex<Vec<String>>,
    }

    impl Caller for Staggered {
        fn call(&self, call: &Call) -> Result<Answer> {
            let began = Instant::now();
            let lens = call.label.split(':').next().unwrap_or_default().to_string();
            let wait = ["alpha", "beta", "gamma", "delta", "epsilon"]
                .iter()
                .position(|l| *l == lens)
                .unwrap_or(0) as u64
                * 120;
            let text = match call.role {
                Role::Reviewer => {
                    std::thread::sleep(Duration::from_millis(wait));
                    if lens == "delta" { CLEAN } else { ONE }.to_string()
                }
                Role::Judge => rating("Low"),
                _ => NOTHING.to_string(),
            };
            self.times
                .lock()
                .unwrap()
                .push((call.label.clone(), began, Instant::now()));
            Ok(Answer {
                text,
                run_id: new_id(),
                usage: None,
                secs: None,
            })
        }

        fn say(&self, what: &str) {
            self.said.lock().unwrap().push(what.to_string());
        }
    }

    #[test]
    fn each_lens_judge_starts_as_its_own_reviewers_answer_and_one_with_nothing_says_so() {
        let extra = "  - {id: gamma, criteria: \"gamma criteria\", triggers: {paths: [\"**/*.src\"]}}\n  - {id: delta, criteria: \"delta criteria\", triggers: {paths: [\"**/*.src\"]}}\n  - {id: epsilon, criteria: \"epsilon criteria\", triggers: {paths: [\"**/*.src\"]}}";
        let cfg = config("exception", 1, 1, extra, "");
        let caller = Staggered {
            times: Mutex::new(Vec::new()),
            said: Mutex::new(Vec::new()),
        };
        let s = setting(&cfg);
        fs::create_dir_all(&s.work_dir).unwrap();
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &caller).unwrap();
        drive(&mut run, &s, &caller, &|_| Ok(c.clone())).unwrap();
        let times = caller.times.lock().unwrap().clone();
        let at = |label: &str| times.iter().find(|t| t.0 == label).cloned();
        for lens in ["alpha", "beta", "gamma", "epsilon"] {
            let reviewed = at(&format!("{lens}: reviewer")).unwrap();
            let judged = at(&format!("{lens}: the Judge"))
                .unwrap_or_else(|| panic!("{lens}'s Judge never started: {times:?}"));
            let gap = judged.1.duration_since(reviewed.2);
            assert!(gap < Duration::from_millis(100), "{lens} waited {gap:?}");
        }
        // The first lens's Judge started before the last lens's reviewer
        // answered: nothing waits on the slowest lens.
        let first = at("alpha: the Judge").unwrap();
        let last = at("epsilon: reviewer").unwrap();
        assert!(first.1 < last.2, "{times:?}");
        assert!(at("delta: the Judge").is_none());
        let said = caller.said.lock().unwrap().clone();
        assert!(
            said.contains(&"delta: no findings, nothing for the Judge".to_string()),
            "{said:?}"
        );
        // Each lens's state was kept as it moved.
        let states: LensStates =
            serde_json::from_str(&fs::read_to_string(s.work_dir.join(LENS_STATES)).unwrap())
                .unwrap();
        assert_eq!(states.round, 1);
        assert_eq!(states.lenses["delta"].state, "no findings");
        assert_eq!(states.lenses["alpha"].state, "judged");
        assert!(states.lenses.values().all(|l| l.answered == l.reviewers));
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    fn warm_run(setup: &str) -> (Run, PathBuf, Vec<String>, Vec<String>) {
        use super::super::change::tests::repo_with;
        let top = if setup.is_empty() {
            String::new()
        } else {
            format!("tests: {{setup: '{setup}'}}")
        };
        let cfg = config("exception", 1, 1, "", &top);
        let repo = repo_with(&[("a.src", "let x = 1;\n")]);
        let root = std::env::temp_dir().join(format!("tb-acr-setup-{}", new_id()));
        let mut s = setting(&cfg);
        s.copies = Some(root.clone());
        let fake = Fake::new(defect_in("nothing", 0));
        let c = change(&["a.src"]);
        let mut run = start("r", &repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let told = fake.told.lock().unwrap().clone();
        let prompts = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == Role::Reviewer)
            .map(|c| c.3.clone())
            .collect();
        let _ = fs::remove_dir_all(&repo);
        (run, root, told, prompts)
    }

    fn warm_log(root: &Path) -> String {
        let round = fs::read_dir(root)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().contains("-r-r1-"))
            .expect("the round's copies are kept");
        fs::read_to_string(round.path().join("warm.log")).unwrap()
    }

    #[test]
    fn the_warm_setup_says_how_it_went_in_its_log_and_progress() {
        let (_, root, told, prompts) = warm_run("true");
        let log = warm_log(&root);
        assert!(
            log.starts_with("$ true\n(it printed nothing)\n# exit 0, in "),
            "{log}"
        );
        assert!(
            told.iter()
                .any(|t| t.starts_with("say: round 1: setup: ok in ")),
            "{told:?}"
        );
        assert!(prompts.iter().all(|p| !p.contains("setup failed")));
        let _ = fs::remove_dir_all(&root);
        let (_, root, told, _) = warm_run("");
        assert!(
            told.contains(&"say: round 1: setup: none configured".to_string()),
            "{told:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_warm_setup_is_logged_and_told_to_every_reviewer() {
        let (_, root, told, prompts) = warm_run("echo the cache is gone; exit 3");
        let log = warm_log(&root);
        assert!(
            log.contains("$ echo the cache is gone; exit 3\nthe cache is gone\n# exit 3, in "),
            "{log}"
        );
        assert!(
            told.iter()
                .any(|t| t.starts_with("say: round 1: setup: failed, exit 3, in ")),
            "{told:?}"
        );
        assert!(!prompts.is_empty());
        for p in &prompts {
            assert!(
                p.contains("# The repository's setup failed")
                    && p.contains("failed (exit 3)")
                    && p.contains("the cache is gone"),
                "{p}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// A run waiting at its final gate, every lens's latest findings set as
    /// given: (lens, id, state, same_as).
    fn at_final(findings: &[(&str, &str, FindingState, Option<&str>)]) -> (Config, Run) {
        let extra =
            "  - {id: gamma, criteria: \"gamma criteria\", triggers: {paths: [\"**/*.src\"]}}";
        let cfg = config("in-the-loop", 3, 1, extra, "");
        let fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Reviewer => ONE.into(),
                Role::Judge => rating("Low"),
                _ => NOTHING.into(),
            })
        });
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        // Past the scope gate, if it asked.
        while run.waiting == Some(Gate::Scope) {
            let a = answered(
                &run,
                &cfg,
                &[
                    ("Scope", "Approve".into()),
                    ("Fixer", "This session".into()),
                ],
            );
            super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
            drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        }
        assert_eq!(run.waiting, Some(Gate::Final), "{:?}", run.stage);
        let template = run.lenses["alpha"].rounds.last().unwrap().findings[0].clone();
        for l in run.lenses.values_mut() {
            l.rounds.last_mut().unwrap().findings.clear();
        }
        for (lens, id, state, same) in findings {
            let mut f = template.clone();
            f.id = id.to_string();
            f.title = format!("{id} title");
            f.state = *state;
            f.same_as = same.map(str::to_string);
            run.lenses
                .get_mut(*lens)
                .unwrap()
                .rounds
                .last_mut()
                .unwrap()
                .findings
                .push(f);
        }
        (cfg, run)
    }

    fn logged_questions(run: &Run, cfg: &Config) -> Vec<super::super::carousel::Question> {
        super::super::carousel::ask(run, cfg, "acrbus")
            .carousels
            .iter()
            .flat_map(|c| c.questions.clone())
            .filter(|q| q.header.starts_with("Logged"))
            .collect()
    }

    #[test]
    fn logged_questions_never_share_text_and_take_either_key_form() {
        use FindingState::Logged;
        let (cfg, mut run) = at_final(&[
            ("alpha", "a1", Logged, None),
            ("alpha", "a2", Logged, None),
            ("beta", "b1", Logged, None),
            ("beta", "b2", Logged, None),
            ("gamma", "g1", Logged, None),
            ("gamma", "g2", Logged, None),
        ]);
        let qs = logged_questions(&run, &cfg);
        assert_eq!(qs.len(), 2);
        assert_ne!(qs[0].question, qs[1].question);
        assert!(
            qs[0].question.starts_with("Logged findings 1-3 of 6"),
            "{}",
            qs[0].question
        );
        assert!(
            qs[1].question.starts_with("Logged findings 4-6 of 6"),
            "{}",
            qs[1].question
        );
        // The first by its text, the second by "[header] text".
        let mut a = answered(&run, &cfg, &[("Result", "Approve".into())]);
        a.insert(qs[0].question.clone(), serde_json::json!(["None of them"]));
        a.insert(
            format!("[{}] {}", qs[1].header, qs[1].question),
            serde_json::json!(["b2"]),
        );
        let applied = super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        let blocked = run.lenses["beta"].rounds.last().unwrap().findings[1].clone();
        assert!(
            blocked.blocks() && blocked.state == FindingState::Folded,
            "{blocked:?}"
        );
        assert_eq!(
            applied
                .feedback
                .iter()
                .filter(|f| f.verdict == Some(super::super::configure::Verdict::ShouldBlock))
                .count(),
            1
        );
        // Approve and "should block" together: the choice to block wins.
        assert_eq!(run.stage, Stage::Fix);
        assert!(
            applied.notes.iter().any(|n| n.contains("doesn't end yet")),
            "{:?}",
            applied.notes
        );
    }

    #[test]
    fn approve_never_ends_a_review_while_a_should_block_is_lost() {
        use FindingState::Logged;
        let (cfg, mut run) = at_final(&[
            ("alpha", "a1", Logged, None),
            ("beta", "b1", Logged, None),
            ("beta", "b2", Logged, None),
            ("gamma", "g1", Logged, None),
            ("gamma", "g2", Logged, Some("b1")),
        ]);
        let qs = logged_questions(&run, &cfg);
        // Every should-block, picked across both questions.
        let mut a = answered(&run, &cfg, &[("Result", "Approve".into())]);
        a.insert(
            qs[0].question.clone(),
            serde_json::json!(["a1", "b1", "b2"]),
        );
        a.insert(qs[1].question.clone(), serde_json::json!(["g1"]));
        let applied = super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        assert_eq!(run.stage, Stage::Fix, "it doesn't end confirmed");
        let blocks: Vec<String> = run
            .lenses
            .values()
            .flat_map(|l| l.rounds.last().unwrap().findings.clone())
            .filter(|f| f.blocks())
            .map(|f| f.id)
            .collect();
        // g2 is b1 found by gamma too: it blocks with it.
        assert_eq!(blocks.len(), 5, "{blocks:?}");
        assert_eq!(applied.feedback.len(), 5);
        // A question left out, or an answer for no question, is refused.
        let (cfg, mut run) = at_final(&[("alpha", "a1", Logged, None)]);
        let only = answered(&run, &cfg, &[("Result", "Approve".into())]);
        let err = format!(
            "{:#}",
            super::super::carousel::apply(&mut run, &cfg, &only, 1).unwrap_err()
        );
        assert!(
            err.contains("need an answer") && err.contains("[Logged]"),
            "{err}"
        );
        let mut stray = only.clone();
        stray.insert(
            "Should any of these block?".into(),
            serde_json::json!(["a1"]),
        );
        let err = format!(
            "{:#}",
            super::super::carousel::apply(&mut run, &cfg, &stray, 1).unwrap_err()
        );
        assert!(err.contains("for no question"), "{err}");
        assert_eq!(run.waiting, Some(Gate::Final), "nothing was applied");
    }

    #[test]
    fn every_upheld_finding_reaches_the_person_however_many() {
        use FindingState::{Dropped, Logged};
        let mut found = Vec::new();
        let ids: Vec<String> = (1..=12).map(|n| format!("f{n}")).collect();
        for (n, id) in ids.iter().enumerate() {
            let lens = ["alpha", "beta", "gamma"][n % 3];
            let same = match n {
                10 => Some("f1"),
                11 => Some("f2"),
                _ => None,
            };
            found.push((lens, id.as_str(), Logged, same));
        }
        found.push(("beta", "rejected", Dropped, None));
        let (cfg, run) = at_final(&found);
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        for c in &ask.carousels {
            assert!((1..=4).contains(&c.questions.len()));
            for q in &c.questions {
                assert!((2..=4).contains(&q.options.len()), "{q:?}");
            }
        }
        let qs = logged_questions(&run, &cfg);
        let offered: Vec<String> = qs
            .iter()
            .flat_map(|q| q.options.iter().map(|o| o.label.clone()))
            .filter(|l| l != "None of them")
            .collect();
        // Ten shown, f11 and f12 with the findings they repeat.
        assert_eq!(offered.len(), 10, "{offered:?}");
        assert!(ids[..10].iter().all(|id| offered.contains(id)));
        let f1 = qs
            .iter()
            .flat_map(|q| &q.options)
            .find(|o| o.label == "f1")
            .unwrap();
        assert!(
            f1.description.contains("also found by"),
            "{}",
            f1.description
        );
        assert!(
            qs[0]
                .question
                .contains("2 more are the same defect another lens found")
                && qs[0].question.contains("1 rejected, not shown"),
            "{}",
            qs[0].question
        );
        let texts: std::collections::BTreeSet<&String> = ask
            .carousels
            .iter()
            .flat_map(|c| c.questions.iter().map(|q| &q.question))
            .collect();
        assert_eq!(
            texts.len(),
            ask.carousels
                .iter()
                .map(|c| c.questions.len())
                .sum::<usize>(),
            "no two share their text"
        );
        assert!(ask.carousels.len() > 1);
        assert!(
            ask.then
                .contains(&format!("Show all {} carousel(s)", ask.carousels.len()))
        );
    }

    #[test]
    fn a_judge_starts_from_its_findings_hunks_and_keeps_everything_it_had() {
        let cfg = plain("exception", 1, 1);
        let fake = Fake::new(defect_in("alpha", 99));
        let s = setting(&cfg);
        let c = diffed(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let calls = fake.calls.lock().unwrap().clone();
        let judge = calls
            .iter()
            .find(|c| c.0 == Role::Judge)
            .expect("alpha's Judge");
        let prompt = &judge.3;
        assert!(
            prompt.contains("# Where the findings point, to start from"),
            "{prompt}"
        );
        assert!(
            prompt.contains("--- a.src\n@@ -1,1 +1,2 @@\n+one\n+two"),
            "{prompt}"
        );
        assert!(prompt.contains("a starting point, not a limit"), "{prompt}");
        // Its brief starts with the role's own text, the same for every
        // Judge, with the hunks after the findings; the change is still
        // attached; and it runs with its binding's tools.
        assert!(
            prompt.starts_with("You are a Judge of reviewers' findings"),
            "{prompt}"
        );
        let findings = prompt.find("\"title\"").expect("the findings");
        let hunks = prompt.find("# Where the findings point").unwrap();
        assert!(findings < hunks, "{prompt}");
        assert!(fake.with_diff.lock().unwrap().contains(&Role::Judge));
        assert!(!fake.bare.lock().unwrap().contains(&Role::Judge));
    }

    #[test]
    fn a_slack_reply_answers_each_logged_question_on_its_own_and_none_is_skipped() {
        use FindingState::Logged;
        let found = [
            ("alpha", "a1", Logged, None),
            ("alpha", "a2", Logged, None),
            ("beta", "b1", Logged, None),
            ("beta", "b2", Logged, None),
            ("gamma", "g1", Logged, None),
            ("gamma", "g2", Logged, None),
        ];
        let (cfg, mut run) = at_final(&found);
        let qs: Vec<super::super::carousel::Question> =
            super::super::carousel::ask(&run, &cfg, "acrbus")
                .carousels
                .iter()
                .flat_map(|c| c.questions.clone())
                .collect();
        let n = |h: &str| qs.iter().position(|q| q.header == h).unwrap() + 1;
        let reply = format!(
            "{}: Approve\n{}: None of them\n{}: b2\n{}: Nothing missed",
            n("Result"),
            n("Logged 1"),
            n("Logged 2"),
            n("Missed")
        );
        let answers = super::super::grayboard::parse(&qs, &reply).unwrap();
        assert_eq!(answers.len(), 4, "no two answers share a key");
        super::super::carousel::apply(&mut run, &cfg, &answers, 1).unwrap();
        assert!(run.lenses["beta"].rounds.last().unwrap().findings[1].blocks());
        assert_eq!(run.stage, Stage::Fix);
        // A reply that leaves a Logged question out is refused, not taken as
        // "none of them".
        let (cfg, mut run) = at_final(&found);
        let short =
            super::super::grayboard::parse(&qs, &format!("{}: Approve", n("Result"))).unwrap();
        let err = format!(
            "{:#}",
            super::super::carousel::apply(&mut run, &cfg, &short, 1).unwrap_err()
        );
        assert!(
            err.contains("need an answer") && err.contains("[Logged 1]"),
            "{err}"
        );
    }

    /// A caller that gives the round's facts to its calls, and tells when
    /// a call has filled the cache: once it's answered. Reviewers find two
    /// defects, each with reasoning a Judge of one finding mustn't see.
    struct Cached {
        shared: Mutex<Option<Shared>>,
        calls: Mutex<Vec<Seen>>,
        answered: Mutex<BTreeSet<String>>,
        said: Mutex<Vec<String>>,
        reviewer_ms: u64,
        /// How long a keep-alive takes to answer: a slow provider's.
        keep_alive_ms: u64,
    }

    /// A call as it was seen: its label, role and prompt, when it started
    /// and ended, and whether it had the facts.
    type Seen = (String, Role, String, Instant, Instant, bool);

    const TWO: &str = r#"{"findings": [
        {"title": "the first defect", "detail": "REASONING-CANARY one", "file": "a.src", "line": 1},
        {"title": "the second defect", "detail": "REASONING-CANARY two", "file": "a.src", "line": 1}]}"#;

    impl Cached {
        fn new(reviewer_ms: u64) -> Cached {
            Cached {
                shared: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                answered: Mutex::new(BTreeSet::new()),
                said: Mutex::new(Vec::new()),
                reviewer_ms,
                keep_alive_ms: 0,
            }
        }

        fn of(&self, role: Role) -> Vec<Seen> {
            let calls = self.calls.lock().unwrap();
            calls
                .iter()
                .filter(|c| c.1 == role && !c.0.starts_with("cache keep-alive"))
                .cloned()
                .collect()
        }
    }

    impl Caller for Cached {
        fn call(&self, call: &Call) -> Result<Answer> {
            let began = Instant::now();
            let with_facts = self.shared.lock().unwrap().is_some()
                && matches!(call.role, Role::Reviewer | Role::Judge);
            let text = if call.label.starts_with("cache keep-alive") {
                assert_eq!(call.prompt, roles::KEEP_ALIVE);
                std::thread::sleep(Duration::from_millis(self.keep_alive_ms));
                "ok".to_string()
            } else {
                match call.role {
                    Role::Reviewer => {
                        std::thread::sleep(Duration::from_millis(self.reviewer_ms));
                        TWO.to_string()
                    }
                    Role::Judge => {
                        let titles: Vec<&str> = ["the first defect", "the second defect"]
                            .into_iter()
                            .filter(|t| call.prompt.contains(t))
                            .collect();
                        let rated: Vec<String> = titles
                            .iter()
                            .map(|t| {
                                format!(r#"{{"title": "{t}", "file": "a.src", "severity": "Low"}}"#)
                            })
                            .collect();
                        format!(r#"{{"findings": [{}]}}"#, rated.join(","))
                    }
                    _ => NOTHING.to_string(),
                }
            };
            self.calls.lock().unwrap().push((
                call.label.clone(),
                call.role,
                call.prompt.to_string(),
                began,
                Instant::now(),
                with_facts,
            ));
            self.answered.lock().unwrap().insert(call.label.clone());
            Ok(Answer {
                text,
                run_id: new_id(),
                usage: Some(RunUsage {
                    calls: 1,
                    input_tokens: 100,
                    cache_read_tokens: 1000,
                    output_tokens: 10,
                    ..RunUsage::default()
                }),
                secs: Some(1),
            })
        }

        fn say(&self, what: &str) {
            self.said.lock().unwrap().push(what.to_string());
        }

        fn share(&self, shared: Option<Shared>) -> bool {
            let given = shared.is_some();
            *self.shared.lock().unwrap() = shared;
            given
        }

        fn warmed(&self, label: &str) -> Option<bool> {
            Some(self.answered.lock().unwrap().contains(label))
        }
    }

    /// A repository the learned-reading tests review: a checkout of plain
    /// files, and a record folder of its own.
    fn reading_repo() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("tb-reading-{}", new_id()));
        let repo = root.join("project");
        for (path, text) in [
            ("a.src", "let x = 1;\n"),
            ("src/helper.rs", "pub fn helper() {}\n"),
            ("docs/notes.md", "how the parts fit\n"),
        ] {
            let full = repo.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        let out = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());
        (repo, root.join("learned"))
    }

    /// A one-round review of `repo`, with its record folder at `store`: the
    /// run, and its round's facts as every reviewer got them.
    fn reading_review(repo: &Path, store: Option<PathBuf>, top: &str) -> (Run, String) {
        let cfg = config("exception", 1, 1, "", top);
        let mut fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Reviewer => r#"{"findings": []}"#.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        fake.usage = Some(RunUsage {
            calls: 1,
            input_tokens: 10,
            opened: vec![
                // In a copy of its own, wherever that was.
                "/elsewhere/acr-copies/r1-x/project/docs/notes.md".into(),
                "src/helper.rs".into(),
                // Outside the repository, and not there: not learned.
                "/etc/hosts".into(),
                "src/gone.rs".into(),
            ],
            ..RunUsage::default()
        });
        let mut s = setting(&cfg);
        s.learned = store;
        fs::create_dir_all(&s.work_dir).unwrap();
        let c = change(&["a.src"]);
        let mut run = start("r", repo, "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let facts = fs::read_to_string(s.work_dir.join("round-1.facts.md")).unwrap_or_default();
        let _ = fs::remove_dir_all(&s.work_dir);
        (run, facts)
    }

    #[test]
    fn what_reviewers_read_is_learned_and_the_next_review_starts_with_it() {
        let (repo, store) = reading_repo();
        let record = super::super::learned::Learned::path(&store, "project");
        // An earlier review's reviewers kept opening the helper.
        let mut earlier = super::super::learned::Learned::default();
        earlier.add("alpha", &["src/helper.rs".to_string()]);
        earlier.add("beta", &["src/helper.rs".to_string()]);
        // A file one call opened isn't worth every turn's reading.
        earlier.add("alpha", &["a.src".to_string()]);
        earlier.save(&record).unwrap();
        let (run, facts) = reading_review(&repo, Some(store.clone()), "");
        assert!(
            facts.contains("## Often read: src/helper.rs") && facts.contains("pub fn helper()"),
            "round 1's facts carry what reviewers kept opening: {facts}"
        );
        assert!(
            !facts.contains("Often read: docs/notes.md"),
            "not yet learned"
        );
        assert!(
            !facts.contains("Often read: a.src"),
            "opened by one call only"
        );
        // This review's reading is in the record, per lens, as paths in the
        // repository, each reviewer call counting each file once.
        let learned = super::super::learned::Learned::load(&record);
        assert_eq!(learned.lenses["alpha"]["src/helper.rs"], 2);
        assert_eq!(learned.lenses["beta"]["src/helper.rs"], 2);
        assert_eq!(learned.lenses["alpha"]["docs/notes.md"], 1);
        assert_eq!(learned.lenses["beta"]["docs/notes.md"], 1);
        let all: Vec<&String> = learned.lenses.values().flat_map(|c| c.keys()).collect();
        assert!(
            all.iter()
                .all(|p| !p.starts_with('/') && !p.contains("gone")),
            "{all:?}"
        );
        assert_eq!(run.learned, vec![1], "the round is counted once");
        // The reviewers' calls carry their lens and what they read.
        let reviewer = run.usage.iter().find(|u| u.role == Role::Reviewer).unwrap();
        assert!(reviewer.lens.is_some() && !reviewer.opened.is_empty());
        // The next review of the repository starts with this one's reading.
        let (_, next) = reading_review(&repo, Some(store.clone()), "");
        assert!(next.contains("## Often read: docs/notes.md"), "{next}");
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn a_reviewer_calls_reading_counts_once_across_all_its_sessions() {
        let (repo, store) = reading_repo();
        let cfg = config("exception", 1, 1, "", "");
        let mut s = setting(&cfg);
        s.learned = Some(store.clone());
        let fake = Fake::new(|_, _: &str| Ok(NOTHING.to_string()));
        let mut run = start("r", &repo, "main", None, &s, &change(&["a.src"]), &fake).unwrap();
        let session = |lens: &str, call: &str, label: &str, opened: &[&str]| -> Usage {
            let mut u: Usage = serde_json::from_value(serde_json::json!({
                "role": "reviewer", "binding": "b", "agent": "a@here", "model": "m",
                "run_id": "x", "tokens_in": 1, "tokens_out": 1, "usd": null,
                "round": 1, "label": label,
            }))
            .unwrap();
            u.lens = Some(lens.into());
            u.call = Some(call.into());
            u.opened = opened.iter().map(|o| o.to_string()).collect();
            u
        };
        run.round = 1;
        run.usage.extend([
            // One call of alpha's: two relay legs and a verifying session,
            // reading the helper twice and the notes twice between them.
            session(
                "alpha",
                "alpha: reviewer 1",
                "alpha: reviewer 1",
                &["src/helper.rs"],
            ),
            session(
                "alpha",
                "alpha: reviewer 1",
                "alpha: reviewer 1 (session 2)",
                &["src/helper.rs", "docs/notes.md"],
            ),
            session(
                "alpha",
                "alpha: reviewer 1",
                "alpha: reviewer 1: verify 1",
                &["docs/notes.md"],
            ),
            // Another of alpha's calls, and one of beta's.
            session(
                "alpha",
                "alpha: reviewer 2",
                "alpha: reviewer 2",
                &["docs/notes.md"],
            ),
            session(
                "beta",
                "beta: reviewer 1",
                "beta: reviewer 1",
                &["docs/notes.md"],
            ),
        ]);
        learn_round(&mut run, &s);
        let record = super::super::learned::Learned::load(&super::super::learned::Learned::path(
            &store, "project",
        ));
        assert_eq!(
            record.lenses["alpha"]["src/helper.rs"], 1,
            "one call, however many sessions"
        );
        assert_eq!(
            record.lenses["alpha"]["docs/notes.md"], 2,
            "two of alpha's calls"
        );
        assert_eq!(record.lenses["beta"]["docs/notes.md"], 1);
        // Learned once a round, even if the round is resumed.
        learn_round(&mut run, &s);
        let again = super::super::learned::Learned::load(&super::super::learned::Learned::path(
            &store, "project",
        ));
        assert_eq!(again, record);
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn learning_what_reviewers_read_can_be_turned_off_and_trials_never_learn() {
        let (repo, store) = reading_repo();
        let record = super::super::learned::Learned::path(&store, "project");
        let mut earlier = super::super::learned::Learned::default();
        earlier.add("alpha", &["src/helper.rs".to_string()]);
        earlier.add("beta", &["src/helper.rs".to_string()]);
        earlier.save(&record).unwrap();
        let (_, off) = reading_review(&repo, Some(store.clone()), "facts: {learned: false}");
        assert!(!off.contains("Often read"), "{off}");
        assert_eq!(
            super::super::learned::Learned::load(&record),
            earlier,
            "nothing learned"
        );
        // A review with no record folder, as a trial, neither reads nor writes one.
        let (_, blind) = reading_review(&repo, None, "");
        assert!(!blind.contains("Often read"));
        assert_eq!(super::super::learned::Learned::load(&record), earlier);
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn often_read_files_keep_to_the_configured_budget() {
        let (repo, store) = reading_repo();
        let record = super::super::learned::Learned::path(&store, "project");
        let mut earlier = super::super::learned::Learned::default();
        for lens in ["alpha", "beta"] {
            earlier.add(
                lens,
                &["src/helper.rs".to_string(), "docs/notes.md".to_string()],
            );
        }
        earlier.save(&record).unwrap();
        // Room for the notes (18 bytes), not the helper (19).
        let (_, facts) = reading_review(&repo, Some(store), "facts: {learned_bytes: 18}");
        assert!(facts.contains("## Often read: docs/notes.md"), "{facts}");
        assert!(!facts.contains("Often read: src/helper.rs"));
        let _ = fs::remove_dir_all(repo.parent().unwrap());
    }

    fn cached_run(top: &str, fan_out: u32, caller: &Cached) -> (Run, PathBuf) {
        let cfg = config("exception", 1, fan_out, "", top);
        let s = setting(&cfg);
        fs::create_dir_all(&s.work_dir).unwrap();
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, caller).unwrap();
        drive(&mut run, &s, caller, &|_| Ok(c.clone())).unwrap();
        (run, s.work_dir.clone())
    }

    #[test]
    fn a_fan_outs_other_calls_start_a_stagger_apart_once_the_first_has_warmed_the_cache() {
        let starts = |caller: &Cached| {
            let mut r: Vec<(String, Instant)> = caller
                .of(Role::Reviewer)
                .into_iter()
                .map(|c| (c.0, c.3))
                .collect();
            r.sort_by_key(|c| c.1);
            r
        };
        let caller = Cached::new(50);
        let _ = cached_run("facts: {stagger_secs: 1}", 2, &caller);
        let reviewers = starts(&caller);
        assert_eq!(reviewers.len(), 4, "two lenses of two");
        // The first goes alone; the other three start a second apart.
        for pair in reviewers[1..].windows(2) {
            let gap = pair[1].1.duration_since(pair[0].1);
            assert!(gap >= Duration::from_millis(950), "{pair:?} {gap:?}");
        }
        let said = caller.said.lock().unwrap().clone();
        assert!(
            said.iter().any(|l| l
                .starts_with(&format!("warming the cache: {} goes first", reviewers[0].0))
                && l.ends_with("then start 1s apart")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|l| l.starts_with("the cache is warm") && l.ends_with("start, 1s apart")),
            "{said:?}"
        );
        // 0 starts them all at once.
        let caller = Cached::new(50);
        let _ = cached_run("facts: {stagger_secs: 0}", 2, &caller);
        let reviewers = starts(&caller);
        let spread = reviewers[3].1.duration_since(reviewers[1].1);
        assert!(spread < Duration::from_millis(500), "{spread:?}");
        assert!(
            !caller
                .said
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("apart")),
            "{:?}",
            caller.said.lock().unwrap()
        );
    }

    #[test]
    fn a_fan_outs_first_call_warms_the_cache_before_the_rest_and_all_share_the_facts() {
        let caller = Cached::new(150);
        let (run, dir) = cached_run("", 3, &caller);
        let reviewers = caller.of(Role::Reviewer);
        assert_eq!(reviewers.len(), 6, "two lenses of three");
        let first = reviewers.iter().min_by_key(|c| c.3).unwrap();
        for other in reviewers.iter().filter(|c| c.0 != first.0) {
            assert!(
                other.3 >= first.4,
                "{} started before the first was done",
                other.0
            );
        }
        let said = caller.said.lock().unwrap().clone();
        assert!(
            said.iter()
                .any(|l| l.starts_with(&format!("warming the cache: {} goes first", first.0))),
            "{said:?}"
        );
        assert!(
            said.iter()
                .any(|l| l.starts_with("the cache is warm (its first call")),
            "{said:?}"
        );
        // Every reviewer and Judge had the facts; their prompts carry no gate
        // text of their own, and the facts were kept with the run.
        assert!(
            caller
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| matches!(c.1, Role::Reviewer | Role::Judge))
                .all(|c| c.5)
        );
        let facts = &run.facts;
        assert_eq!(facts.len(), 1);
        assert!(dir.join("round-1.facts.md").is_file());
        // What every reviewer shares at the head: ACR's prompt, the facts,
        // and their role's text up to the lens.
        let p = run
            .prefixes
            .iter()
            .find(|p| p.what == "reviewers")
            .expect("reviewers' prefix");
        let head = crate::setup::SYSTEM_PROMPT.len() as u64 + facts[0].bytes;
        let role_text = reviewers[0].2.find("# Your lens").unwrap() as u64;
        assert!(p.bytes >= head + role_text, "{p:?}");
        assert!(
            said.iter().any(|l| l.contains("shared prefix, reviewers:")),
            "{said:?}"
        );
        // Each reviewer's call is counted in its fan-out, and only one first.
        let fanned: Vec<&Usage> = run
            .usage
            .iter()
            .filter(|u| u.fan_out.as_deref() == Some("reviewers"))
            .collect();
        assert_eq!(fanned.len(), 6);
        assert_eq!(fanned.iter().filter(|u| u.first).count(), 1);
        let rows = report::by_role(&run.usage);
        let row = rows.iter().find(|r| r.role == "reviewers").unwrap();
        assert_eq!(
            (row.calls, row.fresh, row.cached, row.output),
            (6, 600, 6000, 60)
        );
        assert_eq!(
            (row.first_cached, row.rest_cached),
            (Some(1000), Some(1000))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_role_calls_share_their_head_byte_for_byte_and_what_differs_comes_last() {
        let lens = |id: &str, criteria: &str| Lens {
            id: id.into(),
            criteria: criteria.into(),
            attack: None,
            blocker: None,
            triggers: Default::default(),
            binding: None,
            severity: Default::default(),
            include_generated: false,
        };
        let call = |scope| roles::ReviewerCall {
            scope,
            not_findings: "- decided",
            own_copy: false,
            runbook: Some("run the tests"),
            brief: Some("look here"),
            gate: "",
        };
        let a = roles::reviewer_parts(&lens("alpha", "a"), &call("the change"));
        let b = roles::reviewer_parts(&lens("beta", "b"), &call("the change"));
        assert_eq!(a[0], b[0], "the role's own text, the same for every lens");
        assert_eq!(a[1], b[1], "the runbook, from the config");
        // Sources go fixed, config, tool, model: a model's words never sit
        // ahead of what a tool or the config says, and the call's own
        // instruction is last.
        let order = |parts: &[roles::Part]| -> Vec<roles::Source> {
            parts.iter().map(|p| p.source).collect()
        };
        let sources = order(&a);
        let brief_at = sources
            .iter()
            .position(|s| *s == roles::Source::Model)
            .unwrap();
        let tool_at = sources
            .iter()
            .position(|s| *s == roles::Source::Tool)
            .unwrap();
        assert!(tool_at < brief_at, "{sources:?}");
        assert_eq!(
            a.last().unwrap().text,
            "Answer with only the JSON described above."
        );
        // Two Judges of one lens share all but their findings.
        let raw = |t: &str| roles::Raw {
            title: t.into(),
            detail: "REASONING-CANARY".into(),
            file: Some("a.src".into()),
            line: Some(1),
        };
        let l = lens("alpha", "a");
        let one = roles::join(&roles::judge_finding_parts(
            &l,
            &raw("one"),
            "",
            "- decided",
        ));
        let two = roles::join(&roles::judge_finding_parts(
            &l,
            &raw("two"),
            "",
            "- decided",
        ));
        let shared = super::super::facts::shared_prefix(&one, &two);
        assert!(
            one[..shared].contains("- decided"),
            "decided non-issues ahead of the finding"
        );
        assert!(
            !one.contains("REASONING-CANARY"),
            "no reviewer's reasoning reaches its Judge"
        );
        assert!(one[shared..].contains("one") && !one[shared..].contains("- decided"));
    }

    #[test]
    fn per_finding_judges_rate_each_claim_alone() {
        let caller = Cached::new(0);
        let (run, dir) = cached_run("facts: {judges: per-finding}", 1, &caller);
        let judges = caller.of(Role::Judge);
        assert_eq!(judges.len(), 4, "two findings in each of two lenses");
        for (label, _, prompt, ..) in &judges {
            let titles = ["the first defect", "the second defect"]
                .iter()
                .filter(|t| prompt.contains(*t))
                .count();
            assert_eq!(titles, 1, "{label}: one claim each");
            assert!(!prompt.contains("REASONING-CANARY"), "{label}");
            assert!(label.contains("the Judge, finding "), "{label}");
        }
        let alpha = &run.lenses["alpha"].rounds[0];
        assert_eq!(alpha.findings.len(), 2, "{:?}", alpha.findings);
        let judged: Vec<&Usage> = run.usage.iter().filter(|u| u.role == Role::Judge).collect();
        assert_eq!(judged.len(), 4);
        assert!(
            judged
                .iter()
                .all(|u| u.fan_out.as_deref() == Some("judges"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_keep_alive_refreshes_a_live_prefix_and_is_counted_apart() {
        let caller = Cached::new(1600);
        let (run, dir) = cached_run("facts: {keep_alive_secs: 1, warm_secs: 0}", 1, &caller);
        let kept: Vec<&Usage> = run
            .usage
            .iter()
            .filter(|u| u.fan_out.as_deref() == Some("keep-alive"))
            .collect();
        assert!(!kept.is_empty(), "{:?}", run.usage);
        assert!(
            kept.iter()
                .all(|u| u.label.as_deref() == Some("cache keep-alive: reviewers"))
        );
        let said = caller.said.lock().unwrap().clone();
        assert!(
            said.iter()
                .any(|l| l == "cache: refreshed the reviewers' prefix"),
            "{said:?}"
        );
        let rows = report::by_role(&run.usage);
        let alive = rows.iter().find(|r| r.role == "keep-alive").unwrap();
        assert_eq!(alive.calls, kept.len());
        let reviewers = rows.iter().find(|r| r.role == "reviewers").unwrap();
        assert_eq!(reviewers.calls, 2, "keep-alive isn't reviewers' work");
        // Off, there's none.
        let caller = Cached::new(1200);
        let (run, dir2) = cached_run("facts: {keep_alive_secs: 0}", 1, &caller);
        assert!(
            !run.usage
                .iter()
                .any(|u| u.fan_out.as_deref() == Some("keep-alive"))
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn a_slow_keep_alive_is_never_followed_at_once_by_another_nor_overlapped() {
        // The provider takes 2.5s to answer a keep-alive due every 2s.
        let mut caller = Cached::new(7500);
        caller.keep_alive_ms = 2500;
        let (_run, dir) = cached_run("facts: {keep_alive_secs: 2, warm_secs: 0}", 1, &caller);
        let calls = caller.calls.lock().unwrap().clone();
        let mut kept: Vec<(Instant, Instant)> = calls
            .iter()
            .filter(|c| c.0 == "cache keep-alive: reviewers")
            .map(|c| (c.3, c.4))
            .collect();
        kept.sort();
        assert!(kept.len() >= 2, "{kept:?}");
        for pair in kept.windows(2) {
            let (_, ended) = pair[0];
            let (next, _) = pair[1];
            assert!(
                next >= ended,
                "a keep-alive went out while the last was still in flight"
            );
            assert!(
                next.duration_since(ended) >= Duration::from_millis(1800),
                "the next keep-alive followed {:?} after the last answered, not its interval",
                next.duration_since(ended)
            );
        }
        let said = caller.said.lock().unwrap().clone();
        assert!(
            said.iter().any(|l| l.starts_with(
                "cache: the reviewers' keep-alive took 2s to answer, past its 2s interval: the \
                 next waits its interval after the answer"
            )),
            "{said:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn facts_only_reviewers_get_nothing_a_model_wrote_and_no_decided_non_issues() {
        let cfg = config(
            "exception",
            1,
            1,
            "",
            "facts: {reviewers: facts-only}\nstages: [brief]",
        );
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Orchestrator if prompt.contains("You brief the reviewers") => {
                    panic!("no brief is asked for")
                }
                Role::Reviewer => ONE.to_string(),
                Role::Judge => rating("High"),
                _ => NOTHING.to_string(),
            })
        });
        let mut s = setting(&cfg);
        s.builder_brief = Some(stages::BuilderBrief {
            scope: Some("SCOPE-CANARY".into()),
            lenses: BTreeMap::from([("alpha".to_string(), "BRIEF-CANARY".to_string())]),
        });
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let calls = fake.calls.lock().unwrap();
        let reviewers: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Reviewer)
            .map(|c| &c.3)
            .collect();
        assert!(!reviewers.is_empty());
        for p in &reviewers {
            assert!(
                !p.contains("BRIEF-CANARY") && !p.contains("the decided thing"),
                "{p}"
            );
        }
        let sent = fake.sent.lock().unwrap();
        for (role, text) in sent.iter().filter(|(r, _)| *r == Role::Reviewer) {
            assert!(!text.contains("SCOPE-CANARY"), "{role:?}: {text}");
        }
        // The Judge still holds the reviewers to what's decided.
        assert!(
            calls
                .iter()
                .any(|c| c.0 == Role::Judge && c.3.contains("the decided thing"))
        );
        let _ = fs::remove_dir_all(&s.work_dir);
    }

    /// A repository of two files, for tests that read what's in them.
    fn two_files() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tb-acr-reuse-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.src"), "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        fs::write(dir.join("b.src"), "x\n").unwrap();
        dir
    }

    const BLOCKER_AND_NIT: &str = r#"{"findings": [{"title": "Blocker in b", "detail": "d", "file": "b.src", "line": 1}, {"title": "A nit in a", "detail": "d", "file": "a.src", "line": 5}]}"#;
    // The same claim, a line away, in another reviewer's words' order.
    const JUST_THE_NIT: &str =
        r#"{"findings": [{"title": "a nit, in a", "detail": "d", "file": "a.src", "line": 6}]}"#;

    /// The Judge's ratings of whichever of those two it's given.
    fn rate_two(prompt: &str) -> String {
        let mut out = Vec::new();
        if prompt.contains("Blocker in b") {
            out.push(
                r#"{"title": "Blocker in b", "file": "b.src", "line": 1, "severity": "High"}"#,
            );
        }
        if prompt.contains("nit") {
            out.push(r#"{"title": "A nit in a", "file": "a.src", "line": 5, "severity": "Low"}"#);
        }
        format!(r#"{{"findings": [{}]}}"#, out.join(", "))
    }

    /// alpha's reviewers find a blocker in b.src and a nit in a.src until
    /// the blocker's fixed, then the nit alone. The fix writes into `writes`.
    fn nit_after_fix(
        dir: PathBuf,
        writes: &'static [&'static str],
    ) -> impl Fn(Role, &str) -> Result<String> + Sync {
        let fixed = Mutex::new(false);
        move |role, prompt| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => if *fixed.lock().unwrap() {
                    JUST_THE_NIT
                } else {
                    BLOCKER_AND_NIT
                }
                .to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rate_two(prompt),
                Role::Orchestrator if prompt.contains("Fix these") => {
                    *fixed.lock().unwrap() = true;
                    for f in writes {
                        fs::write(dir.join(f), "fixed\n").unwrap();
                    }
                    r#"{"done": "fixed it"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        }
    }

    fn go_in<F: Fn(Role, &str) -> Result<String> + Sync>(
        cfg: &Config,
        dir: &Path,
        fake: &Fake<F>,
    ) -> Run {
        let s = setting(cfg);
        let c = change(&["a.src", "b.src"]);
        let mut run = start("r", dir, "main", None, &s, &c, fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, fake, &|_| Ok(again.clone())).unwrap();
        run
    }

    #[test]
    fn a_ruling_that_didnt_block_is_reused_on_unchanged_code_without_a_judge() {
        let cfg = plain("exception", 3, 1);
        let dir = two_files();
        let fake = Fake::new(nit_after_fix(dir.clone(), &["b.src"]));
        let run = go_in(&cfg, &dir, &fake);
        assert_eq!(run.status, Status::Confirmed);
        // Round 1's Judge only: round 2's nit took its ruling.
        assert_eq!(fake.count(Role::Judge), 1);
        let nit = &run.lenses["alpha"].rounds[1].findings[0];
        assert_eq!(nit.reused_from, Some(1));
        assert_eq!(nit.state, FindingState::Logged);
        // Only what didn't block is kept: the blocker never is.
        assert_eq!(run.rulings.len(), 1);
        assert_eq!(run.rulings[0].file, "a.src");
        let told = fake.told.lock().unwrap().join("\n");
        assert!(told.contains("took round 1's ruling"), "{told}");
        let comment = report::finding_line(nit);
        assert!(comment.contains("carried over from round 1"), "{comment}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_ruling_on_code_a_fix_changed_goes_to_the_judge_again() {
        let cfg = plain("exception", 3, 1);
        let dir = two_files();
        let fake = Fake::new(nit_after_fix(dir.clone(), &["a.src", "b.src"]));
        let run = go_in(&cfg, &dir, &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(fake.count(Role::Judge), 2);
        assert_eq!(run.lenses["alpha"].rounds[1].findings[0].reused_from, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reusing_rulings_can_be_switched_off() {
        let cfg = config("exception", 3, 1, "", "savings: {reuse_rulings: false}");
        let dir = two_files();
        let fake = Fake::new(nit_after_fix(dir.clone(), &["b.src"]));
        let run = go_in(&cfg, &dir, &fake);
        assert_eq!(fake.count(Role::Judge), 2);
        assert!(run.rulings.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_different_claim_at_the_same_place_isnt_given_the_ruling() {
        let cfg = plain("exception", 3, 1);
        let dir = two_files();
        let fixed = Mutex::new(false);
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => {
                    if *fixed.lock().unwrap() {
                        r#"{"findings": [{"title": "A crash in a", "file": "a.src", "line": 5}]}"#
                            .to_string()
                    } else {
                        BLOCKER_AND_NIT.to_string()
                    }
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge if prompt.contains("A crash in a") => {
                    r#"{"findings": [{"title": "A crash in a", "file": "a.src", "line": 5, "severity": "Low"}]}"#
                        .to_string()
                }
                Role::Judge => rate_two(prompt),
                Role::Orchestrator if prompt.contains("Fix these") => {
                    *fixed.lock().unwrap() = true;
                    r#"{"done": "fixed it"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let run = go_in(&cfg, &dir, &fake);
        assert_eq!(fake.count(Role::Judge), 2);
        assert_eq!(run.lenses["alpha"].rounds[1].findings[0].reused_from, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_first_pass_escalates_whatever_could_block_or_it_isnt_sure_of() {
        let cfg = plain("exception", 1, 1);
        let lens = cfg.lenses.iter().find(|l| l.id == "alpha").unwrap();
        let changed = stages::changed_lines("");
        let r = |json: &str| {
            roles::read_ratings(&format!(r#"{{"findings": [{json}]}}"#))
                .unwrap()
                .remove(0)
        };
        let goes = |json: &str| first_pass_escalates(&cfg, lens, &r(json), &changed);
        assert!(!goes(r#"{"title": "t", "severity": "Low"}"#));
        assert!(!goes(r#"{"title": "t", "severity": "Rejected"}"#));
        assert!(!goes(
            r#"{"title": "t", "severity": "Low", "real_defect": false}"#
        ));
        assert!(goes(r#"{"title": "t", "severity": "Low", "unsure": true}"#));
        assert!(goes(r#"{"title": "t", "severity": "Medium"}"#));
        assert!(goes(r#"{"title": "t", "severity": "High"}"#));
        assert!(goes(
            r#"{"title": "t", "severity": "Low", "values_call": true}"#
        ));
        assert!(goes(r#"{"title": "t", "severity": "Low", "split": true}"#));
        assert!(goes(
            r#"{"title": "t", "severity": "Low", "misleads": true}"#
        ));
    }

    /// alpha's reviewers find one defect; the first pass and the Judge rate
    /// it as told.
    fn first_and_full(
        first: &'static str,
        full: &'static str,
    ) -> impl Fn(Role, &str) -> Result<String> + Sync {
        move |role, prompt| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge if prompt.contains("You're the first pass") => {
                    format!(r#"{{"findings": [{first}]}}"#)
                }
                Role::Judge => format!(r#"{{"findings": [{full}]}}"#),
                _ => NOTHING.to_string(),
            })
        }
    }

    #[test]
    fn a_finding_the_first_pass_leaves_unrated_goes_on_to_the_judge() {
        let cfg = config("exception", 1, 1, "", "savings: {first_pass_judge: small}");
        let two = r#"{"findings": [{"title": "a defect", "file": "a.src"}, {"title": "another", "file": "a.src", "line": 9}]}"#;
        let fake = Fake::new(move |role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => two.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                // It rates one of the two, Low: nothing it rated escalates.
                Role::Judge if prompt.contains("You're the first pass") => {
                    r#"{"findings": [{"title": "a defect", "file": "a.src", "severity": "Low"}]}"#
                        .to_string()
                }
                Role::Judge => r#"{"findings": [{"title": "a defect", "file": "a.src", "severity": "Low"}, {"title": "another", "file": "a.src", "line": 9, "severity": "Low"}]}"#.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let judged = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == Role::Judge && c.1 == "large")
            .count();
        assert_eq!(judged, 1, "the Judge rules on them all");
        assert!(said(&fake).contains("the first pass rated 1 of the 2 finding(s)"));
        assert_eq!(run.lenses["alpha"].rounds[0].findings.len(), 2, "none lost");
    }

    #[test]
    fn a_first_pass_that_settles_a_lens_saves_its_judge_call() {
        let cfg = config("exception", 1, 1, "", "savings: {first_pass_judge: small}");
        let fake = Fake::new(first_and_full(
            r#"{"title": "a defect", "file": "a.src", "severity": "Low"}"#,
            r#"{"title": "a defect", "file": "a.src", "severity": "High"}"#,
        ));
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        let on = |name: &str| {
            calls
                .iter()
                .filter(|c| c.0 == Role::Judge && c.1 == name)
                .count()
        };
        assert_eq!((on("small"), on("large")), (1, 0));
        let f = &run.lenses["alpha"].rounds[0].findings[0];
        assert_eq!(f.ruled_by.as_deref(), Some("small"));
        assert_eq!(f.state, FindingState::Logged);
    }

    #[test]
    fn what_the_first_pass_sends_on_the_judge_rules_fresh_from_its_own_prompt() {
        let found = r#"{"title": "a defect", "file": "a.src", "severity": "High"}"#;
        // Without a first pass: the Judge's prompt as it would be.
        let plain_cfg = plain("exception", 1, 1);
        let fake = Fake::new(first_and_full(found, found));
        go(&plain_cfg, &["a.src"], &fake);
        let alone: Vec<String> = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == Role::Judge)
            .map(|c| c.3.clone())
            .collect();
        for first in [
            r#"{"title": "a defect", "file": "a.src", "severity": "High"}"#,
            r#"{"title": "a defect", "file": "a.src", "severity": "Low", "unsure": true}"#,
        ] {
            let cfg = config("exception", 1, 1, "", "savings: {first_pass_judge: small}");
            let fake = Fake::new(first_and_full(first, found));
            let (run, _) = go(&cfg, &["a.src"], &fake);
            let calls = fake.calls.lock().unwrap();
            let full: Vec<&String> = calls
                .iter()
                .filter(|c| c.0 == Role::Judge && c.1 == "large")
                .map(|c| &c.3)
                .collect();
            assert_eq!(full.len(), 1, "sent on: {first}");
            // Not anchored: the very prompt it gets without a first pass.
            assert_eq!(full[0], &alone[0]);
            assert!(!full[0].contains("first pass"));
            let f = &run.lenses["alpha"].rounds[0].findings[0];
            assert_eq!(f.ruled_by.as_deref(), Some("large"));
            assert!(f.blocks());
        }
    }

    #[test]
    fn after_a_fix_reviewers_are_told_where_to_look_first_when_its_on() {
        for (top, told) in [("savings: {focus_after_fix: true}", true), ("", false)] {
            let cfg = config("exception", 3, 1, "", top);
            let mut fake = Fake::new(defect_in("alpha", 1));
            fake.fixes = Some(vec!["b.src".into()]);
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(run.status, Status::Confirmed);
            let calls = fake.calls.lock().unwrap();
            let alpha: Vec<&String> = calls
                .iter()
                .filter(|c| c.0 == Role::Reviewer && c.3.contains("alpha criteria"))
                .map(|c| &c.3)
                .collect();
            assert_eq!(alpha.len(), 2);
            assert!(!alpha[0].contains("## Since round"), "round 1 has no fix");
            assert_eq!(alpha[1].contains("## Since round 1"), told, "{top}");
            if told {
                assert!(alpha[1].contains("b.src") && alpha[1].contains("+the fix"));
                // Focus, never less: the whole change is still its brief.
                assert!(alpha[1].contains("review the whole change"));
                let head = alpha[0].trim_end_matches(roles::FINDINGS_REMINDER);
                assert!(alpha[1].starts_with(head));
                // The answer's form, said last, after the focus.
                assert!(alpha[1].ends_with(roles::FINDINGS_REMINDER));
            }
        }
    }

    #[test]
    fn a_lens_the_blast_radius_check_skips_is_reported_skipped_and_swept_later() {
        let cfg = config("exception", 20, 1, "", "savings: {blast_radius: true}");
        let fixes = Mutex::new(0);
        let mut fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && *fixes.lock().unwrap() < 2 => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                Role::Orchestrator if prompt.contains("You synthesize") => {
                    *fixes.lock().unwrap() += 1;
                    r#"{"done": "made it stronger"}"#.to_string()
                }
                // It would skip alpha too, which has blockers of its own: it
                // isn't asked about it, and couldn't skip it.
                Role::Sizer if prompt.contains("must attack") || prompt.contains("passed the change before a fix") => {
                    r#"{"lenses": [{"lens": "beta", "attack": false, "reason": "the fix only touches alpha's parsing"}, {"lens": "alpha", "attack": false, "reason": "no"}]}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        fake.fixes = Some(vec!["a.src".into()]);
        let run = converging(&cfg, &fake);
        assert_eq!(run.status, Status::Confirmed);
        let beta = &run.lenses["beta"];
        assert_eq!(
            beta.skipped.iter().map(|s| s.round).collect::<Vec<_>>(),
            [2, 3]
        );
        // Attacked in round 1, then in the full sweep on the final version.
        assert_eq!(
            beta.rounds.iter().map(|r| r.round).collect::<Vec<_>>(),
            [1, 4]
        );
        assert!(beta.skipped_since_attack().is_none());
        assert!(run.lenses["alpha"].skipped.is_empty());
        // Five reviewers, where attacking beta every round took six.
        assert_eq!(fake.count(Role::Reviewer), 5);
        assert!(fake.bare.lock().unwrap().contains(&Role::Sizer));
        let told = fake.told.lock().unwrap().join("\n");
        assert!(told.contains("beta: skips round 2"), "{told}");
    }

    #[test]
    fn a_skipped_lens_never_reads_as_passing_the_version_it_skipped() {
        let lens = LensResult {
            id: "beta".into(),
            criteria: "c".into(),
            rounds: vec![Round {
                round: 1,
                ..Round::default()
            }],
            done: Vec::new(),
            outcome: Outcome::Confirmed,
            joined: String::new(),
            skipped: vec![Skipped {
                round: 2,
                reason: "the fix can't touch it".into(),
            }],
        };
        assert!(lens.skipped_since_attack().is_some());
        let said = report::outcome_of(&lens);
        assert!(said.starts_with("Skipped in round 2"), "{said}");
        assert!(!said.contains("Confirmed"));
        // Attacked since: then its outcome stands.
        let mut later = lens.clone();
        later.rounds.push(Round {
            round: 3,
            ..Round::default()
        });
        assert!(later.skipped_since_attack().is_none());
        assert_eq!(report::outcome_of(&later), "Confirmed");
    }

    #[test]
    fn a_blocker_deferred_to_a_ticket_is_never_reused_and_is_judged_again() {
        let cfg = plain("per-round", 3, 1);
        let dir = two_files();
        let fixed = Mutex::new(false);
        let leak = r#"{"title": "A leak in a", "detail": "d", "file": "a.src", "line": 5}"#;
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => {
                    if *fixed.lock().unwrap() {
                        format!(r#"{{"findings": [{leak}]}}"#)
                    } else {
                        format!(
                            r#"{{"findings": [{leak}, {{"title": "Blocker in b", "file": "b.src", "line": 1}}]}}"#
                        )
                    }
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => {
                    let mut out = vec![
                        r#"{"title": "A leak in a", "file": "a.src", "line": 5, "severity": "High"}"#,
                    ];
                    if prompt.contains("Blocker in b") {
                        out.push(
                            r#"{"title": "Blocker in b", "file": "b.src", "line": 1, "severity": "High"}"#,
                        );
                    }
                    format!(r#"{{"findings": [{}]}}"#, out.join(", "))
                }
                Role::Orchestrator if prompt.contains("Fix these") => {
                    *fixed.lock().unwrap() = true;
                    fs::write(dir.join("b.src"), "fixed\n").unwrap();
                    r#"{"done": "fixed it"}"#.to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let s = setting(&cfg);
        let c = change(&["a.src", "b.src"]);
        let mut run = start("r", &dir, "main", None, &s, &c, &fake).unwrap();
        let again = c.clone();
        drive(&mut run, &s, &fake, &|_| Ok(again.clone())).unwrap();
        let first = &run.lenses["alpha"].rounds[0].findings;
        let id = |title: &str| first.iter().find(|f| f.title == title).unwrap().id.clone();
        let (leak_id, fix_id) = (id("A leak in a"), id("Blocker in b"));
        decide(
            &mut run,
            &Decision {
                fold: vec![fix_id],
                defer: vec![leak_id],
                ..Decision::default()
            },
        )
        .unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(again.clone())).unwrap();
        // The leak was deferred: a decision, not a ruling, so nothing kept it,
        // and round 2's same claim, on code no fix changed, went to the Judge.
        assert!(run.rulings.is_empty());
        assert_eq!(fake.count(Role::Judge), 2);
        let leak = &run.lenses["alpha"].rounds[1].findings[0];
        assert_eq!(leak.reused_from, None);
        // What the rules say: it blocks, open for a person again.
        assert_eq!(leak.state, FindingState::Open);
        assert_eq!(
            run.lenses["alpha"].rounds[0].findings[0].state,
            FindingState::Deferred
        );
        let _ = fs::remove_dir_all(dir);
    }
    /// A first session out of room: what it found, checked and left.
    const HANDED: &str = "ACRBUS-HANDOFF\n{\"findings\": [{\"title\": \"first half\", \"detail\": \
        \"why\", \"file\": \"a.src\", \"line\": 3}], \"handoff\": {\"checked\": \"the parser\", \
        \"left\": \"the writer, next its flush\"}}";
    /// The next session's answer, one finding of it the first's again.
    const SECOND: &str = r#"{"findings": [{"title": "second half", "detail": "why", "file": "a.src", "line": 9}, {"title": "First half", "detail": "said again", "file": "a.src", "line": 3}]}"#;

    fn prompts(
        fake: &Fake<impl Fn(Role, &str) -> Result<String> + Sync>,
        role: Role,
        has: &str,
    ) -> Vec<String> {
        fake.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == role && c.3.contains(has))
            .map(|c| c.3.clone())
            .collect()
    }

    #[test]
    fn a_session_past_its_size_hands_off_and_the_next_carries_its_findings_through() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "sessions: {relay_tokens: {reviewer: 40000}}",
        );
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && prompt.contains("# Carrying on") => {
                    SECOND.into()
                }
                Role::Reviewer if prompt.contains("alpha") => HANDED.into(),
                Role::Reviewer => CLEAN.into(),
                Role::Judge => r#"{"findings": [{"title": "first half", "file": "a.src", "line": 3, "severity": "Low"}, {"title": "second half", "file": "a.src", "line": 9, "severity": "Low"}]}"#.into(),
                _ => NOTHING.into(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let alpha = prompts(&fake, Role::Reviewer, "alpha");
        assert_eq!(alpha.len(), 2, "two sessions");
        // The second starts with the first's brief, so it shares its cached
        // prefix, and is told what the first checked, left and found.
        assert!(alpha[1].starts_with(&alpha[0]));
        assert!(alpha[1].contains("you are session 2"));
        assert!(alpha[1].contains("- Session 1: the parser"));
        assert!(alpha[1].contains("the writer, next its flush"));
        assert!(alpha[1].contains("- first half (a.src:3)"));
        // Both sessions' findings reach the Judge, each once.
        let judged = prompts(&fake, Role::Judge, "alpha");
        assert_eq!(judged.len(), 1);
        assert!(judged[0].contains("first half") && judged[0].contains("second half"));
        assert!(!judged[0].contains("said again"), "{}", judged[0]);
        // Each session is a call of its own in the usage, marked as a leg.
        let parts: Vec<Option<String>> = run
            .usage
            .iter()
            .filter(|u| {
                u.label
                    .as_deref()
                    .is_some_and(|l| l.starts_with("alpha: reviewer"))
            })
            .map(|u| u.part.clone())
            .collect();
        assert_eq!(parts, vec![Some("leg 1".into()), Some("leg 2".into())]);
        assert!(
            run.usage
                .iter()
                .filter(|u| u.label.as_deref().is_some_and(|l| l.starts_with("beta")))
                .all(|u| u.part.is_none())
        );
        // The report counts it: one call relayed into two sessions.
        let sessions = report::sessions(&run.usage);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            (sessions[0].role, sessions[0].relayed, sessions[0].legs),
            (Role::Reviewer, 1, 2)
        );
        assert!(
            sessions[0]
                .line()
                .contains("1 call(s) relayed, 2 session(s) in all")
        );
        assert_eq!(report::session_lines(&run.usage).len(), 2);
        // Every reviewer's call carried its size; the Judge's, none.
        let relays = fake.relays.lock().unwrap();
        assert!(
            relays
                .iter()
                .filter(|r| r.0 == Role::Reviewer)
                .all(|r| r.1 == Some(40_000))
        );
        assert!(
            relays
                .iter()
                .filter(|r| r.0 == Role::Judge)
                .all(|r| r.1.is_none())
        );
    }

    #[test]
    fn a_handoff_that_cant_be_read_or_makes_no_progress_is_never_a_pass() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "sessions: {relay_tokens: {reviewer: 40000}}",
        );
        let stuck = "ACRBUS-HANDOFF\n{\"findings\": [], \"handoff\": {\"checked\": \"x\", \"left\": \"the same\"}}";
        // Each restarted once, fresh, then a failure, never a pass.
        for (answer, sessions) in [("ACRBUS-HANDOFF\nI ran out of room, sorry", 2), (stuck, 4)] {
            let fake = Fake::new(move |role, prompt: &str| {
                Ok(match role {
                    Role::Reviewer if prompt.contains("alpha") => answer.into(),
                    Role::Reviewer => CLEAN.into(),
                    _ => NOTHING.into(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(
                prompts(&fake, Role::Reviewer, "alpha").len(),
                sessions,
                "{answer}"
            );
            assert_ne!(run.status, Status::Confirmed);
            assert_eq!(run.waiting, Some(Gate::Escalation));
            assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
            assert_eq!(run.lenses["beta"].outcome, Outcome::Confirmed);
            assert!(run.escalations[0].reason.contains("wasn't fully reviewed"));
        }
    }

    #[test]
    fn a_reviewers_handoff_that_checked_more_carries_on_and_one_in_new_words_only_stops() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "sessions: {relay_tokens: {reviewer: 40000}}",
        );
        let first = "ACRBUS-HANDOFF\n{\"findings\": [], \"handoff\": {\"checked\": \"a.src\", \"left\": \"the rest\"}}";
        // The same, in new words: cased, spaced and stopped again.
        let reworded = "ACRBUS-HANDOFF\n{\"findings\": [], \"handoff\": {\"checked\": \"A.src \", \"left\": \"The  rest.\"}}";
        // Nothing found, but more checked: its work.
        let more = "ACRBUS-HANDOFF\n{\"findings\": [], \"handoff\": {\"checked\": \"b.src\", \"left\": \"the rest\"}}";
        // One that stops is restarted once, fresh, and stops again.
        for (second, sessions, stops) in [(reworded, 4, true), (more, 3, false)] {
            let fake = Fake::new(move |role, prompt: &str| {
                Ok(match role {
                    Role::Reviewer
                        if prompt.contains("alpha") && prompt.contains("you are session 3") =>
                    {
                        CLEAN.into()
                    }
                    Role::Reviewer
                        if prompt.contains("alpha") && prompt.contains("you are session 2") =>
                    {
                        second.into()
                    }
                    Role::Reviewer if prompt.contains("alpha") => first.into(),
                    Role::Reviewer => CLEAN.into(),
                    _ => NOTHING.into(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(
                prompts(&fake, Role::Reviewer, "alpha").len(),
                sessions,
                "{second}"
            );
            let told = said(&fake);
            assert_eq!(
                told.contains("did no work: handed off again with nothing new"),
                stops,
                "{told}"
            );
            assert_eq!(run.status == Status::Confirmed, !stops, "{second}");
        }
    }

    #[test]
    fn without_a_relay_a_call_runs_in_one_session_as_before() {
        let cfg = plain("exception", 3, 1);
        assert_eq!(
            cfg.sessions,
            super::super::config::SessionsConfig::default()
        );
        // Even an answer that looks like a handoff is the answer.
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => HANDED.into(),
                Role::Reviewer => CLEAN.into(),
                Role::Judge => rating("Low"),
                _ => NOTHING.into(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(prompts(&fake, Role::Reviewer, "alpha").len(), 1);
        assert!(fake.relays.lock().unwrap().iter().all(|r| r.1.is_none()));
        assert!(run.usage.iter().all(|u| u.part.is_none()));
        assert!(prompts(&fake, Role::Judge, "alpha")[0].contains("first half"));
        assert!(prompts(&fake, Role::Reviewer, "# This session").is_empty());
    }

    #[test]
    fn a_judge_past_its_size_hands_off_and_its_ratings_are_merged() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "sessions: {relay_tokens: {judge: 40000}}",
        );
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => r#"{"findings": [{"title": "A", "file": "a.src", "line": 1}, {"title": "B", "file": "a.src", "line": 2}]}"#.into(),
                Role::Reviewer => CLEAN.into(),
                Role::Judge if prompt.contains("# Carrying on") => r#"{"findings": [{"title": "B", "file": "a.src", "line": 2, "severity": "Low"}]}"#.into(),
                Role::Judge => "ACRBUS-HANDOFF\n{\"findings\": [{\"title\": \"A\", \"file\": \"a.src\", \"line\": 1, \"severity\": \"Low\"}], \"handoff\": {\"checked\": \"A\", \"left\": \"B\"}}".into(),
                _ => NOTHING.into(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        assert_eq!(fake.count(Role::Judge), 2);
        let titles: Vec<String> = run.lenses["alpha"]
            .rounds
            .last()
            .unwrap()
            .findings
            .iter()
            .map(|f| f.title.clone())
            .collect();
        assert!(
            titles.contains(&"A".to_string()) && titles.contains(&"B".to_string()),
            "{titles:?}"
        );
        let parts: Vec<Option<String>> = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Judge)
            .map(|u| u.part.clone())
            .collect();
        assert_eq!(parts, vec![Some("leg 1".into()), Some("leg 2".into())]);
    }

    #[test]
    fn a_scouted_lens_lists_suspicions_and_verifies_each_on_its_own() {
        let cfg = config("exception", 3, 1, "", "sessions: {scout: [alpha]}");
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("# This session: scout") => r#"{"suspects": [{"title": "S1", "file": "a.src", "line": 1, "claim": "c1", "check": "k1"}, {"title": "S2", "file": "a.src", "line": 2, "claim": "c2", "check": "k2"}]}"#.into(),
                Role::Reviewer if prompt.contains("Title: S1") => r#"{"findings": [{"title": "S1 holds", "detail": "evidence", "file": "a.src", "line": 1}]}"#.into(),
                Role::Reviewer => CLEAN.into(),
                Role::Judge => rating("Low"),
                _ => NOTHING.into(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let alpha = prompts(&fake, Role::Reviewer, "alpha");
        assert_eq!(alpha.len(), 3, "a scout, and a check for each suspicion");
        let scout = &alpha[0];
        let brief = &scout[..scout.find("\n\n# This session: scout").unwrap()];
        let checks: Vec<&String> = alpha
            .iter()
            .filter(|p| p.contains("# This session: verify"))
            .collect();
        assert_eq!(checks.len(), 2);
        for c in &checks {
            // From the reviewer's brief and its own suspicion alone.
            assert!(c.starts_with(brief));
            assert!(c.contains("Title: S1") != c.contains("Title: S2"));
        }
        // S1 held and S2 didn't: one finding reaches the Judge.
        let judged = prompts(&fake, Role::Judge, "alpha");
        assert!(judged[0].contains("S1 holds") && !judged[0].contains("S2"));
        let mut parts: Vec<String> = run
            .usage
            .iter()
            .filter(|u| {
                u.label
                    .as_deref()
                    .is_some_and(|l| l.starts_with("alpha: reviewer"))
            })
            .filter_map(|u| u.part.clone())
            .collect();
        parts.sort();
        assert_eq!(parts, vec!["scout", "verify 1", "verify 2"]);
        let sessions = report::sessions(&run.usage);
        assert_eq!(
            (
                sessions[0].scouts,
                sessions[0].verifies,
                sessions[0].relayed
            ),
            (1, 2, 0)
        );
        let json = serde_json::to_value(report::report(&run, &cfg)).unwrap();
        assert_eq!(json["sessions"][0]["verifies"], 2);
        // A lens not scouted reviews as before.
        assert!(!prompts(&fake, Role::Reviewer, "beta")[0].contains("# This session"));
    }

    #[test]
    fn a_suspicion_that_cant_be_checked_fails_the_reviewer() {
        let cfg = config("exception", 3, 1, "", "sessions: {scout: [alpha]}");
        for broken in ["Title: S2", "# This session: scout"] {
            let fake = Fake::new(move |role, prompt: &str| {
                Ok(match role {
                    Role::Reviewer if prompt.contains(broken) => "I couldn't tell.".into(),
                    Role::Reviewer if prompt.contains("# This session: scout") => {
                        r#"{"suspects": [{"title": "S1"}, {"title": "S2"}]}"#.into()
                    }
                    Role::Reviewer => CLEAN.into(),
                    _ => NOTHING.into(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_ne!(run.status, Status::Confirmed, "{broken}");
            assert_eq!(run.lenses["alpha"].outcome, Outcome::Open);
            assert!(run.escalations[0].reason.contains("wasn't fully reviewed"));
        }
    }

    #[test]
    fn a_suspicion_whose_check_cant_be_read_is_checked_again_fresh() {
        let cfg = config("exception", 3, 1, "", "sessions: {scout: [alpha]}");
        for (fresh, confirmed) in [(CLEAN, true), ("still prose", false)] {
            let checks = &Mutex::new(0);
            let fake = Fake::new(move |role, prompt: &str| {
                Ok(match role {
                    Role::Reviewer if prompt.contains("# This session: scout") => {
                        r#"{"suspects": [{"title": "S1"}, {"title": "S2"}]}"#.into()
                    }
                    Role::Reviewer if prompt.contains("Title: S2") => {
                        let mut n = checks.lock().unwrap();
                        *n += 1;
                        if *n == 1 {
                            "I couldn't tell.".into()
                        } else {
                            fresh.into()
                        }
                    }
                    Role::Reviewer => CLEAN.into(),
                    _ => NOTHING.into(),
                })
            });
            let (run, _) = go(&cfg, &["a.src"], &fake);
            assert_eq!(*checks.lock().unwrap(), 2, "S2's check, then once more");
            assert!(
                said(&fake).contains("restarted fresh: alpha: reviewer: verify 2, attempt 2"),
                "{}",
                said(&fake)
            );
            // The scout isn't asked again: only the check that gave no answer.
            let scouts = prompts(&fake, Role::Reviewer, "alpha")
                .iter()
                .filter(|p| p.contains("# This session: scout"))
                .count();
            assert_eq!(scouts, 1);
            assert_eq!(run.status == Status::Confirmed, confirmed, "{fresh}");
        }
    }

    #[test]
    fn a_first_pass_relays_like_the_judge_and_a_reused_ruling_takes_no_session() {
        // Judges relay: the first pass hands off once, and its sessions'
        // ratings settle the lens together, with no Judge call.
        let cfg = config(
            "exception",
            1,
            1,
            "",
            "savings: {first_pass_judge: small}\nsessions: {relay_tokens: {judge: 40000}}",
        );
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") => r#"{"findings": [{"title": "A", "file": "a.src", "line": 1}, {"title": "B", "file": "a.src", "line": 2}]}"#.into(),
                Role::Reviewer => CLEAN.into(),
                Role::Judge if prompt.contains("You're the first pass") && prompt.contains("# Carrying on") => {
                    r#"{"findings": [{"title": "B", "file": "a.src", "line": 2, "severity": "Low"}]}"#.into()
                }
                Role::Judge if prompt.contains("You're the first pass") => "ACRBUS-HANDOFF\n{\"findings\": [{\"title\": \"A\", \"file\": \"a.src\", \"line\": 1, \"severity\": \"Low\"}], \"handoff\": {\"checked\": \"A\", \"left\": \"B\"}}".into(),
                Role::Judge => r#"{"findings": [{"title": "A", "file": "a.src", "line": 1, "severity": "High"}]}"#.into(),
                _ => NOTHING.into(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        let on = |name: &str| {
            calls
                .iter()
                .filter(|c| c.0 == Role::Judge && c.1 == name)
                .count()
        };
        assert_eq!(
            (on("small"), on("large")),
            (2, 0),
            "two first-pass sessions, no Judge"
        );
        drop(calls);
        let found = &run.lenses["alpha"].rounds[0].findings;
        let titles: Vec<&str> = found.iter().map(|f| f.title.as_str()).collect();
        assert!(titles.contains(&"A") && titles.contains(&"B"), "{titles:?}");
        assert!(found.iter().all(|f| f.ruled_by.as_deref() == Some("small")));
        let parts: Vec<Option<String>> = run
            .usage
            .iter()
            .filter(|u| u.fan_out.as_deref() == Some("first-pass Judges"))
            .map(|u| u.part.clone())
            .collect();
        assert_eq!(parts, vec![Some("leg 1".into()), Some("leg 2".into())]);
        // Every Judge call it made carried the relay size.
        assert!(
            fake.relays
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.0 == Role::Judge)
                .all(|r| r.1 == Some(40_000))
        );
    }

    /// Three blockers, two in a.src, from alpha, and one in b.src, from
    /// beta, until a fix has run; the fixer answers with what its blockers
    /// were.
    fn three_blockers(fixed: &Mutex<bool>) -> impl Fn(Role, &str) -> Result<String> + Sync + '_ {
        move |role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if !*fixed.lock().unwrap() => ONE.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge if prompt.contains("alpha criteria") => r#"{"findings": [
                    {"title": "first defect", "file": "a.src", "severity": "High"},
                    {"title": "second defect", "file": "./a.src", "severity": "High"}]}"#
                    .to_string(),
                Role::Judge => {
                    r#"{"findings": [{"title": "third defect", "file": "b.src", "severity": "High"}]}"#
                        .to_string()
                }
                Role::Orchestrator if prompt.contains("Fix these") => {
                    *fixed.lock().unwrap() = true;
                    let which: Vec<&str> = ["first", "second", "third"]
                        .into_iter()
                        .filter(|w| prompt.contains(&format!("{w} defect")))
                        .collect();
                    format!(r#"{{"done": "fixed {}"}}"#, which.join(" and "))
                }
                _ => NOTHING.to_string(),
            })
        }
    }

    #[test]
    fn a_fix_goes_in_groups_by_file_each_a_fresh_session_with_only_its_blockers() {
        let cfg = plain("exception", 3, 1);
        assert!(cfg.savings.group_fixes, "on by default");
        let fixed = Mutex::new(false);
        let fake = Fake::new(three_blockers(&fixed));
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let fixes: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Orchestrator)
            .map(|c| &c.3)
            .collect();
        assert_eq!(fixes.len(), 2, "a.src's two together, b.src's alone");
        assert!(fixes[0].contains("first defect") && fixes[0].contains("second defect"));
        assert!(!fixes[0].contains("third defect"));
        assert!(fixes[1].contains("third defect") && !fixes[1].contains("first defect"));
        // Both open the same way, with only their blockers last, so the
        // second reads the opening from the cache.
        let opening = fixes[0].find("# The blockers").unwrap();
        assert_eq!(fixes[0][..opening], fixes[1][..opening]);
        assert!(fixes[0][..opening].contains(roles::FIX_SCOPE));
        assert!(fixes[0][..opening].contains(roles::FIX_TESTS));
        drop(calls);
        let told = fake.told.lock().unwrap();
        assert!(told.contains(&"call: the Orchestrator, fixing group 1 of 2: 2 blocker(s)".into()));
        assert!(told.contains(&"call: the Orchestrator, fixing group 2 of 2: 1 blocker(s)".into()));
        drop(told);
        // Each group is a leg of its own, on the record.
        let parts: Vec<Option<String>> = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Orchestrator)
            .map(|u| u.part.clone())
            .collect();
        assert_eq!(parts, vec![Some("group 1".into()), Some("group 2".into())]);
        let sessions = report::sessions(&run.usage);
        let o = sessions
            .iter()
            .find(|x| x.role == Role::Orchestrator)
            .unwrap();
        assert_eq!((o.groups, o.relayed), (2, 0));
        assert!(o.line().contains("2 fix group(s)"), "{}", o.line());
        // Each lens is told what its own group did.
        assert_eq!(run.lenses["alpha"].done, ["fixed first and second"]);
        assert_eq!(run.lenses["beta"].done, ["fixed third"]);
    }

    #[test]
    fn a_fix_in_one_group_is_one_call_as_before_and_grouping_can_be_turned_off() {
        let cfg = config("exception", 3, 1, "", "savings: {group_fixes: false}");
        let fixed = Mutex::new(false);
        let fake = Fake::new(three_blockers(&fixed));
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        assert_eq!(fake.count(Role::Orchestrator), 1);
        assert!(
            fake.told
                .lock()
                .unwrap()
                .contains(&"call: the Orchestrator, fixing 3 blocker(s)".into())
        );
        let parts: Vec<Option<String>> = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Orchestrator)
            .map(|u| u.part.clone())
            .collect();
        assert_eq!(parts, vec![None]);
    }

    #[test]
    fn fix_groups_put_one_files_blockers_and_a_same_defect_together_and_the_rest_alone() {
        let f = |id: &str, file: Option<&str>, same_as: Option<&str>| Finding {
            id: id.into(),
            title: format!("{id} title"),
            detail: String::new(),
            file: file.map(str::to_string),
            line: None,
            severity: Severity::High,
            action: Action::Blocks,
            reason: String::new(),
            state: FindingState::Folded,
            note: String::new(),
            same_as: same_as.map(str::to_string),
            reused_from: None,
            ruled_by: None,
        };
        let findings = vec![
            ("alpha".to_string(), f("a1", Some("x.rs"), None)),
            ("alpha".to_string(), f("a2", None, None)),
            ("beta".to_string(), f("b1", Some("y.rs"), None)),
            // The same defect as a1, pointing elsewhere: it goes with a1.
            ("beta".to_string(), f("b2", Some("z.rs"), Some("a1"))),
            ("beta".to_string(), f("b3", Some("./x.rs"), None)),
            ("beta".to_string(), f("b4", None, None)),
        ];
        let reported = vec![Reported {
            id: "you-1".into(),
            round: 1,
            text: "the dates are off".into(),
            fixed: false,
        }];
        let groups = fix_groups(&findings, &reported);
        let ids: Vec<Vec<String>> = groups.iter().map(FixGroup::ids).collect();
        assert_eq!(
            ids,
            vec![
                vec!["a1", "b2", "b3"],
                vec!["a2"],
                vec!["b1"],
                vec!["b4"],
                vec!["you-1"],
            ]
        );
        assert_eq!(groups[0].blockers(), 2, "the same defect counts once");
        let list = fix_list(&groups[0].findings, &groups[0].reported);
        assert!(
            list.contains("The same defect was found by: beta"),
            "{list}"
        );
    }

    #[test]
    fn a_group_that_fails_is_tried_again_from_where_it_stopped() {
        let cfg = plain("exception", 3, 1);
        let fixed = Mutex::new(false);
        let fail = Mutex::new(true);
        let inner = three_blockers(&fixed);
        let fake = Fake::new(|role, prompt: &str| {
            if role == Role::Orchestrator
                && prompt.contains("third defect")
                && std::mem::replace(&mut *fail.lock().unwrap(), false)
            {
                bail!("the fixer's agent went away");
            }
            inner(role, prompt)
        });
        let s = setting(&cfg);
        let c = change(&["a.src", "b.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Waiting);
        let reason = &run.escalations.last().unwrap().reason;
        assert!(reason.contains("group 2 of 2"), "{reason}");
        assert!(reason.contains("beta-r1-1"), "{reason}");
        assert!(
            reason.contains("carries on from the first group not done"),
            "{reason}"
        );
        // What group 1 did is kept for its lens.
        assert_eq!(run.lenses["alpha"].done, ["fixed first and second"]);
        let a = answered(&run, &cfg, &[("Unfixed", "Try it again".into())]);
        assert_eq!(a.len(), 1);
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let fixes: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Orchestrator)
            .map(|c| &c.3)
            .collect();
        assert_eq!(fixes.len(), 3, "group 1 once, group 2 twice");
        assert!(fixes[2].contains("third defect") && !fixes[2].contains("first defect"));
        assert!(run.partial_fix.is_none(), "done with");
        assert_eq!(run.lenses["beta"].done, ["fixed third"]);
    }

    #[test]
    fn the_orchestrators_fix_relays_and_its_next_session_carries_on_from_what_was_changed() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "savings: {group_fixes: false}\nsessions: {relay_tokens: {orchestrator: 60000}}",
        );
        let fixed = Mutex::new(false);
        let inner = three_blockers(&fixed);
        let fake = Fake::new(|role, prompt: &str| {
            if role == Role::Orchestrator && !prompt.contains("# Carrying on") {
                return Ok("ACRBUS-HANDOFF\n{\"done\": \"fixed first\", \"handoff\": {\"checked\": \"first\", \"left\": \"second and third\"}}".into());
            }
            inner(role, prompt)
        });
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let fixes: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Orchestrator)
            .map(|c| &c.3)
            .collect();
        assert_eq!(fixes.len(), 2);
        assert!(fixes[1].starts_with(fixes[0].as_str()), "the same opening");
        assert!(fixes[1].contains("What it changed is in the working directory already"));
        assert!(fixes[1].contains("- Session 1: fixed first"));
        assert!(fixes[1].contains("second and third"));
        assert!(!fixes[1].contains("What they already have, not to repeat"));
        drop(calls);
        assert!(
            fake.relays
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.0 == Role::Orchestrator)
                .all(|r| r.1 == Some(60_000))
        );
        let parts: Vec<Option<String>> = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Orchestrator)
            .map(|u| u.part.clone())
            .collect();
        assert_eq!(parts, vec![Some("leg 1".into()), Some("leg 2".into())]);
        // Every session's work is the fix's.
        assert_eq!(
            run.lenses["alpha"].done,
            ["fixed first\nfixed first and second and third"]
        );
    }

    #[test]
    fn a_fixer_that_hands_off_twice_with_nothing_more_done_stops_its_relay() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "savings: {group_fixes: false}\nsessions: {relay_tokens: {orchestrator: 60000}}",
        );
        let fixed = Mutex::new(false);
        let inner = three_blockers(&fixed);
        let fake = Fake::new(|role, prompt: &str| {
            if role == Role::Orchestrator {
                return Ok("ACRBUS-HANDOFF\n{\"done\": \"fixed first\", \"handoff\": {\"checked\": \"first\", \"left\": \"the rest\"}}".into());
            }
            inner(role, prompt)
        });
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        // Two sessions, then two more restarted fresh.
        assert_eq!(fake.count(Role::Orchestrator), 4);
        let reason = &run.escalations.last().unwrap().reason;
        assert!(reason.contains("came back unusable twice"), "{reason}");
        assert!(reason.contains("nothing more done"), "{reason}");
        assert!(reason.contains("went unfixed"), "{reason}");
    }

    /// What a fixer's calls said, by its progress.
    fn said(fake: &Fake<impl Fn(Role, &str) -> Result<String> + Sync>) -> String {
        fake.told.lock().unwrap().join("\n")
    }

    #[test]
    fn a_fixer_that_hands_off_with_no_change_to_the_tree_did_no_work() {
        // Run G's shape: a session that read, changed nothing, and wrote a
        // handoff whose words were new each time.
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "savings: {group_fixes: false}\nsessions: {relay_tokens: {orchestrator: 60000}}",
        );
        let fixed = Mutex::new(false);
        let inner = three_blockers(&fixed);
        let n = std::sync::atomic::AtomicU32::new(0);
        let mut fake = Fake::new(|role, prompt: &str| {
            if role == Role::Orchestrator {
                let n = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(format!(
                    "ACRBUS-HANDOFF\n{{\"done\": \"read the parser, pass {n}\", \"handoff\": {{\"checked\": \"a.src\", \"left\": \"all three, step {n}\"}}}}"
                ));
            }
            inner(role, prompt)
        });
        fake.still = true;
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(
            fake.count(Role::Orchestrator),
            2,
            "it fails at its first, and at its fresh restart"
        );
        let reason = &run.escalations.last().unwrap().reason;
        assert!(reason.contains("did no work"), "{reason}");
        assert!(reason.contains("no change to the working tree"), "{reason}");
        assert!(
            said(&fake).contains("say: the Orchestrator, fixing 3 blocker(s): did no work:"),
            "{}",
            said(&fake)
        );
        assert_eq!(run.stage, Stage::Done);
    }

    #[test]
    fn a_fix_group_that_changes_nothing_did_no_work_unless_it_declines_each_blocker() {
        let cfg = plain("exception", 3, 1);
        let fixed = Mutex::new(false);
        let mut fake = Fake::new(three_blockers(&fixed));
        fake.still = true;
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(
            fake.count(Role::Orchestrator),
            2,
            "its first group fails, restarted fresh too"
        );
        let reason = &run.escalations.last().unwrap().reason;
        assert!(reason.contains("did no work"), "{reason}");
        assert!(reason.contains("declined none"), "{reason}");
        assert!(said(&fake).contains(": did no work: "), "{}", said(&fake));

        // One that declines each of its blockers, with a reason, did its
        // work: deciding no change is needed.
        let fixed = Mutex::new(false);
        let inner = three_blockers(&fixed);
        let mut fake = Fake::new(|role, prompt: &str| {
            let answer = inner(role, prompt)?;
            if role != Role::Orchestrator || !prompt.contains("Fix these") {
                return Ok(answer);
            }
            let blockers = &prompt[prompt.find("# The blockers").unwrap()..];
            let declined: Vec<String> = blockers
                .lines()
                .filter_map(|l| l.strip_prefix("- "))
                .filter_map(|l| l.split_whitespace().next())
                .map(|id| format!(r#"{{"finding": "{id}", "reason": "the code already does it"}}"#))
                .collect();
            Ok(format!(
                r#"{{"done": "nothing needed changing", "declined": [{}]}}"#,
                declined.join(", ")
            ))
        });
        fake.still = true;
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(fake.count(Role::Orchestrator), 2, "both groups ran");
        assert!(!said(&fake).contains("did no work"), "{}", said(&fake));
        assert!(
            run.escalations
                .iter()
                .all(|e| !e.reason.contains("did no work")),
            "{:?}",
            run.escalations
        );
    }

    #[test]
    fn a_fixers_handoff_carries_its_reading_and_planned_edits_to_the_next_session() {
        let cfg = config(
            "exception",
            3,
            1,
            "",
            "savings: {group_fixes: false}\nsessions: {relay_tokens: {orchestrator: 60000}}",
        );
        let fixed = Mutex::new(false);
        let inner = three_blockers(&fixed);
        let mut fake = Fake::new(|role, prompt: &str| {
            if role == Role::Orchestrator && !prompt.contains("# Carrying on") {
                return Ok(r#"ACRBUS-HANDOFF
{"done": "fixed first", "handoff": {"checked": "first",
 "read": ["a.src: parse() drops the error at line 4", "b.src: the caller retries"],
 "planned": [{"file": "a.src", "change": "return the error from parse()"}],
 "left": "second and third"}}"#
                    .into());
            }
            inner(role, prompt)
        });
        fake.fixes = Some(vec!["a.src".into()]);
        let (run, _) = go(&cfg, &["a.src", "b.src"], &fake);
        assert_eq!(run.status, Status::Confirmed);
        let calls = fake.calls.lock().unwrap();
        let fixes: Vec<&String> = calls
            .iter()
            .filter(|c| c.0 == Role::Orchestrator)
            .map(|c| &c.3)
            .collect();
        assert_eq!(fixes.len(), 2);
        // Every fixer is told what a handoff of its holds.
        assert!(fixes[0].contains(r#""read""#) && fixes[0].contains(r#""planned""#));
        assert!(fixes[0].contains(r#""declined""#));
        let next = fixes[1];
        assert!(next.contains("not to read again"), "{next}");
        assert!(
            next.contains("a.src: parse() drops the error at line 4"),
            "{next}"
        );
        assert!(next.contains("b.src: the caller retries"), "{next}");
        assert!(next.contains("to make now"), "{next}");
        assert!(
            next.contains("a.src: return the error from parse()"),
            "{next}"
        );
    }

    #[test]
    fn a_reviewer_that_calls_no_tool_did_no_work() {
        let cfg = plain("exception", 1, 1);
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.usage = Some(RunUsage {
            calls: 3,
            tool_calls: 0,
            counts_tools: true,
            ..RunUsage::default()
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let told = said(&fake);
        assert!(
            told.contains("did no work: it ended without calling a tool"),
            "{told}"
        );
        assert_ne!(run.status, Status::Confirmed, "never a pass");
        assert_eq!(fake.count(Role::Judge), 0, "nothing of it to rule on");

        // One whose bridge doesn't count tool calls isn't taken for idle.
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.usage = Some(RunUsage {
            calls: 3,
            ..RunUsage::default()
        });
        go(&cfg, &["a.src"], &fake);
        assert!(!said(&fake).contains("did no work"), "{}", said(&fake));

        // One that read: its work.
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.usage = Some(RunUsage {
            calls: 3,
            tool_calls: 4,
            counts_tools: true,
            ..RunUsage::default()
        });
        go(&cfg, &["a.src"], &fake);
        assert!(!said(&fake).contains("did no work"), "{}", said(&fake));
    }

    #[test]
    fn a_judge_that_rates_none_of_its_findings_did_no_work() {
        let cfg = plain("exception", 1, 1);
        let fake = Fake::new(|role, _: &str| {
            Ok(match role {
                Role::Reviewer => ONE.to_string(),
                Role::Judge => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let told = said(&fake);
        assert!(
            told.contains("alpha: the Judge: did no work: it returned no ratings for the 1 finding(s) it was given"),
            "{told}"
        );
        assert_ne!(
            run.status,
            Status::Confirmed,
            "its findings never pass unruled"
        );
    }

    #[test]
    fn a_first_pass_that_rates_nothing_settles_nothing() {
        let cfg = config("exception", 1, 1, "", "savings: {first_pass_judge: small}");
        let fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer => ONE.to_string(),
                Role::Judge if prompt.contains(roles::FIRST_PASS_NOTE) => CLEAN.to_string(),
                Role::Judge => {
                    r#"{"findings": [{"title": "a defect", "file": "a.src", "severity": "Low"}]}"#
                        .to_string()
                }
                _ => NOTHING.to_string(),
            })
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let calls = fake.calls.lock().unwrap();
        let on = |name: &str| {
            calls
                .iter()
                .filter(|c| c.0 == Role::Judge && c.1 == name)
                .count()
        };
        // Each lens's first pass rated nothing, and its restart nothing
        // again, so each lens's Judge rules.
        assert_eq!((on("small"), on("large")), (4, 2));
        drop(calls);
        let told = said(&fake);
        assert!(
            told.contains("alpha: the Judge, first pass: did no work: it returned no ratings for the 1 finding(s) it was given"),
            "{told}"
        );
        assert!(
            told.contains("restarted fresh: alpha: the Judge, first pass, attempt 2, because"),
            "{told}"
        );
        assert!(told.contains("came back unusable twice"), "{told}");
        assert!(told.contains("so the Judge rules"), "{told}");
        let f = &run.lenses["alpha"].rounds[0].findings[0];
        assert_eq!(f.ruled_by.as_deref(), Some("large"));
    }

    #[test]
    fn a_session_that_takes_over_a_fix_in_groups_gets_only_whats_left() {
        let cfg = plain("exception", 3, 1);
        let fixed = Mutex::new(false);
        let inner = three_blockers(&fixed);
        let fake = Fake::new(|role, prompt: &str| {
            if role == Role::Orchestrator && prompt.contains("third defect") {
                bail!("the fixer's agent went away");
            }
            inner(role, prompt)
        });
        let s = setting(&cfg);
        let c = change(&["a.src", "b.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        let a = answered(&run, &cfg, &[("Unfixed", "This session".into())]);
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Fix));
        let brief = fix_brief(&run);
        assert!(brief.contains("third defect"), "{brief}");
        assert!(!brief.contains("- alpha-r1-1 ("), "{brief}");
        assert!(
            brief.contains("Already fixed by the Orchestrator, in the working tree:\n- alpha-r1-1, alpha-r1-2: fixed first and second"),
            "{brief}"
        );
        fixed_by_session(&mut run, &s, &fake, BTreeMap::new()).unwrap();
        assert!(run.partial_fix.is_none());
        assert_eq!(run.stage, Stage::Review);
    }

    #[test]
    fn a_tool_a_calls_model_asked_for_that_its_binding_doesnt_name_is_recorded_and_reported() {
        let cfg = plain("exception", 3, 1);
        let reviewer = cfg.binding(Role::Reviewer, None).unwrap();
        let own = reviewer.tools.clone().unwrap()[0].clone();
        let mut fake = Fake::new(|_, _: &str| Ok(CLEAN.to_string()));
        fake.usage = Some(RunUsage {
            calls: 2,
            input_tokens: 10,
            output_tokens: 10,
            tools: [(own, 3), ("sk_context-slack_query".to_string(), 1)].into(),
            ..RunUsage::default()
        });
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let reviewers: Vec<&Usage> = run
            .usage
            .iter()
            .filter(|u| u.role == Role::Reviewer)
            .collect();
        assert!(!reviewers.is_empty());
        for u in &reviewers {
            assert_eq!(
                u.unnamed_tools,
                [("sk_context-slack_query".to_string(), 1)].into(),
                "{:?}",
                u.label
            );
        }
        let listed = super::super::report::unnamed_tools(&run.usage);
        assert!(listed.len() >= reviewers.len());
        assert!(
            listed[0].line().contains(
                "asked for tool(s) its binding doesn't name: sk_context-slack_query (1×)"
            ),
            "{}",
            listed[0].line()
        );
    }

    #[test]
    fn reattack_all_attacks_every_lens_after_each_fix_and_blocked_only_those_with_blockers() {
        // alpha blocks once; beta passes from the start.
        let rounds_of = |cfg: &Config| -> (u32, usize, usize, Vec<String>) {
            let fake = Fake::new(defect_in("alpha", 1));
            let (run, _) = go(cfg, &["a.src"], &fake);
            (
                run.round,
                run.lenses["alpha"].rounds.len(),
                run.lenses["beta"].rounds.len(),
                fake.told.lock().unwrap().clone(),
            )
        };
        let blocked = config("exception", 3, 1, "", "reattack: blocked");
        let (_, alpha, beta, _) = rounds_of(&blocked);
        assert_eq!(
            (alpha, beta),
            (2, 1),
            "beta passed, and isn't attacked again"
        );
        let all = config("exception", 3, 1, "", "reattack: all");
        assert_eq!(all.reattack, Some(Reattack::All));
        let (_, alpha, beta, told) = rounds_of(&all);
        assert_eq!(
            (alpha, beta),
            (2, 2),
            "every lens attacks the fixed version"
        );
        assert!(
            told.iter()
                .any(|t| t.contains("every lens attacks the new version, as reattack: all asks")),
            "{told:?}"
        );
        // Unset, and nobody asked at scope: the default, said as nobody's.
        let unset = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        let (run, _) = go(&unset, &["a.src"], &fake);
        assert_eq!(run.reattack, None);
        assert_eq!(run.reattacks(), Reattack::Blocked);
        assert!(run.reattack_line().contains("the default: nobody chose"));
        // Asked at scope when unset, and the answer is kept.
        let asked = plain("ends", 3, 1);
        let s = setting(&asked);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert!(headers(&run, &asked).contains(&"Re-attack".to_string()));
        let a = answered(
            &run,
            &asked,
            &[
                ("Scope", "Approve".into()),
                ("Fixer", "The Orchestrator".into()),
                ("Re-attack", "Every lens".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &asked, &a, 1).unwrap();
        assert_eq!(run.reattack, Some(Reattack::All));
        assert!(!headers(&run, &asked).contains(&"Re-attack".to_string()));
    }

    #[test]
    fn a_fixers_restart_builds_on_the_edits_its_lost_session_left() {
        let cfg = plain("exception", 3, 1);
        let fixes = Mutex::new(0);
        let mut fake = Fake::new(|role, prompt: &str| {
            Ok(match role {
                Role::Reviewer if prompt.contains("alpha") && *fixes.lock().unwrap() == 0 => {
                    ONE.to_string()
                }
                Role::Reviewer => CLEAN.to_string(),
                Role::Judge => rating("High"),
                // The first session edits, then answers in words no repair
                // reads; its restart finishes the job.
                Role::Orchestrator if prompt.contains("# Already in the working tree") => {
                    *fixes.lock().unwrap() += 1;
                    r#"{"done": "checked the edits and finished the fix"}"#.to_string()
                }
                Role::Orchestrator if prompt.contains("Fix these") => "I fixed it.".to_string(),
                _ => NOTHING.to_string(),
            })
        });
        fake.edits_once = true;
        fake.fixes = Some(vec!["a.src".into()]);
        let (run, _) = go(&cfg, &["a.src"], &fake);
        let told = said(&fake);
        assert!(!told.contains("did no work"), "{told}");
        assert!(told.contains("restarted fresh"), "{told}");
        assert_eq!(*fixes.lock().unwrap(), 1);
        assert_eq!(run.status, Status::Confirmed, "{:?}", failures(&run));
    }

    #[test]
    fn an_outage_at_the_sizer_pauses_the_review_saved_instead_of_failing_a_step() {
        let cfg = plain("exception", 3, 1);
        let down = std::sync::atomic::AtomicBool::new(true);
        let fake = Fake::new(|role, _: &str| {
            if role == Role::Sizer && down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(anyhow::Error::new(CallFailed {
                    failure: Failure::Unreachable,
                    message: "fetch failed (ECONNREFUSED)".into(),
                }));
            }
            Ok(match role {
                Role::Sizer => r#"{"tier": "small", "sure": true, "reason": "docs"}"#.to_string(),
                Role::Reviewer => CLEAN.to_string(),
                _ => NOTHING.to_string(),
            })
        });
        let s = setting(&cfg);
        let c = change(&["docs/a.md"]);
        // The Sizer runs as the review starts: an outage there is said as
        // one, not taken for a tier it couldn't choose.
        let e = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap_err();
        assert!(is_outage(&e), "{e:#}");
        assert!(said(&fake).contains("its model couldn't be reached, so it waits"));
        // Back up, it goes on.
        down.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.tier, "small");
        assert!(
            !run.escalations
                .iter()
                .any(|e| e.reason.contains("unreachable"))
        );
    }

    #[test]
    fn a_fixing_sessions_brief_reaches_the_next_rounds_reviewers() {
        let cfg = plain("exception", 3, 1);
        let fake = Fake::new(defect_in("alpha", 1));
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        run.fixer = Fixer::Session;
        let reload = |_| Ok(c.clone());
        drive(&mut run, &s, &fake, &reload).unwrap();
        assert_eq!(run.waiting, Some(Gate::Fix));
        assert!(!run.converge);
        let briefs = BTreeMap::from([(
            "alpha".to_string(),
            "Start at the new lock in a.src.".to_string(),
        )]);
        fixed_by_session(&mut run, &s, &fake, briefs).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        let alpha = prompts(&fake, Role::Reviewer, "alpha");
        assert!(alpha.len() >= 2, "{}", alpha.len());
        assert!(!alpha[0].contains("Start at the new lock"));
        assert!(
            alpha
                .last()
                .unwrap()
                .contains("Start at the new lock in a.src."),
            "round 2 gets the fixing session's brief"
        );
    }

    #[test]
    fn a_session_that_says_it_fixed_with_the_tree_unchanged_did_no_work() {
        let cfg = plain("exception", 3, 1);
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.still = true;
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        run.fixer = Fixer::Session;
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Fix));
        let e = fixed_by_session(&mut run, &s, &fake, BTreeMap::new()).unwrap_err();
        assert!(format!("{e:#}").contains("did no work"), "{e:#}");
        assert!(said(&fake).contains("did no work: it said the fixes are in"));
        // Still waiting for the fixes, in the same round.
        assert_eq!((run.waiting, run.round), (Some(Gate::Fix), 1));
    }

    /// What a fix changed: the file its blocker points at, a rebuilt
    /// binary, and a doc nothing named.
    fn rebuilt() -> Option<Vec<super::super::change::Changed>> {
        let c = |path: &str, binary: bool, existed: bool| super::super::change::Changed {
            path: path.into(),
            binary,
            existed,
        };
        Some(vec![
            c("a.src", false, true),
            c("build/tool.bin", true, true),
            c("notes.md", false, false),
        ])
    }

    #[test]
    fn a_fix_that_changes_a_binary_or_a_file_no_blocker_names_is_flagged_and_asked_at_the_next_gate()
     {
        let cfg = plain("ends", 3, 1);
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.changed = rebuilt();
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let reload = |_| Ok(c.clone());
        drive(&mut run, &s, &fake, &reload).unwrap();
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "Approve".into()),
                ("Fixer", "The Orchestrator".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        // The fix ran, and the next gate is the result's.
        assert_eq!(run.waiting, Some(Gate::Final));
        let told = fake.told.lock().unwrap().join("\n");
        assert!(
            told.contains(
                "say: round 1's fix changed 3 file(s): a.src, build/tool.bin, notes.md; flagged \
                 for you to keep or revert at the next gate, and the report: build/tool.bin (a \
                 binary, and no blocker points at it), notes.md (no blocker points at it)"
            ),
            "{told}"
        );
        let flagged: Vec<(&str, bool, bool, FlagState)> = run
            .flags
            .iter()
            .map(|f| (f.path.as_str(), f.binary, f.outside, f.state))
            .collect();
        assert_eq!(
            flagged,
            [
                ("build/tool.bin", true, true, FlagState::Open),
                ("notes.md", false, true, FlagState::Open),
            ]
        );
        assert_eq!(headers(&run, &cfg), ["Flagged", "Result", "Missed"]);
        let ask = super::super::carousel::ask(&run, &cfg, "acrbus");
        let q = &ask.carousels[0].questions[0];
        assert!(q.multi_select);
        let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, ["build/tool.bin", "notes.md", "Keep them"]);
        // Each with the exact command that reverts it.
        assert!(q.options[0].description.contains(&run.flags[0].command));
        assert!(q.options[1].description.contains(&run.flags[1].command));
        assert!(run.flags[1].command.starts_with("rm -f -- "));
        // In the report too.
        assert_eq!(report::report(&run, &cfg).flags.len(), 2);
        // Nothing is reverted until a person says.
        assert!(fake.reverted.lock().unwrap().is_empty());
        let a = answered(
            &run,
            &cfg,
            &[
                ("Flagged", serde_json::json!(["build/tool.bin"])),
                ("Result", "Approve".into()),
                ("Missed", "Not now".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        assert_eq!(run.status, Status::Confirmed);
        let reverted = fake.reverted.lock().unwrap();
        assert_eq!(reverted.len(), 1);
        assert_eq!(reverted[0].0, "build/tool.bin");
        assert_eq!(reverted[0].1, run.flags[0].from);
        assert!(reverted[0].2);
        assert_eq!(
            run.flags.iter().map(|f| f.state).collect::<Vec<_>>(),
            [FlagState::Reverted, FlagState::Kept]
        );
        assert!(
            report::flag_line(&run.flags[1])
                .contains("round 1's fix changed notes.md (no blocker points at it): kept")
        );
    }

    #[test]
    fn a_file_the_blockers_name_is_not_flagged_and_a_revert_that_fails_asks_again() {
        let cfg = plain("ends", 3, 1);
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.changed = rebuilt();
        fake.stuck = true;
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        let reload = |_| Ok(c.clone());
        drive(&mut run, &s, &fake, &reload).unwrap();
        let a = answered(
            &run,
            &cfg,
            &[
                ("Scope", "Approve".into()),
                ("Fixer", "The Orchestrator".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        assert!(!run.flags.iter().any(|f| f.path == "a.src"));
        let a = answered(
            &run,
            &cfg,
            &[
                ("Flagged", serde_json::json!(["notes.md", "build/tool.bin"])),
                ("Result", "Approve".into()),
                ("Missed", "Not now".into()),
            ],
        );
        super::super::carousel::apply(&mut run, &cfg, &a, 1).unwrap();
        drive(&mut run, &s, &fake, &reload).unwrap();
        assert_eq!(run.waiting, Some(Gate::Escalation));
        let reasons: Vec<&str> = run.escalations.iter().map(|e| e.reason.as_str()).collect();
        assert!(
            reasons
                .iter()
                .any(|r| r.contains("notes.md couldn't be reverted")
                    && r.contains(&run.flags[1].command)),
            "{reasons:?}"
        );
        // Asked again, with the escalation.
        assert_eq!(headers(&run, &cfg), ["Escalated", "Escalated", "Flagged"]);
    }

    #[test]
    fn a_fix_by_the_session_that_ran_the_review_is_flagged_too() {
        let cfg = plain("exception", 3, 1);
        let mut fake = Fake::new(defect_in("alpha", 1));
        fake.changed = rebuilt();
        let s = setting(&cfg);
        let c = change(&["a.src"]);
        let mut run = start("r", Path::new("."), "main", None, &s, &c, &fake).unwrap();
        run.fixer = Fixer::Session;
        drive(&mut run, &s, &fake, &|_| Ok(c.clone())).unwrap();
        assert_eq!(run.waiting, Some(Gate::Fix));
        fixed_by_session(&mut run, &s, &fake, BTreeMap::new()).unwrap();
        assert_eq!(run.flags.len(), 2);
        assert!(run.flags.iter().all(|f| f.round == 1));
    }
}
