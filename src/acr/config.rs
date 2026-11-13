//! ACR's config: what a review may do, in layers. Nothing here knows about
//! any repository: every lens, path and language comes from these files.
//!
//! The layers load in order, each overriding the one before: the framework's
//! defaults, the user's config, the repository's, and this run's flags. The
//! repository's can choose only among what the user's allows, and it has no
//! keys for secrets, tools or commands, so an unknown key stops it dead.
//!
//! The repository's layer needn't be in the repository. It can be a file in
//! the user's `acr/` directory, so that no review config ever goes into the
//! repositories being reviewed.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::config::APP_NAME;
use anyhow::{Context, Result, bail};
use globset::Glob;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Where a repository keeps its config.
pub const REPO_CONFIG: &str = ".acr/config.yaml";
pub const VERSION: u32 = 1;
/// The schema every config is checked against, published with the binary.
pub const SCHEMA: &str = include_str!("../../schema/acr.schema.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Sizer,
    Orchestrator,
    Reviewer,
    Judge,
    /// Configures a repository's review from what's in it, outside any
    /// review: `acr configure` and `acr calibrate`. A strong model. Without
    /// its own binding it uses the Judge's.
    Configurer,
}

impl Role {
    /// The roles a review needs bound. The configurer isn't one.
    pub const ALL: [Role; 4] = [Role::Sizer, Role::Orchestrator, Role::Reviewer, Role::Judge];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Sizer => "sizer",
            Role::Orchestrator => "orchestrator",
            Role::Reviewer => "reviewer",
            Role::Judge => "judge",
            Role::Configurer => "configurer",
        }
    }

    /// These only read. The Orchestrator also applies fixes.
    fn read_only(self) -> bool {
        self != Role::Orchestrator
    }
}

/// When a person is asked, on top of the escalations that always reach them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HumanGate {
    /// At every step: they approve the scope, decide what's folded in each
    /// round, and approve the result. The default for a review.
    InTheLoop,
    /// They decide what's folded in, each round.
    PerRound,
    /// They approve the scope and the final result.
    Ends,
    /// Only escalations.
    Exception,
}

impl HumanGate {
    pub fn as_str(self) -> &'static str {
        match self {
            HumanGate::InTheLoop => "in-the-loop",
            HumanGate::PerRound => "per-round",
            HumanGate::Ends => "ends",
            HumanGate::Exception => "exception",
        }
    }

    /// Where a person is asked that `other` doesn't ask them.
    fn asks_more_than(self, other: HumanGate) -> bool {
        (self.asks_scope() && !other.asks_scope())
            || (self.asks_fold() && !other.asks_fold())
            || (self.asks_final() && !other.asks_final())
    }

    /// They approve the scope before any reviewer runs.
    pub fn asks_scope(self) -> bool {
        matches!(self, HumanGate::InTheLoop | HumanGate::Ends)
    }

    /// They decide which blockers are fixed, each round.
    pub fn asks_fold(self) -> bool {
        matches!(self, HumanGate::InTheLoop | HumanGate::PerRound)
    }

    /// They approve the result once every lens agrees.
    pub fn asks_final(self) -> bool {
        matches!(self, HumanGate::InTheLoop | HumanGate::Ends)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewScope {
    WholePr,
    BottomOfStack,
}

/// Fixed across repositories, so that results stay comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Rejected,
}

/// What a severity does to a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Blocks,
    BlocksUnlessWaived,
    NeverBlocks,
    Drop,
}

impl Severity {
    pub fn default_action(self) -> Action {
        match self {
            Severity::Critical | Severity::High => Action::Blocks,
            // A narrow-case defect is logged, as the full in-session method
            // rules: its blockers are realistic-case defects, weak pins and
            // misleading claims, which the Judge's facts raise to High. A
            // lens's `severity` can still make a Medium block.
            Severity::Medium | Severity::Low => Action::NeverBlocks,
            Severity::Rejected => Action::Drop,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierMatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    pub id: String,
    #[serde(rename = "match")]
    pub rule: TierMatch,
    pub fan_out: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Triggers {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lens {
    /// Stable, so results can be compared over time.
    pub id: String,
    /// What's wrong, in this lens: the bar its reviewers and Judge hold to.
    pub criteria: String,
    /// How to attack in this lens: the techniques that find what the
    /// criteria call wrong, such as running new tests in another time zone
    /// or in random order. Every reviewer of the lens gets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attack: Option<String>,
    /// What blocks in this lens: its dimension's definition of a blocker,
    /// set when the repository is calibrated, from the dimensions its full
    /// review used. Reviewers get it, and a finding the Judge shows meets
    /// it, quoting the words it meets, blocks. Left out, the Judge's
    /// severity and the general rules decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
    pub triggers: Triggers,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// What a severity does in this lens, if not its default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub severity: BTreeMap<Severity, Action>,
    /// This lens's reviewers get generated files in the diff too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_generated: bool,
}

fn yes() -> bool {
    true
}

/// Generated files, which the diff a review sends leaves out, listing each
/// with its line counts. Reviewers can still read them in the repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generated {
    /// Leave generated files out. On unless a config turns it off.
    #[serde(default = "yes")]
    pub exclude: bool,
    /// More paths that are generated, besides the naming conventions of
    /// code generators that ACR knows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Paths never treated as generated, whatever says they are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<String>,
}

impl Default for Generated {
    fn default() -> Generated {
        Generated {
            exclude: true,
            paths: Vec::new(),
            keep: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Labels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_ready: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready: Option<String>,
    /// `{lens}` is the lens's ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommentFormat {
    Summary,
    Full,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Integrations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<CommentFormat>,
    /// What a review waiting for you sends to Grayboard, when config.toml
    /// has `[grayboard]`: its questions in full, the default, or only that
    /// it waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grayboard: Option<Share>,
}

/// How much of a review's questions go to Grayboard, and so to your Slack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Share {
    /// The questions and their options, to answer by replying.
    Full,
    /// Only "review X needs your answer: N questions"; you answer at the
    /// review's machine.
    Nudge,
}

/// What a repository sets. The user's config and a run's flags can set it
/// too. Each layer's values replace the last's, except that `bindings`
/// merge role by role.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<Tier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lenses: Option<Vec<Lens>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_findings: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_gate: Option<HumanGate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    /// Which lenses attack again after a fix. Left out, the review asks at
    /// its scope gate, or takes `blocked` if nobody is asked there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reattack: Option<Reattack>,
    /// Who may answer a review's questions: `person` takes answers only
    /// from a terminal or Grayboard, never `acr resume --answers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Answers>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_escalate: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_scope: Option<ReviewScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrations: Option<Integrations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated: Option<Generated>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<Role, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub costs: Option<Costs>,
    /// What every round does besides its lenses' reviewers. See `Stage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stages: Option<Vec<Stage>>,
    /// How the repository's tests run, for the mutation stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<Tests>,
    /// How to work in this repository on this machine: setting up a copy,
    /// running its tests and probes, guarding a command that could hang,
    /// and where dependency and SDK sources are. Every role that can run
    /// things gets it, and the brief stage too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runbook: Option<String>,
    /// The models a review of this repository may use, as patterns such as
    /// `example/*`: every binding a review would call, fallbacks too, must name
    /// a model one matches. Left out, any model the user's bindings name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_models: Option<Vec<String>>,
    /// How a round shares what tools found in the code with its roles, for
    /// the model's prompt cache, and how its Judges and reviewers are asked.
    /// See `FactsPolicy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<FactsPolicy>,
    /// How a review saves on its Judges and its later rounds, without taking
    /// anything a role may read or use. See `SavingsPolicy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub savings: Option<SavingsPolicy>,
    /// How long calls are broken up, so a call's context doesn't grow every
    /// turn without end. See `SessionsPolicy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<SessionsPolicy>,
}

/// How a role's long calls are broken into shorter sessions, each starting
/// from the shared opening the prompt cache holds, instead of one session
/// that rereads everything it has read on every turn. Off unless set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsPolicy {
    /// By role, `reviewer`, `judge` or `orchestrator`: the prompt tokens of
    /// one turn past which a call hands off to a fresh session. It writes
    /// its findings so far, what it has checked and what's left, and a
    /// fresh session on the same binding carries on from that. An
    /// Orchestrator's relay is for its fixes: what it has fixed so far, and
    /// what's left to fix. Only agents acrbus can steer mid-call relay;
    /// others run whole.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relay_tokens: BTreeMap<Role, u64>,
    /// Lenses whose reviewers scout first: one session lists what it
    /// suspects, without verifying, and each suspicion is then confirmed or
    /// dropped in a short session of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scout: Option<Vec<String>>,
    /// Seconds a call may log no new turn before progress warns of it,
    /// once, until it moves again: a call waiting on a slow provider uses no
    /// CPU and changes no file, so its bridge's quiet-run watch never sees
    /// it. Nothing is cut off for it. 0 turns the warning off. Default 180.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_warn_secs: Option<u64>,
}

impl SessionsPolicy {
    fn over(mut self, later: SessionsPolicy) -> SessionsPolicy {
        self.relay_tokens.extend(later.relay_tokens);
        self.scout = later.scout.or(self.scout);
        self.stall_warn_secs = later.stall_warn_secs.or(self.stall_warn_secs);
        self
    }
}

/// The least a relay may be set to: well past any shared opening, so a
/// session has room to work before it hands off.
pub const RELAY_MIN_TOKENS: u64 = 32_000;

/// How long a call may log no new turn before progress warns, unless set.
pub const STALL_WARN_SECS: u64 = 180;

/// The sessions settings, resolved. A run's stored config from before a
/// field was added reads it as its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsConfig {
    pub relay_tokens: BTreeMap<Role, u64>,
    pub scout: Vec<String>,
    pub stall_warn_secs: u64,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        SessionsConfig {
            relay_tokens: BTreeMap::new(),
            scout: Vec::new(),
            stall_warn_secs: STALL_WARN_SECS,
        }
    }
}

impl SessionsConfig {
    fn of(p: Option<SessionsPolicy>) -> SessionsConfig {
        let p = p.unwrap_or_default();
        SessionsConfig {
            relay_tokens: p.relay_tokens,
            scout: p.scout.unwrap_or_default(),
            stall_warn_secs: p.stall_warn_secs.unwrap_or(STALL_WARN_SECS),
        }
    }

    /// Where `role`'s calls hand off, if they do.
    pub fn relay(&self, role: Role) -> Option<u64> {
        self.relay_tokens.get(&role).copied()
    }

    /// Whether `lens`'s reviewers scout, then verify.
    pub fn scouts(&self, lens: &str) -> bool {
        self.scout.iter().any(|l| l == lens)
    }
}

