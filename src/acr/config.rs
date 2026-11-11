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
    /// They decide what's folded in, each round.
    PerRound,
    /// They approve the scope and the final result.
    Ends,
    /// Only escalations.
    Exception,
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
    /// Seconds a call can take.
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
    /// Another binding, to try a call on once when this one runs out of
    /// time: its `timeout` is the role's budget, and past it the role goes
    /// on elsewhere, on a faster model say, rather than waiting on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// The tools that can change files: a shell, and the writing ones.
const CHANGING_TOOLS: [&str; 3] = ["Bash", "Write", "Edit"];

impl Binding {
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
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub human_gate: Option<HumanGate>,
    pub max_rounds: Option<u32>,
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
}

/// The framework's own defaults. They name no binding, model or provider:
/// which model does each role is the user's to say. Probes are on: every
/// change says it does something, and running scenarios of that finds
/// what reading it misses.
fn framework_defaults() -> Policy {
    Policy {
        human_gate: Some(HumanGate::Exception),
        max_rounds: Some(3),
        review_scope: Some(ReviewScope::WholePr),
        stages: Some(vec![Stage::Probes]),
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
    };
    config.check()?;
    config.check_models()?;
    with_stage_lenses(with_universal_lenses(default_probes(config, chosen)))
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
            let Some(b) = self.allowed_bindings.get(name) else {
                bail!("binding {name} isn't in your allowed_bindings");
            };
            match &b.fallback {
                Some(f) if f == name => bail!("binding {name} is its own fallback"),
                Some(f) if !self.allowed_bindings.contains_key(f) => {
                    bail!("binding {name}'s fallback, {f}, isn't in your allowed_bindings")
                }
                _ => {}
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

    /// Each binding a role or lens names, and each one's fallback, once,
    /// with its name: every binding a review could call.
    pub fn chosen_bindings(&self) -> Vec<(&str, &Binding)> {
        let mut out: Vec<(&str, &Binding)> = Vec::new();
        let names = self
            .bindings
            .values()
            .chain(self.lenses.iter().filter_map(|l| l.binding.as_ref()));
        for name in names {
            if let Some(b) = self.allowed_bindings.get(name)
                && !out.iter().any(|(n, _)| *n == name.as_str())
            {
                out.push((name, b));
            }
        }
        let fallbacks: Vec<&String> = out
            .iter()
            .filter_map(|(_, b)| b.fallback.as_ref())
            .collect();
        for name in fallbacks {
            if let Some(b) = self.allowed_bindings.get(name)
                && !out.iter().any(|(n, _)| *n == name.as_str())
            {
                out.push((name, b));
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
        assert_eq!(cfg.human_gate, HumanGate::PerRound);
        assert_eq!(cfg.review_scope, ReviewScope::BottomOfStack);
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
        // Every binding, fallbacks too, within what's allowed, in any case.
        with("allowed_models: ['vendor-b/*']\n").unwrap();
        with("allowed_models: ['VENDOR-B/*']\n").unwrap();
        // A fallback outside it is refused like any other binding.
        let err = refused("allowed_models: ['vendor-b/fast']\n");
        assert!(err.contains("binding lean uses vendor-b/lean"), "{err}");
        // So is a role bound outside it, or to a binding with no model.
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
    fn a_fallback_is_a_binding_and_not_itself() {
        let check = |text: &str| {
            let user = parse_user(text).unwrap();
            resolve(&user, parse_repo(SMALL).unwrap(), &Flags::default())
                .and_then(|cfg| cfg.check_bindings())
        };
        check(MODELS_USER).unwrap();
        let err = check(&MODELS_USER.replace("fallback: lean", "fallback: gone")).unwrap_err();
        assert!(format!("{err:#}").contains("fallback, gone, isn't in your allowed_bindings"));
        let err = check(&MODELS_USER.replace("fallback: lean", "fallback: local")).unwrap_err();
        assert!(format!("{err:#}").contains("binding local is its own fallback"));
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
        })
        .unwrap();
        let mut ours: Vec<String> = repo.as_object().unwrap().keys().cloned().collect();
        ours.sort();
        let mut published = keys(&schema);
        published.sort();
        assert_eq!(ours, published);
        assert_eq!(schema["additionalProperties"], false);
    }
}