/// How a round shares the change map and diff with its roles, ahead of
/// their own text, so a role's calls read them from the prompt cache. Each
/// field left out takes the framework's default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsPolicy {
    /// Build the change map each round and give it, with the diff, to every
    /// reviewer and Judge ahead of their own text. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<bool>,
    /// Seconds a fan-out of one role waits for its first call to fill the
    /// cache before the rest start. 0 starts them all at once. Default 60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_secs: Option<u64>,
    /// Seconds between the starts of a fan-out's other calls once its
    /// first has warmed the cache: a burst of new sessions can be spread
    /// over a provider's replicas, of which only one holds the prefix, where
    /// calls a little apart land on it. 0 starts them all at once. Default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stagger_secs: Option<u64>,
    /// Seconds between refreshes of a role's cached prefix while its calls
    /// run, so the provider doesn't evict it. 0 turns it off. Default 240.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive_secs: Option<u64>,
    /// `per-lens`, the default: one Judge rates a lens's findings together.
    /// `per-finding`: one Judge per finding, given its claim and the lines it
    /// cites, without the reviewer's reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judges: Option<JudgeMode>,
    /// `all`, the default: reviewers get the briefs written for the change
    /// and the decided non-issues, as before. `facts-only`: nothing a model
    /// wrote, and no decided non-issues, which the Judge still applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewers: Option<ReviewerInput>,
    /// Give every role on a bridge the same tool definitions, the union of
    /// theirs, and hold each to its own when it calls one, so roles on one
    /// model share a cached prefix. Only for agents that can enforce it.
    /// Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_tools: Option<bool>,
    /// Learn which files this repository's reviewers open, and give the
    /// ones they keep opening whole in later rounds' facts, and in the next
    /// review's. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned: Option<bool>,
    /// The most bytes of such files a round's facts carry. Default 24576.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_bytes: Option<u64>,
}

impl FactsPolicy {
    fn over(self, later: FactsPolicy) -> FactsPolicy {
        FactsPolicy {
            shared: later.shared.or(self.shared),
            warm_secs: later.warm_secs.or(self.warm_secs),
            stagger_secs: later.stagger_secs.or(self.stagger_secs),
            keep_alive_secs: later.keep_alive_secs.or(self.keep_alive_secs),
            judges: later.judges.or(self.judges),
            reviewers: later.reviewers.or(self.reviewers),
            match_tools: later.match_tools.or(self.match_tools),
            learned: later.learned.or(self.learned),
            learned_bytes: later.learned_bytes.or(self.learned_bytes),
        }
    }
}

/// How a review saves on its Judges and its later rounds. None of these
/// takes anything from a role: every reviewer and Judge keeps its tools and
/// what it may read. Each field left out takes the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavingsPolicy {
    /// Carry a Judge's ruling over to a later round's finding that makes
    /// the same claim, in the same lens, at the same place in a file no fix
    /// has changed since, when that ruling didn't block. Anything else goes
    /// to a Judge. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reuse_rulings: Option<bool>,
    /// A binding that rules on findings first, such as a cheaper model.
    /// What it would have block, rates Medium or above, calls a split or a
    /// values call, or marks unsure goes to the Judge's binding, which rules
    /// on it fresh, without the first pass's ruling. Left out, none: every
    /// finding goes to the Judge. Check it with `acr judge-check` first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass_judge: Option<String>,
    /// After a fix, tell each lens's reviewers what it changed and what it
    /// could affect, its references and tests, as where to look first. The
    /// whole change stays theirs. Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_after_fix: Option<bool>,
    /// After a fix, ask one small call, on the Sizer's binding, whether each
    /// lens that passed before the fix needs to attack again. A lens it
    /// skips is reported as skipped, with the reason, never as passing the
    /// new version. A lens with blockers of its own always attacks again.
    /// Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blast_radius: Option<bool>,
    /// Fix a round's blockers in groups, each in a fresh session on the
    /// Orchestrator's binding: blockers in the same files together, the
    /// rest each alone, one group after another in the checkout, each
    /// seeing what the groups before it changed. One long session for every
    /// blocker rereads all it has done on every turn. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_fixes: Option<bool>,
    /// A cheaper binding for the fix groups the rules call easy: every
    /// blocker in the group at or below `max_severity`, the group citing at
    /// most `max_files` files, each with its cause stated, file, line and
    /// detail, none a person's call or a defect a person reported missed,
    /// and none like one fixed in an earlier round. Every other group, and
    /// an easy one's restart after its fix failed, goes to the
    /// Orchestrator's binding, as do its other calls. Left out, the
    /// Orchestrator's binding fixes every group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixer: Option<FixerPolicy>,
}

/// `savings.fixer`: the binding easy fix groups go to, and what makes one
/// easy. Each threshold left out takes its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixerPolicy {
    /// A binding in `allowed_bindings` that may write.
    pub binding: String,
    /// The most severe a blocker in an easy group may be: High by default,
    /// and never Critical, which always goes to the Orchestrator's binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_severity: Option<Severity>,
    /// The most files an easy group's blockers may cite. Default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u32>,
}

/// `savings.fixer`, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixerConfig {
    pub binding: String,
    pub max_severity: Severity,
    pub max_files: u32,
}

impl SavingsPolicy {
    fn over(self, later: SavingsPolicy) -> SavingsPolicy {
        SavingsPolicy {
            reuse_rulings: later.reuse_rulings.or(self.reuse_rulings),
            first_pass_judge: later.first_pass_judge.or(self.first_pass_judge),
            focus_after_fix: later.focus_after_fix.or(self.focus_after_fix),
            blast_radius: later.blast_radius.or(self.blast_radius),
            group_fixes: later.group_fixes.or(self.group_fixes),
            fixer: later.fixer.or(self.fixer),
        }
    }
}

/// The savings settings, resolved. A run's stored config from before a
/// field was added reads it as its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavingsConfig {
    pub reuse_rulings: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_pass_judge: Option<String>,
    pub focus_after_fix: bool,
    pub blast_radius: bool,
    pub group_fixes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixer: Option<FixerConfig>,
}

impl Default for SavingsConfig {
    fn default() -> Self {
        SavingsConfig {
            reuse_rulings: true,
            first_pass_judge: None,
            focus_after_fix: false,
            blast_radius: false,
            group_fixes: true,
            fixer: None,
        }
    }
}

impl SavingsConfig {
    fn of(p: Option<SavingsPolicy>) -> SavingsConfig {
        let d = SavingsConfig::default();
        let p = p.unwrap_or_default();
        SavingsConfig {
            reuse_rulings: p.reuse_rulings.unwrap_or(d.reuse_rulings),
            first_pass_judge: p.first_pass_judge.or(d.first_pass_judge),
            focus_after_fix: p.focus_after_fix.unwrap_or(d.focus_after_fix),
            blast_radius: p.blast_radius.unwrap_or(d.blast_radius),
            group_fixes: p.group_fixes.unwrap_or(d.group_fixes),
            fixer: p
                .fixer
                .map(|f| FixerConfig {
                    binding: f.binding,
                    max_severity: f.max_severity.unwrap_or(Severity::High),
                    max_files: f.max_files.unwrap_or(1),
                })
                .or(d.fixer),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JudgeMode {
    #[default]
    PerLens,
    PerFinding,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewerInput {
    #[default]
    All,
    FactsOnly,
}

/// The facts settings, resolved. A run's stored config from before a field
/// was added reads it as its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FactsConfig {
    pub shared: bool,
    pub warm_secs: u64,
    pub stagger_secs: u64,
    pub keep_alive_secs: u64,
    pub judges: JudgeMode,
    pub reviewers: ReviewerInput,
    pub match_tools: bool,
    pub learned: bool,
    pub learned_bytes: u64,
}

impl Default for FactsConfig {
    fn default() -> Self {
        FactsConfig {
            shared: true,
            warm_secs: 60,
            stagger_secs: 3,
            keep_alive_secs: 240,
            judges: JudgeMode::PerLens,
            reviewers: ReviewerInput::All,
            match_tools: true,
            learned: true,
            learned_bytes: 24 * 1024,
        }
    }
}

impl FactsConfig {
    fn of(p: Option<FactsPolicy>) -> FactsConfig {
        let d = FactsConfig::default();
        let p = p.unwrap_or_default();
        FactsConfig {
            shared: p.shared.unwrap_or(d.shared),
            warm_secs: p.warm_secs.unwrap_or(d.warm_secs),
            stagger_secs: p.stagger_secs.unwrap_or(d.stagger_secs),
            keep_alive_secs: p.keep_alive_secs.unwrap_or(d.keep_alive_secs),
            judges: p.judges.unwrap_or(d.judges),
            reviewers: p.reviewers.unwrap_or(d.reviewers),
            match_tools: p.match_tools.unwrap_or(d.match_tools),
            learned: p.learned.unwrap_or(d.learned),
            learned_bytes: p.learned_bytes.unwrap_or(d.learned_bytes),
        }
    }
}

/// What a careful reviewer does by habit, made part of every round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// First in each round, before any reviewer: are the change's tests
    /// enough? Changed lines no test runs, from a coverage report, and
    /// breaks in the changed code every test passes, from mutants. Gaps
    /// are the `tests` lens's findings, and a gap that blocks stops the
    /// round there, sparing the reviewers' tokens until the tests are
    /// written. It runs the mutation stage's mutants itself.
    Gate,
    /// Before each round's reviewers, the Orchestrator writes each lens a
    /// brief for this change: where in it to attack, and how. It reads the
    /// change and the code, with no case of its own to make for it.
    Brief,
    /// Every role gets what the repository's history says about the
    /// changed files: earlier commits to them, docs and tests that mention
    /// them, and where the functions the change touches are used.
    Context,
    /// A lens that checks, one by one, every claim the change makes about
    /// itself.
    Claims,
    /// A lens that proposes small breaks in the changed code, which the
    /// pipeline runs against the tests in copies of its own. A break the
    /// tests miss is a finding.
    Mutation,
    /// A lens that writes and runs scenarios of what the change says it
    /// does. Its binding needs a shell.
    Probes,
    /// Between rounds of a review to convergence, after the steelman's
    /// fixes: a fresh writer, which didn't make them, pins each fixed
    /// blocker with a test and closes the gate's gaps. Each test it writes
    /// is run on the code from before the fixes, where it must fail, and
    /// after, where it must pass.
    TestWriting,
}

/// How the repository's tests run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tests {
    /// The command, run from the repository's root. `{tests}` in it stands
    /// for the test files to run. Without it, it's found from the
    /// repository's files: Flutter, Cargo, Go, npm or pytest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// How many mutants run at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<u32>,
    /// Seconds one run of the tests may take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// How many mutants a reviewer proposes in a round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutants: Option<u32>,
    /// A command run once a round, from the root of a copy of the change,
    /// before any reviewer's copy is made from it: the package install and
    /// a first build, so every copy starts warm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    /// The command that runs the tests with coverage, from the root, for
    /// the gate: it writes an lcov report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
    /// Where that report is, from the root: `coverage/lcov.info` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    /// What a gap that blocks does: `warn`, the default, and let the
    /// reviewers run, or `stop` the round before them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateMode>,
    /// Other ways the gate runs the change's own tests, each once, such as
    /// in random order, one at a time, or in another time zone. A test that
    /// passes plainly and fails one of these depends on what it varies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<Variant>,
    /// How many more times the gate runs the change's own tests plainly,
    /// after the first, to catch one that passes only sometimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<u32>,
}

/// A way to run the change's tests: its name, and its command, from the
/// root, with `{tests}` for the test files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    pub name: String,
    pub command: String,
}

/// What the test gate does with a gap that blocks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateMode {
    /// The round ends before any reviewer runs. It spares the reviewers'
    /// tokens, but what they'd have found waits a round, after the tests
    /// have been rewritten once already.
    Stop,
    /// The reviewers run anyway, each told what the gate found.
    #[default]
    Warn,
}

/// US dollars per million tokens. Cached input left out costs what other
/// input does, which is the most it could.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Input read from a cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    /// Input written to a cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
}

impl Price {
    pub fn usd(&self, input: u64, cache_read: u64, cache_write: u64, output: u64) -> f64 {
        let per_million = |tokens: u64, rate: f64| tokens as f64 / 1_000_000.0 * rate;
        per_million(input, self.input_per_mtok)
            + per_million(
                cache_read,
                self.cached_input_per_mtok.unwrap_or(self.input_per_mtok),
            )
            + per_million(
                cache_write,
                self.cache_write_per_mtok.unwrap_or(self.input_per_mtok),
            )
            + per_million(output, self.output_per_mtok)
    }

    fn rates(&self) -> [f64; 4] {
        [
            self.input_per_mtok,
            self.output_per_mtok,
            self.cached_input_per_mtok.unwrap_or(0.0),
            self.cache_write_per_mtok.unwrap_or(0.0),
        ]
    }
}

/// What a call takes beyond the text ACR sends it and reads back: the files
/// its tools read, the turns it takes, the thinking nobody sees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerCall {
    #[serde(default)]
    pub input_tokens: u64,
    /// Input read from a cache, as an agent's later turns mostly are.
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// What estimates are made from. A repository's own agent knows its calls
/// best, so its config is where these usually come from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Costs {
    /// By model, or by binding name for a binding that names no model.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prices: BTreeMap<String, Price>,
    /// By role, added to every call it makes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_call: BTreeMap<Role, PerCall>,
}

impl Costs {
    fn over(mut self, later: Costs) -> Costs {
        self.prices.extend(later.prices);
        self.per_call.extend(later.per_call);
        self
    }
}

impl Policy {
    fn over(mut self, later: Policy) -> Policy {
        self.version = later.version.or(self.version);
        self.tiers = later.tiers.or(self.tiers);
        self.lenses = later.lenses.or(self.lenses);
        self.not_findings = later.not_findings.or(self.not_findings);
        self.human_gate = later.human_gate.or(self.human_gate);
        self.max_rounds = later.max_rounds.or(self.max_rounds);
        self.reattack = later.reattack.or(self.reattack);
        self.answers = later.answers.or(self.answers);
        self.always_escalate = later.always_escalate.or(self.always_escalate);
        self.review_scope = later.review_scope.or(self.review_scope);
        self.integrations = later.integrations.or(self.integrations);
        self.generated = later.generated.or(self.generated);
        self.bindings.extend(later.bindings);
        self.costs = match (self.costs, later.costs) {
            (Some(costs), Some(later)) => Some(costs.over(later)),
            (costs, later) => later.or(costs),
        };
        self.stages = later.stages.or(self.stages);
        self.tests = later.tests.or(self.tests);
        self.runbook = later.runbook.or(self.runbook);
        self.allowed_models = later.allowed_models.or(self.allowed_models);
        self.facts = match (self.facts, later.facts) {
            (Some(facts), Some(later)) => Some(facts.over(later)),
            (facts, later) => later.or(facts),
        };
        self.savings = match (self.savings, later.savings) {
            (Some(savings), Some(later)) => Some(savings.over(later)),
            (savings, later) => later.or(savings),
        };
        self.sessions = match (self.sessions, later.sessions) {
            (Some(sessions), Some(later)) => Some(sessions.over(later)),
            (sessions, later) => later.or(sessions),
        };
        self
    }
}

/// A way to reach a model, which only the user's config can define.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    /// The bridge agent, as `name@machine`.
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How hard it thinks, such as `high`, if its bridge allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The tools a call may use. Roles that only read must name these, and
    /// one that writes should too if its agent refuses any tool it isn't
    /// named, as some headless agents do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// The most seconds a call may run, as a cap for one that never ends.
    /// Left out, the bridge's own longest. It's not a budget: a call is
    /// never cut off while it works, and one that stalls is ended by its
    /// bridge's idle check, so a timeout set short only loses work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// It may apply an Orchestrator's fixes, in the repository under review.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub writes: bool,
    /// US dollars per million tokens, for estimates. Set both, and they
    /// win over a repository's prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
    /// Its calls come out of a subscription, so they count in tokens and
    /// never in dollars, whatever a price or the agent says.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subscription: bool,
    /// No longer used: read so that a config from before still loads. A
    /// call that ran out of time was tried once more on it, from scratch,
    /// which lost the work done and cut off calls that were still working.
    #[serde(default, skip_serializing)]
    pub fallback: Option<String>,
    /// The host's MCP servers its calls may use, by name, where its bridge
    /// lets a request name them (pi's, given `options.mcp`). Left out, a
    /// call gets none: a bridge whose agent can turn MCP off does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<Vec<String>>,
}

/// A binding's `timeout` under this is said to be one, by `time_notes`.
const SHORT_CAP: u64 = 3600;

/// The tools that can change files: a shell, and the writing ones.
const CHANGING_TOOLS: [&str; 3] = ["Bash", "Write", "Edit"];

impl Binding {
    /// The tools a call on it may reach for: its own, and each MCP server's
    /// it names, as `mcp__<server>__*`. None if it names no tools, when its
    /// bridge's are all it has.
    pub fn may_use(&self) -> Option<Vec<String>> {
        let mut all = self.tools.clone()?;
        for server in self.mcp.iter().flatten() {
            all.push(format!("mcp__{server}__*"));
        }
        Some(all)
    }

    /// The tools a call used that it doesn't name: `used`'s names that
    /// match none of `may_use`, a name ending in `*` covering every tool it
    /// starts, in any case. Empty when it names no tools.
    pub fn unnamed<'a>(&self, used: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        let Some(may) = self.may_use() else {
            return Vec::new();
        };
        let covered = |name: &str| {
            may.iter().any(|t| match t.strip_suffix('*') {
                Some(head) => name.to_lowercase().starts_with(&head.to_lowercase()),
                None => t.eq_ignore_ascii_case(name),
            })
        };
        used.into_iter().filter(|n| !covered(n)).cloned().collect()
    }

    /// Whether it names a tool that can run or change files, in whatever
    /// case its agent spells it, `Bash` or `bash`. A reviewer that has one
    /// works in a copy of its own, to run and break things in, and never in
    /// the checkout.
    pub fn changes_files(&self) -> bool {
        self.tools.as_ref().is_some_and(|t| {
            t.iter()
                .any(|t| CHANGING_TOOLS.iter().any(|c| c.eq_ignore_ascii_case(t)))
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Everything a repository can choose from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allowed_bindings: BTreeMap<String, Binding>,
    /// This user's defaults, which a repository overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Policy>,
}

/// A run's flags, the last layer.
/// Which lenses attack again after a fix: an intensity a review is asked
/// for at its start when its config doesn't say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reattack {
    /// The lenses that had blockers, and any whose round didn't finish or
    /// the fixed change now triggers. A lens that passed isn't attacked
    /// again, so a fix that breaks what it covers goes unseen until a
    /// sweep, if the review has one.
    Blocked,
    /// Every lens, after every fix: a fix for one lens can break another.
    /// Each round after a fix costs about as much as the first.
    All,
}

impl Reattack {
    pub fn as_str(self) -> &'static str {
        match self {
            Reattack::Blocked => "blocked",
            Reattack::All => "all",
        }
    }
}

/// What a run's flags make weaker than `config`, the repository's config
/// without them, each said: a gate that asks a person less, fewer rounds,
/// another binding for a role, fewer lenses attacking after a fix, or less
/// of the change in scope.
pub fn weakened(config: &Config, run: &Config) -> Vec<String> {
    let mut out = Vec::new();
    if config.human_gate.asks_more_than(run.human_gate) {
        out.push(format!(
            "--human-gate {}, where its config asks at more points ({})",
            run.human_gate.as_str(),
            config.human_gate.as_str()
        ));
    }
    if run.max_rounds < config.max_rounds {
        out.push(format!(
            "--max-rounds {}, under its config's {}",
            run.max_rounds, config.max_rounds
        ));
    }
    for (role, name) in &run.bindings {
        if config.bindings.get(role) != Some(name) {
            out.push(format!(
                "--binding {}={name}, in place of its config's {}",
                role.as_str(),
                config.bindings.get(role).map_or("default", String::as_str)
            ));
        }
    }
    if config.reattack == Some(Reattack::All) && run.reattack == Some(Reattack::Blocked) {
        out.push(format!(
            "--reattack {}, where its config re-attacks with every lens",
            Reattack::Blocked.as_str()
        ));
    }
    if config.review_scope == ReviewScope::WholePr && run.review_scope == ReviewScope::BottomOfStack
    {
        out.push(
            "--review-scope bottom-of-stack, where its config reviews the whole pull request"
                .into(),
        );
    }
    out
}

/// Who may answer a review's questions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Answers {
    /// A terminal, Grayboard, or `acr resume --answers`, such as an agent
    /// relaying its user's answers: the default.
    #[default]
    Anyone,
    /// A person only: a terminal's prompt, or a Grayboard reply from the
    /// owner's human handle. `--answers` and run flags that weaken the
    /// review are refused, unless confirmed at a terminal.
    Person,
}

#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub human_gate: Option<HumanGate>,
    pub max_rounds: Option<u32>,
    pub reattack: Option<Reattack>,
    pub review_scope: Option<ReviewScope>,
    pub bindings: BTreeMap<Role, String>,
}

/// The layers, resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub tiers: Vec<Tier>,
    pub lenses: Vec<Lens>,
    pub not_findings: Option<String>,
    pub human_gate: HumanGate,
    pub max_rounds: u32,
    /// Which lenses attack again after a fix; None until someone chooses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reattack: Option<Reattack>,
    /// Who may answer the review's questions.
    #[serde(default)]
    pub answers: Answers,
    pub always_escalate: Vec<String>,
    pub review_scope: ReviewScope,
    pub integrations: Integrations,
    #[serde(default)]
    pub generated: Generated,
    pub bindings: BTreeMap<Role, String>,
    pub allowed_bindings: BTreeMap<String, Binding>,
    #[serde(default)]
    pub costs: Costs,
    /// The repository's layer came from a file outside it, which nothing in
    /// the repository can change.
    #[serde(default)]
    pub outside: bool,
    #[serde(default)]
    pub stages: Vec<Stage>,
    /// Probes, on by default, are off: no stages were chosen, and the
    /// reviewers' binding can't run things.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probes_off: bool,
    #[serde(default)]
    pub tests: Tests,
    #[serde(default)]
    pub runbook: Option<String>,
    /// The models a review may use, as patterns: none means any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    /// How a round shares the change map with its roles. Older runs' stored
    /// configs have none, and get the defaults.
    #[serde(default)]
    pub facts: FactsConfig,
    /// How a review saves on its Judges and later rounds. Older runs'
    /// stored configs have none, and get the defaults.
    #[serde(default)]
    pub savings: SavingsConfig,
    /// How long calls are broken into sessions. Older runs' stored configs
    /// have none, and get the defaults: off.
    #[serde(default)]
    pub sessions: SessionsConfig,
}

/// The framework's own defaults. They name no binding, model or provider:
/// which model does each role is the user's to say. Probes are on: every
/// change says it does something, and running scenarios of that finds
/// what reading it misses.
fn framework_defaults() -> Policy {
    Policy {
        human_gate: Some(HumanGate::InTheLoop),
        max_rounds: Some(3),
        review_scope: Some(ReviewScope::WholePr),
        // Git's word on the changed files costs no call, and saves every
        // reviewer looking it up; probes, where reviewers can run things.
        stages: Some(vec![Stage::Context, Stage::Probes]),
        ..Policy::default()
    }
}

/// Why probes are off, for what shows a config.
pub const PROBES_OFF: &str = "Probes are off: they're on by default, but they run scenarios, and \
     the reviewers' binding has no Bash, Write or Edit. Give it those, since each reviewer works \
     in a copy of its own, or list `stages` in the repository's config to choose for yourself.";

/// Each binding names a model `allowed` allows, as patterns: none allowed
/// means any.
pub fn check_models(allowed: &[String], bindings: &[(&str, &Binding)]) -> Result<()> {
    if allowed.is_empty() {
        return Ok(());
    }
    let mut set = globset::GlobSetBuilder::new();
    for pattern in allowed {
        set.add(
            globset::GlobBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("allowed_models: bad pattern {pattern:?}"))?,
        );
    }
    let set = set.build()?;
    let list = allowed.join(", ");
    for (name, b) in bindings {
        match &b.model {
            Some(model) if set.is_match(model) => {}
            Some(model) => bail!(
                "binding {name} uses {model}, which this review's allowed_models ({list}) \
                 doesn't allow: bind its role to another, or allow it"
            ),
            None => bail!(
                "binding {name} names no model, so it can't be held to this review's \
                 allowed_models ({list}): give it its `model`"
            ),
        }
    }
    Ok(())
}

/// Where a role's binding can be set.
fn no_binding(role: Role) -> String {
    let role = role.as_str();
    format!(
        "no binding for the {role}: set `defaults.bindings.{role}` in your acr.yaml, \
         `bindings.{role}` in the repository's config, or `--binding {role}=<name>`"
    )
}

pub fn parse_repo(text: &str) -> Result<Policy> {
    serde_yaml_ng::from_str(text).context("in the repository's ACR config")
}

pub fn parse_user(text: &str) -> Result<UserConfig> {
    serde_yaml_ng::from_str(text).context("in your ACR config")
}

/// The user's config, if they have one.
pub fn load_user(path: &Path) -> Result<UserConfig> {
    match fs::read_to_string(path) {
        Ok(text) => parse_user(&text).with_context(|| path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(UserConfig::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Puts the layers together and checks the result.
pub fn resolve(user: &UserConfig, repo: Policy, flags: &Flags) -> Result<Config> {
    if repo.version.is_none() {
        bail!("the repository's config needs `version: {VERSION}`");
    }
    let flag_layer = Policy {
        human_gate: flags.human_gate,
        max_rounds: flags.max_rounds,
        reattack: flags.reattack,
        review_scope: flags.review_scope,
        bindings: flags.bindings.clone(),
        ..Policy::default()
    };
    // Stages someone chose, rather than the framework's.
    let chosen =
        repo.stages.is_some() || user.defaults.as_ref().is_some_and(|d| d.stages.is_some());
    let merged = framework_defaults()
        .over(user.defaults.clone().unwrap_or_default())
        .over(repo)
        .over(flag_layer);
    if merged.version != Some(VERSION) {
        bail!("this {APP_NAME} reads ACR config version {VERSION}");
    }
    let config = Config {
        tiers: merged.tiers.unwrap_or_default(),
        lenses: merged.lenses.unwrap_or_default(),
        not_findings: merged.not_findings,
        human_gate: merged.human_gate.expect("set by the framework"),
        max_rounds: merged.max_rounds.expect("set by the framework"),
        reattack: merged.reattack,
        answers: merged.answers.unwrap_or_default(),
        always_escalate: merged.always_escalate.unwrap_or_default(),
        review_scope: merged.review_scope.expect("set by the framework"),
        integrations: merged.integrations.unwrap_or_default(),
        generated: merged.generated.unwrap_or_default(),
        bindings: merged.bindings,
        allowed_bindings: user.allowed_bindings.clone(),
        costs: merged.costs.unwrap_or_default(),
        outside: false,
        stages: merged.stages.unwrap_or_default(),
        probes_off: false,
        tests: merged.tests.unwrap_or_default(),
        runbook: merged.runbook,
        allowed_models: merged.allowed_models.unwrap_or_default(),
        facts: FactsConfig::of(merged.facts),
        savings: SavingsConfig::of(merged.savings),
        sessions: SessionsConfig::of(merged.sessions),
    };
    config.check()?;
    config.check_models()?;
    config.first_pass_judge()?;
    config.fixer()?;
    let config = with_stage_lenses(with_universal_lenses(default_probes(config, chosen)))?;
    config.check_sessions()?;
    Ok(config)
}

/// Probes by default need reviewers that can run things: without that,
/// they're left out, rather than failing every round. Chosen, they stay,
/// and say why they can't run.
fn default_probes(mut config: Config, chosen: bool) -> Config {
    let runs = config
        .binding(Role::Reviewer, None)
        .is_ok_and(Binding::changes_files);
    if !chosen && !runs && config.stages.contains(&Stage::Probes) {
        config.stages.retain(|s| *s != Stage::Probes);
        config.probes_off = true;
    }
    config
}

/// The lens every repository gets, whatever its dimensions: elegance.
/// A repository can give its own `elegance` lens to word it for its code,
/// but not leave it out.
pub const ELEGANCE: &str = "elegance";
const ELEGANCE_CRITERIA: &str = "The change is as simple as the problem it solves allows: it \
     works at the right level, in the code's own idiom, with nothing it doesn't need: no \
     duplicated logic, dead code, or layer, option or wrapper nothing needs, and no special case \
     standing in for a missing general rule. Its names and structure say what it does, so a \
     reader can see it's right.";
const ELEGANCE_ATTACK: &str = "Is there a smaller change that does the same? Search the code \
     for what it re-implements. For each new branch, flag, parameter, wrapper or layer: what \
     needs it, and would the code be simpler without it? Is a special case covering for a rule \
     that should be general? Read it as a newcomer would: do its names and structure say what it \
     does without its comments? Compare it with how nearby code solves the same kind of problem.";
const ELEGANCE_BLOCKER: &str = "a shape that will cost the change's readers or its next change: \
     logic duplicated where it must stay in step, a special case that hides a missing general \
     rule, a layer, option or abstraction nothing needs, or a structure that hides whether the \
     change is right; not taste in naming or formatting.";

fn with_universal_lenses(mut config: Config) -> Config {
    if !config.lenses.iter().any(|l| l.id == ELEGANCE) {
        config.lenses.push(Lens {
            id: ELEGANCE.to_string(),
            criteria: ELEGANCE_CRITERIA.to_string(),
            attack: Some(ELEGANCE_ATTACK.to_string()),
            blocker: Some(ELEGANCE_BLOCKER.to_string()),
            triggers: Triggers {
                paths: vec!["**".into()],
                ..Triggers::default()
            },
            binding: None,
            severity: BTreeMap::new(),
            include_generated: false,
        });
    }
    config
}

/// The config with a lens for each stage that runs as one. They apply to
/// every change, and a Judge rates what they find, as any lens's.
fn with_stage_lenses(mut config: Config) -> Result<Config> {
    let gated = config.stages.contains(&Stage::Gate);
    for stage in config.stages.clone() {
        let Some(id) = super::stages::lens_id(stage) else {
            continue;
        };
        // The gate runs the mutants itself.
        if gated && stage == Stage::Mutation {
            continue;
        }
        if config.lenses.iter().any(|l| l.id == id) {
            bail!("a lens is named {id}, which the {id} stage's own lens is called");
        }
        config.lenses.push(Lens {
            id: id.to_string(),
            criteria: super::stages::criteria(stage).to_string(),
            triggers: Triggers {
                paths: vec!["**".into()],
                ..Triggers::default()
            },
            attack: None,
            blocker: None,
            binding: None,
            severity: BTreeMap::new(),
            include_generated: false,
        });
    }
    Ok(config)
}

fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_globs(paths: &[String], whose: &str) -> Result<()> {
    for path in paths {
        Glob::new(path).with_context(|| format!("{whose}: bad path pattern {path:?}"))?;
    }
    Ok(())
}

impl Config {
    fn check(&self) -> Result<()> {
        if self.tiers.is_empty() {
            bail!("the config needs at least one tier");
        }
        if self.lenses.is_empty() {
            bail!("the config needs at least one lens");
        }
        if self.max_rounds == 0 {
            bail!("max_rounds must be at least 1");
        }
        let mut seen = std::collections::HashSet::new();
        for lens in &self.lenses {
            let whose = format!("lens {}", lens.id);
            if !plain_id(&lens.id) {
                bail!(
                    "{:?} can't be a lens's id: use lowercase letters, digits and hyphens",
                    lens.id
                );
            }
            if !seen.insert(lens.id.as_str()) {
                bail!("there are two lenses with the id {}", lens.id);
            }
            if lens.criteria.trim().is_empty() {
                bail!("{whose} has no criteria");
            }
            let t = &lens.triggers;
            if t.paths.is_empty() && t.content.is_empty() && t.description.is_none() {
                bail!("{whose} has no triggers");
            }
            check_globs(&t.paths, &whose)?;
            for pattern in &t.content {
                Regex::new(pattern).with_context(|| format!("{whose}: bad pattern {pattern:?}"))?;
            }
            // Results must mean the same in every repository.
            for severity in lens.severity.keys() {
                if matches!(
                    severity,
                    Severity::Critical | Severity::High | Severity::Rejected
                ) {
                    bail!("{whose}: what {severity:?} does can't be changed");
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for tier in &self.tiers {
            let whose = format!("tier {}", tier.id);
            if !plain_id(&tier.id) {
                bail!(
                    "{:?} can't be a tier's id: use lowercase letters, digits and hyphens",
                    tier.id
                );
            }
            if !seen.insert(tier.id.as_str()) {
                bail!("there are two tiers with the id {}", tier.id);
            }
            if tier.rule.paths.is_empty() && tier.rule.description.is_none() {
                bail!("{whose} has nothing to match");
            }
            if tier.fan_out == 0 {
                bail!("{whose}: fan_out must be at least 1");
            }
            check_globs(&tier.rule.paths, &whose)?;
            if let Some(id) = tier.required.iter().find(|r| !self.has_lens(r)) {
                bail!("{whose} requires lens {id}, which doesn't exist");
            }
        }
        check_globs(&self.generated.paths, "generated.paths")?;
        check_globs(&self.generated.keep, "generated.keep")?;
        if let Some(labels) = self.integrations.labels.as_ref() {
            for (key, value) in [
                ("waiting", &labels.waiting),
                ("confirmed", &labels.confirmed),
            ] {
                if value.as_ref().is_some_and(|v| !v.contains("{lens}")) {
                    bail!("integrations.labels.{key} needs {{lens}}");
                }
            }
        }
        for (key, price) in &self.costs.prices {
            if price.rates().iter().any(|r| !r.is_finite() || *r < 0.0) {
                bail!("costs.prices.{key}: a price can't be negative");
            }
        }
        if !self.allowed_bindings.is_empty() {
            self.check_bindings()?;
        }
        Ok(())
    }

    /// Relays only for the roles that read at length, past a size that
    /// leaves a session room to work; scouting only for lenses that exist,
    /// the framework's own included.
    fn check_sessions(&self) -> Result<()> {
        for (role, tokens) in &self.sessions.relay_tokens {
            if !matches!(role, Role::Reviewer | Role::Judge | Role::Orchestrator) {
                bail!(
                    "sessions.relay_tokens.{}: only reviewers', Judges' and the Orchestrator's \
                     fix calls relay",
                    role.as_str()
                );
            }
            if *tokens < RELAY_MIN_TOKENS {
                bail!(
                    "sessions.relay_tokens.{}: {tokens} is too few to work in; it must be at \
                     least {RELAY_MIN_TOKENS}",
                    role.as_str()
                );
            }
        }
        for id in &self.sessions.scout {
            if !self.lenses.iter().any(|l| &l.id == id) {
                bail!("sessions.scout names lens {id}, which doesn't exist");
            }
        }
        Ok(())
    }

    /// Whether every binding in use is one the user allows.
    pub fn check_bindings(&self) -> Result<()> {
        if let Some(role) = Role::ALL
            .into_iter()
            .find(|r| !self.bindings.contains_key(r))
        {
            bail!("{}", no_binding(role));
        }
        let chosen = self
            .bindings
            .values()
            .chain(self.lenses.iter().filter_map(|l| l.binding.as_ref()));
        for name in chosen {
            if !self.allowed_bindings.contains_key(name) {
                bail!("binding {name} isn't in your allowed_bindings");
            }
        }
        for role in Role::ALL.into_iter().filter(|r| r.read_only()) {
            let binding = self.binding(role, None)?;
            if binding.tools.as_ref().is_none_or(Vec::is_empty) {
                bail!(
                    "the {} binding must name its read-only tools, in `tools`",
                    role.as_str()
                );
            }
        }
        for lens in self.lenses.iter().filter(|l| l.binding.is_some()) {
            let binding = self.binding(Role::Reviewer, Some(lens))?;
            if binding.tools.as_ref().is_none_or(Vec::is_empty) {
                bail!("lens {}'s binding must name its read-only tools", lens.id);
            }
        }
        // A misspelt model would only show up as a missing price.
        for key in self.costs.prices.keys() {
            let used = self
                .allowed_bindings
                .iter()
                .any(|(name, b)| name == key || b.model.as_ref() == Some(key));
            if !used {
                bail!("costs.prices names {key}, which no binding in your allowed_bindings uses");
            }
        }
        Ok(())
    }

    /// Each binding a role or lens names, once, with its name: every
    /// binding a review could call.
    pub fn chosen_bindings(&self) -> Vec<(&str, &Binding)> {
        let mut out: Vec<(&str, &Binding)> = Vec::new();
        let names = self
            .bindings
            .values()
            .chain(self.lenses.iter().filter_map(|l| l.binding.as_ref()))
            .chain(self.savings.first_pass_judge.iter())
            .chain(self.savings.fixer.iter().map(|f| &f.binding));
        for name in names {
            if let Some(b) = self.allowed_bindings.get(name)
                && !out.iter().any(|(n, _)| *n == name.as_str())
            {
                out.push((name, b));
            }
        }
        out
    }

    /// What a review's bindings set that would cut off its calls, said so
    /// that a person can decide: a time budget, or a fallback from before.
    pub fn time_notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        // An easy group's blockers are at or below `max_severity`; if no
        // lens blocks at any severity that low, no group can be easy.
        if let Some(f) = &self.savings.fixer {
            let can = self.lenses.iter().any(|l| {
                [
                    Severity::Critical,
                    Severity::High,
                    Severity::Medium,
                    Severity::Low,
                ]
                .into_iter()
                .filter(|s| *s >= f.max_severity)
                .any(|s| {
                    matches!(
                        self.action(l, s),
                        Action::Blocks | Action::BlocksUnlessWaived
                    )
                })
            });
            if !can {
                out.push(format!(
                    "savings.fixer routes no fix to {}: its max_severity, {:?}, is below every \
                     severity that blocks here, so every group is hard. Raise max_severity, or \
                     have a severity that low block in a lens",
                    f.binding, f.max_severity
                ));
            }
        }
        for (name, b) in self.chosen_bindings() {
            if b.fallback.is_some() {
                out.push(format!(
                    "Binding {name} names a fallback, which {APP_NAME} no longer uses: a call \
                     isn't cut off while it works, and its bridge ends one that stalls. You can \
                     remove it."
                ));
            }
            // A cap of hours is the safety net it should be. One shorter
            // than that is worth a person's look: it's what cut off
            // reviewers that were still working.
            if let Some(secs) = b.timeout.filter(|s| *s < SHORT_CAP) {
                out.push(format!(
                    "Binding {name} ends its calls after {secs} s, even while they work, and \
                     a reviewer on a large change can need longer. Leave `timeout` out, or \
                     make it a cap no call should reach: its bridge's idle check ends a call \
                     that stalls."
                ));
            }
        }
        out
    }

    /// Every binding a review could call names a model `allowed_models`
    /// allows: the repository's, or the user's for every repository.
    fn check_models(&self) -> Result<()> {
        check_models(&self.allowed_models, &self.chosen_bindings())
    }

    /// ACR runs on one machine: every binding in use must reach an agent on
    /// `machine`, where the code is, so its reviewers read the code itself.
    pub fn check_local(&self, machine: &str) -> Result<()> {
        for (name, binding) in self.chosen_bindings() {
            let on = binding
                .agent
                .split_once('@')
                .map(|(_, m)| m.split('.').next().unwrap_or(m));
            if on.is_some_and(|m| !m.eq_ignore_ascii_case(machine)) {
                bail!(
                    "ACR runs on one machine: binding {name} reaches {}, but the code is on \
                     {machine}. Bind it to an agent on {machine}.",
                    binding.agent
                );
            }
        }
        Ok(())
    }

    /// A binding's prices: its own, if it sets both, or else the
    /// repository's for its name or its model.
    pub fn price(&self, name: &str, binding: &Binding) -> Option<Price> {
        if let (Some(input_per_mtok), Some(output_per_mtok)) =
            (binding.input_per_mtok, binding.output_per_mtok)
        {
            return Some(Price {
                input_per_mtok,
                output_per_mtok,
                cached_input_per_mtok: binding.cached_input_per_mtok,
                cache_write_per_mtok: binding.cache_write_per_mtok,
            });
        }
        let model = binding
            .model
            .as_ref()
            .and_then(|m| self.costs.prices.get(m));
        self.costs.prices.get(name).or(model).copied()
    }

    /// What every call by a role is taken to add.
    pub fn per_call(&self, role: Role) -> PerCall {
        self.costs.per_call.get(&role).copied().unwrap_or_default()
    }

    fn has_lens(&self, id: &str) -> bool {
        self.lenses.iter().any(|l| l.id == id)
    }

    /// The name of the binding for a role, or for a reviewer of `lens`.
    pub fn binding_name<'a>(&'a self, role: Role, lens: Option<&'a Lens>) -> Result<&'a str> {
        let name = lens
            .and_then(|l| l.binding.as_ref())
            .or_else(|| self.bindings.get(&role));
        name.map(String::as_str).with_context(|| no_binding(role))
    }

    pub fn binding<'a>(&'a self, role: Role, lens: Option<&'a Lens>) -> Result<&'a Binding> {
        let name = self.binding_name(role, lens)?;
        self.allowed_bindings
            .get(name)
            .with_context(|| format!("binding {name} isn't in your allowed_bindings"))
    }

    /// The first-pass Judge's binding, by name, if the savings set one.
    pub fn first_pass_judge(&self) -> Result<Option<(&str, &Binding)>> {
        let Some(name) = self.savings.first_pass_judge.as_deref() else {
            return Ok(None);
        };
        let binding = self.allowed_bindings.get(name).with_context(|| {
            format!("savings.first_pass_judge: binding {name} isn't in your allowed_bindings")
        })?;
        Ok(Some((name, binding)))
    }

    /// `savings.fixer`, if set, with its binding, which must be one in
    /// `allowed_bindings` that may write.
    pub fn fixer(&self) -> Result<Option<(&FixerConfig, &Binding)>> {
        let Some(f) = self.savings.fixer.as_ref() else {
            return Ok(None);
        };
        if f.max_severity == Severity::Critical {
            bail!(
                "savings.fixer: max_severity can't be Critical: a Critical blocker always goes to \
                 the Orchestrator's binding"
            );
        }
        let name = &f.binding;
        let binding = self.allowed_bindings.get(name).with_context(|| {
            format!("savings.fixer: binding {name} isn't in your allowed_bindings")
        })?;
        if !binding.writes {
            bail!(
                "savings.fixer: binding {name} can't apply fixes: give it `writes: true` in your \
                 acr.yaml"
            );
        }
        Ok(Some((f, binding)))
    }

    /// What a severity does in a lens.
    pub fn action(&self, lens: &Lens, severity: Severity) -> Action {
        lens.severity
            .get(&severity)
            .copied()
            .unwrap_or(severity.default_action())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const GO: &str = include_str!("../../examples/acr/go.yaml");
    const RUST: &str = include_str!("../../examples/acr/rust.yaml");
    const USER: &str = include_str!("../../examples/acr/user.yaml");

    fn user() -> UserConfig {
        parse_user(USER).unwrap()
    }

    fn resolved(repo: &str) -> Result<Config> {
        resolve(&user(), parse_repo(repo)?, &Flags::default())
    }

    #[test]
    fn every_repository_gets_elegance_worded_its_own_way_if_it_likes() {
        // Built in, with a blocker of its own, for every change.
        let cfg = resolved(GO).unwrap();
        let lens = cfg.lenses.iter().find(|l| l.id == ELEGANCE).unwrap();
        assert!(lens.blocker.is_some() && lens.attack.is_some());
        assert_eq!(lens.triggers.paths, ["**"]);
        // A repository's own elegance lens is used in its place, not beside
        // it.
        let own = GO.replacen(
            "lenses:\n",
            "lenses:\n  - id: elegance\n    criteria: \"our own words\"\n    triggers: {paths: [\"**/*.go\"]}\n",
            1,
        );
        let cfg = resolved(&own).unwrap();
        let ours: Vec<&Lens> = cfg.lenses.iter().filter(|l| l.id == ELEGANCE).collect();
        assert_eq!(ours.len(), 1);
        assert_eq!(ours[0].criteria, "our own words");
    }

    #[test]
    fn the_go_example_is_valid() {
        let cfg = resolved(GO).unwrap();
        assert_eq!(cfg.human_gate, HumanGate::InTheLoop);
        // A pull request merges whole, so it's reviewed whole.
        assert_eq!(cfg.review_scope, ReviewScope::WholePr);
        // Its five, and elegance, which every repository gets.
        assert_eq!(cfg.lenses.len(), 6);
        assert!(cfg.lenses.iter().any(|l| l.id == ELEGANCE));
        assert_eq!(cfg.tiers.last().unwrap().fan_out, 3);
        assert_eq!(cfg.not_findings.as_deref(), Some("not-findings.md"));
    }

    #[test]
    fn a_repository_in_another_language_needs_no_framework_change() {
        let cfg = resolved(RUST).unwrap();
        assert!(
            cfg.lenses
                .iter()
                .any(|l| l.triggers.paths.iter().any(|p| p.ends_with(".rs")))
        );
    }

    #[test]
    fn the_framework_knows_nothing_about_any_repository() {
        for source in [
            include_str!("config.rs"),
            include_str!("change.rs"),
            include_str!("flow.rs"),
            include_str!("mod.rs"),
            include_str!("plan.rs"),
            include_str!("report.rs"),
            include_str!("roles.rs"),
            include_str!("stages.rs"),
        ] {
            // Its own source, read to fingerprint the rules, isn't
            // knowledge of a repository.
            let code: String = source
                .split("#[cfg(test)]")
                .next()
                .unwrap()
                .lines()
                .filter(|l| !l.contains("include_str!("))
                .collect::<Vec<_>>()
                .join("\n");
            // Words from the two examples' configs.
            for word in [
                "goroutine-safety",
                "http-compatibility",
                "internal/store",
                "lock-discipline",
                "wire-compatibility",
                ".go\"",
                ".rs\"",
            ] {
                assert!(!code.contains(word), "{word} in the framework");
            }
            // Nor any model or provider: those are the user's to choose.
            let lower = code.to_lowercase();
            for word in [
                "glm",
                "opus",
                "sonnet",
                "haiku",
                "fable",
                "claude",
                "gpt",
                "gemini",
                "grok",
                "fireworks",
                "anthropic",
                "openai",
            ] {
                assert!(!lower.contains(word), "{word} in the framework");
            }
        }
    }

    #[test]
    fn layers_override_in_order() {
        let mut user = user();
        let mut defaults = user.defaults.clone().unwrap();
        defaults.max_rounds = Some(9);
        defaults.review_scope = Some(ReviewScope::BottomOfStack);
        defaults.human_gate = Some(HumanGate::Ends);
        user.defaults = Some(defaults);
        let repo = "version: 1\nhuman_gate: per-round\nmax_rounds: 2\n\
                    tiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                    lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n";
        let flags = Flags {
            max_rounds: Some(1),
            ..Flags::default()
        };
        let cfg = resolve(&user, parse_repo(repo).unwrap(), &flags).unwrap();
        assert_eq!(cfg.max_rounds, 1, "the flag beats the repository");
        assert_eq!(
            cfg.human_gate,
            HumanGate::PerRound,
            "the repository beats the user"
        );
        assert_eq!(
            cfg.review_scope,
            ReviewScope::BottomOfStack,
            "the user beats the default"
        );
        assert_eq!(
            cfg.bindings[&Role::Judge],
            "large",
            "the user's defaults fill the rest"
        );
    }

    #[test]
    fn an_unknown_key_is_an_error_that_names_it() {
        for (text, key) in [
            ("version: 1\nhuman_gates: ends\n", "human_gates"),
            (
                "version: 1\nlenses: [{id: a, criteria: c, triggers: {}, nope: 1}]\n",
                "nope",
            ),
            (
                "version: 1\ntiers: [{id: a, match: {paths: []}, fan_out: 1, x: 1}]\n",
                "x",
            ),
            ("version: 1\nintegrations: {labels: {nope: x}}\n", "nope"),
            (
                "version: 1\ncosts: {per_call: {reviewer: {tokens: 1}}}\n",
                "tokens",
            ),
            (
                "version: 1\ncosts: {prices: {m: {input_per_mtok: 1, output_per_mtok: 1, agent: a@b}}}\n",
                "agent",
            ),
        ] {
            let err = format!("{:#}", parse_repo(text).unwrap_err());
            assert!(err.contains(key), "{err}");
        }
    }

    #[test]
    fn a_repository_cant_add_secrets_tools_commands_or_bindings() {
        for key in [
            "secrets: {T: /x}",
            "tools: [bash]",
            "command: /bin/sh",
            "commands: [rm]",
            "env: {A: b}",
            "allowed_bindings: {mine: {agent: a@b}}",
        ] {
            let text = format!("version: 1\n{key}\n");
            assert!(parse_repo(&text).is_err(), "{key}");
        }
        assert!(parse_repo("version: 1\nbindings: {sizer: {agent: a@b}}\n").is_err());
    }

    const SMALL: &str = "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                         lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n";

    const MODELS_USER: &str = "version: 1\nallowed_bindings:\n\
        \x20 open: {agent: a@m, model: vendor-a/big, tools: [read]}\n\
        \x20 local: {agent: a@m, model: vendor-b/fast, tools: [read], fallback: lean}\n\
        \x20 lean: {agent: a@m, model: vendor-b/lean, tools: [read]}\n\
        \x20 bare: {agent: a@m, tools: [read]}\n\
        defaults:\n  bindings: {sizer: local, reviewer: local, orchestrator: local, judge: local}\n";

    #[test]
    fn a_review_uses_only_the_models_its_repository_allows() {
        let user = parse_user(MODELS_USER).unwrap();
        let with = |extra: &str| {
            let repo = parse_repo(&format!("{SMALL}{extra}")).unwrap();
            resolve(&user, repo, &Flags::default())
        };
        let refused = |extra: &str| format!("{:#}", with(extra).unwrap_err());
        // Every binding within what's allowed, in any case.
        with("allowed_models: ['vendor-b/*']\n").unwrap();
        with("allowed_models: ['VENDOR-B/*']\n").unwrap();
        // A fallback from before is never called, so it isn't held to it.
        with("allowed_models: ['vendor-b/fast']\n").unwrap();
        // A role bound outside it is refused, or to a binding with no model.
        let err = refused("allowed_models: ['vendor-b/*']\nbindings: {judge: open}\n");
        assert!(err.contains("binding open uses vendor-a/big"), "{err}");
        let err = refused("allowed_models: ['vendor-b/*']\nbindings: {judge: bare}\n");
        assert!(err.contains("binding bare names no model"), "{err}");
        // Set by the user, it holds every repository that doesn't set its own.
        let mut held = user.clone();
        held.defaults.as_mut().unwrap().allowed_models = Some(vec!["vendor-a/*".into()]);
        let err = resolve(&held, parse_repo(SMALL).unwrap(), &Flags::default()).unwrap_err();
        assert!(format!("{err:#}").contains("binding local uses vendor-b/fast"));
        let own = parse_repo(&format!("{SMALL}allowed_models: ['vendor-b/*']\n")).unwrap();
        resolve(&held, own, &Flags::default()).unwrap();
        // Left out everywhere, any model goes.
        with("bindings: {judge: open}\n").unwrap();
    }

    #[test]
    fn a_config_with_time_budgets_still_loads_and_says_what_they_do() {
        let check = |text: &str| {
            let user = parse_user(text).unwrap();
            resolve(&user, parse_repo(SMALL).unwrap(), &Flags::default())
                .and_then(|cfg| cfg.check_bindings().map(|_| cfg))
        };
        // A fallback is read, even one to nowhere, and never checked or kept.
        let cfg = check(&MODELS_USER.replace("fallback: lean", "fallback: gone")).unwrap();
        let notes = cfg.time_notes();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("Binding local names a fallback")),
            "{notes:?}"
        );
        let stored = serde_json::to_string(&cfg).unwrap();
        assert!(!stored.contains("fallback"), "{stored}");
        // A timeout still caps a call, and is said to.
        let cfg = check(&MODELS_USER.replace("fallback: lean", "timeout: 1500")).unwrap();
        let notes = cfg.time_notes();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("Binding local ends its calls after 1500 s")),
            "{notes:?}"
        );
        assert_eq!(cfg.allowed_bindings["local"].timeout, Some(1500));
        // A cap of hours is the safety net it should be, and goes unsaid.
        let cfg = check(&MODELS_USER.replace("fallback: lean", "timeout: 14400")).unwrap();
        assert!(cfg.time_notes().is_empty(), "{:?}", cfg.time_notes());
        // With neither, there's nothing to say.
        let plain = MODELS_USER.replace(", fallback: lean", "");
        assert!(check(&plain).unwrap().time_notes().is_empty());
    }

    #[test]
    fn probes_are_on_by_default_wherever_reviewers_can_run_things() {
        let probes = |cfg: &Config| cfg.stages.contains(&Stage::Probes) && cfg.has_lens("probes");
        let mut runs = user();
        runs.allowed_bindings.get_mut("medium").unwrap().tools =
            Some(vec!["read".into(), "bash".into(), "write".into()]);
        let read_only = user();
        let chose = |stages: &str| format!("{SMALL}stages: {stages}\n");
        // Nobody chose: on, if the reviewers can run them.
        let cfg = resolve(&runs, parse_repo(SMALL).unwrap(), &Flags::default()).unwrap();
        assert!(probes(&cfg) && !cfg.probes_off);
        // Read-only reviewers: off, and saying so, not failing every round.
        let cfg = resolve(&read_only, parse_repo(SMALL).unwrap(), &Flags::default()).unwrap();
        assert!(!probes(&cfg) && cfg.probes_off);
        // Git's context costs no call, so it's on for both.
        assert!(cfg.stages.contains(&Stage::Context));
        // A repository that chose stages gets what it chose.
        let cfg = resolve(
            &runs,
            parse_repo(&chose("[gate]")).unwrap(),
            &Flags::default(),
        );
        let cfg = cfg.unwrap();
        assert!(!probes(&cfg) && !cfg.probes_off);
        let cfg = resolve(
            &read_only,
            parse_repo(&chose("[probes]")).unwrap(),
            &Flags::default(),
        );
        assert!(
            probes(&cfg.unwrap()),
            "chosen, it stays, and says it needs a shell"
        );
        // So does a user's own default.
        let mut theirs = read_only.clone();
        theirs.defaults.as_mut().unwrap().stages = Some(Vec::new());
        let cfg = resolve(&theirs, parse_repo(SMALL).unwrap(), &Flags::default()).unwrap();
        assert!(!probes(&cfg) && !cfg.probes_off);
    }

    #[test]
    fn costs_merge_model_by_model_and_role_by_role() {
        let mut user = user();
        let theirs = "version: 1\ncosts: {per_call: {judge: {input_tokens: 7}}, prices: {\
                      example/large-model: {input_per_mtok: 1, output_per_mtok: 2}, \
                      example/small-model: {input_per_mtok: 3, output_per_mtok: 4}}}\n\
                      bindings: {sizer: small, reviewer: medium, orchestrator: large, judge: large}\n";
        user.defaults = Some(parse_repo(theirs).unwrap());
        let repo = [
            SMALL,
            "costs: {prices: {example/large-model: {input_per_mtok: 5, output_per_mtok: 25}}, \
             per_call: {reviewer: {input_tokens: 100, output_tokens: 10}}}\n",
        ]
        .concat();
        let cfg = resolve(&user, parse_repo(&repo).unwrap(), &Flags::default()).unwrap();
        let rate = |model: &str| cfg.costs.prices[model].input_per_mtok;
        assert_eq!(rate("example/large-model"), 5.0, "the repository's wins");
        assert_eq!(
            rate("example/small-model"),
            3.0,
            "the user's fills the rest"
        );
        let reviewer = PerCall {
            input_tokens: 100,
            output_tokens: 10,
            ..PerCall::default()
        };
        assert_eq!(cfg.per_call(Role::Reviewer), reviewer);
        assert_eq!(cfg.per_call(Role::Judge).input_tokens, 7);
        assert_eq!(cfg.per_call(Role::Sizer), PerCall::default());
    }

    #[test]
    fn a_binding_with_its_own_prices_keeps_them() {
        let repo = [
            SMALL,
            "costs: {prices: {example/large-model: {input_per_mtok: 5, output_per_mtok: 25}, \
             example/small-model: {input_per_mtok: 1, output_per_mtok: 1}, \
             example/medium-model: {input_per_mtok: 2, output_per_mtok: 2}, \
             small: {input_per_mtok: 9, output_per_mtok: 9}}}\n",
        ]
        .concat();
        let cfg = resolved(&repo).unwrap();
        let rate = |name: &str, b: &Binding| cfg.price(name, b).map(|p| p.input_per_mtok);
        let large = cfg.allowed_bindings["large"].clone();
        assert_eq!(rate("large", &large), Some(5.0), "by its model");
        let small = &cfg.allowed_bindings["small"];
        assert_eq!(rate("small", small), Some(9.0), "by its name first");
        let medium = &cfg.allowed_bindings["medium"];
        assert_eq!(rate("medium", medium), Some(2.0));
        // A binding's tools and its MCP servers' are what a call may use;
        // anything else its model reached for is said, in any case.
        let held = Binding {
            tools: Some(vec!["read".into(), "bash".into()]),
            mcp: Some(vec!["docs".into()]),
            ..large.clone()
        };
        let used: Vec<String> = [
            "read",
            "Bash",
            "mcp__docs__search",
            "sk_context-slack_query",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(held.unnamed(&used), ["sk_context-slack_query"]);
        let open = Binding {
            tools: None,
            ..large.clone()
        };
        assert!(open.unnamed(&used).is_empty(), "no list, nothing to say");
        let mine = Binding {
            input_per_mtok: Some(2.0),
            output_per_mtok: Some(3.0),
            ..large.clone()
        };
        let both = Price {
            input_per_mtok: 2.0,
            output_per_mtok: 3.0,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
        };
        assert_eq!(cfg.price("large", &mine), Some(both));
        let half = Binding {
            input_per_mtok: Some(2.0),
            ..large
        };
        assert_eq!(rate("large", &half), Some(5.0), "half a price isn't one");
    }

    #[test]
    fn a_price_must_be_for_a_binding_in_use_and_not_negative() {
        let typo = [
            SMALL,
            "costs: {prices: {opsu: {input_per_mtok: 5, output_per_mtok: 25}}}\n",
        ];
        let err = format!("{:#}", resolved(&typo.concat()).unwrap_err());
        assert!(err.contains("opsu"), "{err}");
        let negative = [
            SMALL,
            "costs: {prices: {example/large-model: {input_per_mtok: -1, output_per_mtok: 25}}}\n",
        ];
        let err = format!("{:#}", resolved(&negative.concat()).unwrap_err());
        assert!(err.contains("negative"), "{err}");
        let half = [
            SMALL,
            "costs: {prices: {example/large-model: {input_per_mtok: 5}}}\n",
        ];
        assert!(parse_repo(&half.concat()).is_err());
        let cache = [
            SMALL,
            "costs: {prices: {example/large-model: {input_per_mtok: 5, output_per_mtok: 25, cached_input_per_mtok: -1}}}\n",
        ];
        assert!(resolved(&cache.concat()).is_err());
    }

    #[test]
    fn cached_input_has_its_own_price_or_the_inputs() {
        let full = Price {
            input_per_mtok: 10.0,
            output_per_mtok: 20.0,
            cached_input_per_mtok: Some(1.0),
            cache_write_per_mtok: Some(12.5),
        };
        let m = 1_000_000;
        assert_eq!(full.usd(m, m, m, m), 10.0 + 1.0 + 12.5 + 20.0);
        let plain = Price {
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            ..full
        };
        assert_eq!(plain.usd(0, m, m, 0), 20.0, "at most what input costs");
    }

    #[test]
    fn a_repository_chooses_only_from_the_users_bindings() {
        let base = "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n";
        let lens = "lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n";
        let chosen = format!("{base}{lens}bindings: {{judge: small}}\n");
        assert_eq!(resolved(&chosen).unwrap().bindings[&Role::Judge], "small");
        let refused = format!("{base}{lens}bindings: {{judge: free-for-all}}\n");
        let err = format!("{:#}", resolved(&refused).unwrap_err());
        assert!(
            err.contains("free-for-all") && err.contains("allowed_bindings"),
            "{err}"
        );
        let lens_refused = format!(
            "{base}lenses: [{{id: l, criteria: c, triggers: {{paths: ['**']}}, binding: nope}}]\n"
        );
        assert!(resolved(&lens_refused).is_err());
    }

    #[test]
    fn roles_that_only_read_must_name_their_tools() {
        let mut user = user();
        user.allowed_bindings.get_mut("medium").unwrap().tools = None;
        let err = resolve(&user, parse_repo(GO).unwrap(), &Flags::default()).unwrap_err();
        assert!(format!("{err:#}").contains("read-only tools"), "{err:#}");
    }

    #[test]
    fn every_binding_must_be_on_the_machine_with_the_code() {
        let cfg = resolved(GO).unwrap();
        // The example's agents are on my-mac.
        assert!(cfg.check_local("my-mac").is_ok());
        assert!(cfg.check_local("MY-MAC").is_ok());
        let err = format!("{:#}", cfg.check_local("build-box").unwrap_err());
        assert!(
            err.contains("one machine") && err.contains("build-box"),
            "{err}"
        );
        // An agent named without a machine is this one's.
        let mut here = cfg.clone();
        for b in here.allowed_bindings.values_mut() {
            b.agent = "models".into();
        }
        assert!(here.check_local("build-box").is_ok());
    }

    #[test]
    fn the_framework_chooses_no_model() {
        let mut user = user();
        user.defaults.as_mut().unwrap().bindings.clear();
        let err = resolve(&user, parse_repo(GO).unwrap(), &Flags::default()).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("no binding for the") && err.contains("defaults.bindings"),
            "{err}"
        );
        // Any layer can say.
        let flags = Flags {
            bindings: Role::ALL
                .into_iter()
                .map(|r| (r, "small".to_string()))
                .collect(),
            ..Flags::default()
        };
        assert!(resolve(&user, parse_repo(GO).unwrap(), &flags).is_ok());
    }

    #[test]
    fn severity_means_the_same_everywhere() {
        assert_eq!(Severity::Critical.default_action(), Action::Blocks);
        assert_eq!(Severity::High.default_action(), Action::Blocks);
        assert_eq!(Severity::Medium.default_action(), Action::NeverBlocks);
        assert_eq!(Severity::Low.default_action(), Action::NeverBlocks);
        assert_eq!(Severity::Rejected.default_action(), Action::Drop);
        let text = |s: &str| {
            format!(
                "version: 1\ntiers: [{{id: a, match: {{paths: ['**']}}, fan_out: 1}}]\n\
                 lenses: [{{id: l, criteria: c, triggers: {{paths: ['**']}}, severity: {{{s}: never-blocks}}}}]\n"
            )
        };
        assert!(resolved(&text("High")).is_err());
        assert!(resolved(&text("Critical")).is_err());
        let cfg = resolved(&text("Medium")).unwrap();
        assert_eq!(
            cfg.action(&cfg.lenses[0], Severity::Medium),
            Action::NeverBlocks
        );
        assert_eq!(cfg.action(&cfg.lenses[0], Severity::High), Action::Blocks);
    }

    #[test]
    fn mistakes_are_named() {
        let base = "tiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                    lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n";
        let says = |text: String, word: &str| {
            let err = format!("{:#}", resolved(&text).unwrap_err());
            assert!(err.contains(word), "{err}");
        };
        says(base.to_string(), "version");
        says(format!("version: 2\n{base}"), "version");
        says(
            "version: 1\nlenses: [{id: l, criteria: c, triggers: {paths: ['a']}}]\n".into(),
            "tier",
        );
        says("version: 1\ntiers: [{id: a, match: {paths: ['a']}, fan_out: 0}]\nlenses: [{id: l, criteria: c, triggers: {paths: ['a']}}]\n".into(), "fan_out");
        says("version: 1\ntiers: [{id: a, match: {paths: ['a']}, fan_out: 1, required: [gone]}]\nlenses: [{id: l, criteria: c, triggers: {paths: ['a']}}]\n".into(), "gone");
        says("version: 1\ntiers: [{id: a, match: {paths: ['a']}, fan_out: 1}]\nlenses: [{id: l, criteria: c, triggers: {content: ['(']}}]\n".into(), "bad pattern");
        says("version: 1\ntiers: [{id: a, match: {paths: ['a']}, fan_out: 1}]\nlenses: [{id: 'Not Stable', criteria: c, triggers: {paths: ['a']}}]\n".into(), "id");
        says("version: 1\ntiers: [{id: a, match: {paths: ['a']}, fan_out: 1}]\nlenses: [{id: l, criteria: c, triggers: {paths: ['a']}}, {id: l, criteria: c, triggers: {paths: ['a']}}]\n".into(), "two lenses");
        says(
            format!("version: 1\n{base}integrations: {{labels: {{waiting: plain}}}}\n"),
            "{lens}",
        );
    }

    #[test]
    fn a_runs_stored_facts_settings_from_before_learning_still_read() {
        // As 0.1.21 to 0.1.24 stored them, without `learned`.
        let old = r#"{"shared": true, "warm_secs": 60, "keep_alive_secs": 240,
            "judges": "per-lens", "reviewers": "all", "match_tools": true}"#;
        let read: FactsConfig = serde_json::from_str(old).unwrap();
        assert_eq!(read, FactsConfig::default());
        assert!(read.learned && read.learned_bytes == 24 * 1024);
    }

    #[test]
    fn a_fixer_whose_max_severity_no_blocker_reaches_is_said_to_route_nothing() {
        let mut user = parse_user(USER).unwrap();
        user.allowed_bindings.get_mut("small").unwrap().writes = true;
        let repo = |extra: &str| {
            format!(
                "version: 1\ntiers: [{{id: a, match: {{paths: ['**']}}, fan_out: 1}}]\n\
                 lenses: [{{id: l, criteria: c, triggers: {{paths: ['**']}}{extra}}}]\n\
                 savings: {{fixer: {{binding: small, max_severity: Medium}}}}\n"
            )
        };
        let at_defaults =
            resolve(&user, parse_repo(&repo("")).unwrap(), &Flags::default()).unwrap();
        assert!(
            at_defaults
                .time_notes()
                .iter()
                .any(|n| n.contains("savings.fixer routes no fix to small")),
            "{:?}",
            at_defaults.time_notes()
        );
        // At the default, High, every High blocker can be easy.
        let high = format!(
            "version: 1\ntiers: [{{id: a, match: {{paths: ['**']}}, fan_out: 1}}]\n\
             lenses: [{{id: l, criteria: c, triggers: {{paths: ['**']}}}}]\n\
             savings: {{fixer: {{binding: small}}}}\n"
        );
        let by_default = resolve(&user, parse_repo(&high).unwrap(), &Flags::default()).unwrap();
        assert!(
            !by_default
                .time_notes()
                .iter()
                .any(|n| n.contains("routes no fix"))
        );
        let medium_blocks = resolve(
            &user,
            parse_repo(&repo(", severity: {Medium: blocks}")).unwrap(),
            &Flags::default(),
        )
        .unwrap();
        assert!(
            !medium_blocks
                .time_notes()
                .iter()
                .any(|n| n.contains("routes no fix"))
        );
    }

    #[test]
    fn what_a_runs_flags_make_weaker_than_its_config_is_said() {
        let user = parse_user(USER).unwrap();
        let repo = "version: 1\nhuman_gate: in-the-loop\nmax_rounds: 3\nreattack: all\n\
                    tiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                    lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n";
        let own = resolve(&user, parse_repo(repo).unwrap(), &Flags::default()).unwrap();
        assert!(weakened(&own, &own).is_empty(), "no flags, nothing weaker");
        let flags = Flags {
            human_gate: Some(HumanGate::Exception),
            max_rounds: Some(1),
            reattack: Some(Reattack::Blocked),
            bindings: BTreeMap::from([(Role::Judge, "small".to_string())]),
            ..Flags::default()
        };
        let run = resolve(&user, parse_repo(repo).unwrap(), &flags).unwrap();
        let said = weakened(&own, &run).join("; ");
        for part in [
            "--human-gate exception, where its config asks at more points (in-the-loop)",
            "--max-rounds 1, under its config's 3",
            "--reattack blocked, where its config re-attacks with every lens",
            "--binding judge=small",
        ] {
            assert!(said.contains(part), "{part}: {said}");
        }
        // A gate that asks more, or more rounds, isn't weaker.
        let more = Flags {
            max_rounds: Some(5),
            ..Flags::default()
        };
        let stronger = resolve(&user, parse_repo(repo).unwrap(), &more).unwrap();
        assert!(weakened(&own, &stronger).is_empty());
    }

    #[test]
    fn the_published_schema_covers_every_key() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let keys = |v: &serde_json::Value| -> Vec<String> {
            v["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let repo = serde_json::to_value(Policy {
            version: Some(1),
            tiers: Some(Vec::new()),
            lenses: Some(Vec::new()),
            not_findings: Some(String::new()),
            human_gate: Some(HumanGate::Ends),
            max_rounds: Some(1),
            reattack: Some(Reattack::All),
            answers: Some(Answers::Person),
            always_escalate: Some(Vec::new()),
            review_scope: Some(ReviewScope::WholePr),
            integrations: Some(Integrations::default()),
            generated: Some(Generated::default()),
            bindings: BTreeMap::from([(Role::Judge, "x".to_string())]),
            costs: Some(Costs::default()),
            stages: Some(Vec::new()),
            tests: Some(Tests::default()),
            runbook: Some(String::new()),
            allowed_models: Some(Vec::new()),
            facts: Some(FactsPolicy::default()),
            savings: Some(SavingsPolicy::default()),
            sessions: Some(SessionsPolicy::default()),
        })
        .unwrap();
        let mut ours: Vec<String> = repo.as_object().unwrap().keys().cloned().collect();
        ours.sort();
        let mut published = keys(&schema);
        published.sort();
        assert_eq!(ours, published);
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn sessions_are_off_unless_set_and_checked_when_they_are() {
        let user = parse_user(MODELS_USER).unwrap();
        let load = |extra: &str| {
            resolve(
                &user,
                parse_repo(&format!("{SMALL}{extra}"))?,
                &Flags::default(),
            )
        };
        // Off by default: no relay for any role, and no lens scouts.
        let off = load("").unwrap();
        assert_eq!(off.sessions, SessionsConfig::default());
        assert_eq!(off.sessions.relay(Role::Reviewer), None);
        assert!(!off.sessions.scouts("l"));
        let on = load("sessions: {relay_tokens: {reviewer: 120000, judge: 90000}, scout: [l]}\n")
            .unwrap();
        assert_eq!(on.sessions.relay(Role::Reviewer), Some(120_000));
        assert_eq!(on.sessions.relay(Role::Judge), Some(90_000));
        assert!(on.sessions.scouts("l"));
        // The framework's own lenses can scout too.
        assert!(
            load(&format!("sessions: {{scout: [{ELEGANCE}]}}\n"))
                .unwrap()
                .sessions
                .scouts(ELEGANCE)
        );
        for (extra, says) in [
            (
                "sessions: {relay_tokens: {sizer: 90000}}\n",
                "only reviewers', Judges' and the Orchestrator's",
            ),
            ("sessions: {relay_tokens: {reviewer: 1000}}\n", "too few"),
            ("sessions: {scout: [nope]}\n", "doesn't exist"),
        ] {
            let err = format!("{:#}", load(extra).unwrap_err());
            assert!(err.contains(says), "{extra}: {err}");
        }
        assert!(parse_repo(&format!("{SMALL}sessions: {{relay: 1}}\n")).is_err());
        // The user's defaults and the repository's merge, the later winning.
        let mut both = parse_user(MODELS_USER).unwrap();
        let mut defaults = both.defaults.take().unwrap_or_default();
        defaults.sessions = Some(SessionsPolicy {
            relay_tokens: BTreeMap::from([(Role::Reviewer, 100_000), (Role::Judge, 80_000)]),
            scout: None,
            stall_warn_secs: None,
        });
        both.defaults = Some(defaults);
        let merged = resolve(
            &both,
            parse_repo(&format!(
                "{SMALL}sessions: {{relay_tokens: {{judge: 60000}}, scout: [l]}}\n"
            ))
            .unwrap(),
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(merged.sessions.relay(Role::Reviewer), Some(100_000));
        assert_eq!(merged.sessions.relay(Role::Judge), Some(60_000));
        assert!(merged.sessions.scouts("l"));
    }

    #[test]
    fn fixes_group_and_relay_and_a_stall_is_warned_of_by_default_and_each_can_be_set() {
        let user = parse_user(MODELS_USER).unwrap();
        let load = |extra: &str| {
            resolve(
                &user,
                parse_repo(&format!("{SMALL}{extra}"))?,
                &Flags::default(),
            )
        };
        let plain = load("").unwrap();
        assert!(plain.savings.group_fixes, "grouping is on by default");
        assert_eq!(plain.sessions.stall_warn_secs, STALL_WARN_SECS);
        assert_eq!(plain.sessions.relay(Role::Orchestrator), None);
        let set = load(
            "savings: {group_fixes: false}\nsessions: {relay_tokens: {orchestrator: 90000}, \
             stall_warn_secs: 0}\n",
        )
        .unwrap();
        assert!(!set.savings.group_fixes);
        assert_eq!(set.sessions.relay(Role::Orchestrator), Some(90_000));
        assert_eq!(set.sessions.stall_warn_secs, 0, "0 turns the warning off");
        // A run's config stored before these were added still reads, with
        // each at its default.
        let stored: SavingsConfig = serde_json::from_str(
            r#"{"reuse_rulings": true, "focus_after_fix": false, "blast_radius": false}"#,
        )
        .unwrap();
        assert!(stored.group_fixes);
        let stored: SessionsConfig =
            serde_json::from_str(r#"{"relay_tokens": {}, "scout": []}"#).unwrap();
        assert_eq!(stored.stall_warn_secs, STALL_WARN_SECS);
    }
}
