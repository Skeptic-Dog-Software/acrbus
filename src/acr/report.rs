//! What a review reports: machine-readable, with adapters for the labels and
//! comments a repository turns on.

use std::collections::BTreeMap;

use serde::Serialize;

use super::config::{CommentFormat, Config, Role, Severity};
use super::facts::Prefix;
use super::flow::{
    Disagreement, Escalation, FactsRound, Finding, FindingState, Flag, FlagState, Gate, LensResult,
    Outcome, Run, Status, Usage,
};

#[derive(Debug, Serialize)]
pub struct BindingUsed {
    pub role: Role,
    pub binding: String,
    pub agent: String,
    pub model: Option<String>,
    pub calls: u64,
}

#[derive(Debug, Serialize)]
pub struct Report<'a> {
    pub run_id: &'a str,
    pub status: Status,
    pub waiting_for: Option<Gate>,
    pub tier: &'a str,
    pub tier_why: &'a str,
    pub rounds: u32,
    pub lenses: Vec<&'a LensResult>,
    pub escalations: &'a [Escalation],
    pub disagreements: &'a [Disagreement],
    pub bindings_used: Vec<BindingUsed>,
    /// Every call's tokens: the agent's count where it gave one, and an
    /// estimate where it didn't.
    pub tokens: u64,
    pub calls_measured: usize,
    pub calls_estimated: usize,
    /// Calls that came out of a subscription, which count only in tokens.
    pub calls_on_subscription: usize,
    /// For the calls billed in dollars: by the config's prices, or an
    /// agent's where the config has none. None if one of those has no
    /// price, or none was billed in dollars.
    pub usd: Option<f64>,
    /// What the agents said, for the calls they priced.
    pub agent_usd: Option<f64>,
    pub labels: Vec<String>,
    /// Each round's calls by role, and the whole run's when it had more
    /// than one round.
    pub by_role: Vec<RoleUsage>,
    /// Each round's shared facts.
    pub facts: &'a [FactsRound],
    /// What each fan-out's calls, and each round with the last, share at the
    /// head of what they send.
    pub prefixes: &'a [Prefix],
    /// Whose answers the Sizer had to put in the form asked, and how often.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<Repairs>,
    /// The calls broken into sessions, by role: relayed and scouted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<Sessions>,
    /// Calls whose model asked for tools their binding doesn't name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unnamed_tools: Vec<UnnamedTools>,
    /// Which lenses attacked again after a fix, and whose choice it was.
    pub reattack: String,
    /// What the run's flags made weaker than its config.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    pub weakened: &'a [String],
    /// Where each gate's answers came from.
    #[serde(skip_serializing_if = "<[super::flow::AnswerSource]>::is_empty")]
    pub answered_by: &'a [super::flow::AnswerSource],
    /// The blockers a person deferred to a ticket, each with a draft to
    /// file. Nothing is filed for them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tickets: Vec<Ticket>,
    /// The files fixes changed that were flagged for a person: binaries,
    /// and files no blocker pointed at, each with what reverts it and what
    /// they chose.
    #[serde(skip_serializing_if = "<[Flag]>::is_empty")]
    pub flags: &'a [Flag],
}

/// A flagged file, as said at a review's end.
pub fn flag_line(f: &Flag) -> String {
    let state = match f.state {
        FlagState::Open => format!(
            "not answered yet, so it stays as the fix left it; to revert it: {}",
            f.command
        ),
        FlagState::Kept => "kept, as you chose".to_string(),
        FlagState::Reverting => format!("to be reverted, as you chose: {}", f.command),
        FlagState::Reverted => "reverted, as you chose".to_string(),
    };
    format!(
        "flagged: round {}'s fix changed {} ({}): {state}",
        f.round,
        f.path,
        f.why()
    )
}

/// One role's calls broken into sessions, in a run: those that relayed and
/// the sessions they took, and the scouts and the checks of what they
/// suspected. Each session is also a call of its own in `by_role`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sessions {
    pub role: Role,
    /// Calls that handed off at least once.
    pub relayed: usize,
    /// The sessions of every relayed call, scouts' and checks' included.
    pub legs: usize,
    /// Reviewers that scouted, and the suspicions they had checked.
    pub scouts: usize,
    pub verifies: usize,
    /// The groups a fix was split into, one session each before any relay.
    pub groups: usize,
    /// Every one of those sessions' tokens, and dollars where priced.
    pub tokens: u64,
    pub usd: Option<f64>,
}

impl Sessions {
    pub fn line(&self) -> String {
        let role = format!("{:?}", self.role).to_lowercase();
        let usd = self.usd.map_or_else(String::new, |d| format!(", ${d:.2}"));
        let groups = if self.groups > 0 {
            format!("; {} fix group(s)", self.groups)
        } else {
            String::new()
        };
        format!(
            "sessions: {role}: {} call(s) relayed, {} session(s) in all; {} scout(s), {} \
             suspicion(s) checked{groups}; {} tokens{usd}",
            self.relayed, self.legs, self.scouts, self.verifies, self.tokens
        )
    }
}

/// The run's calls broken into sessions, by role, from each call's part.
pub fn sessions(usage: &[Usage]) -> Vec<Sessions> {
    let mut by: BTreeMap<Role, Vec<&Usage>> = BTreeMap::new();
    for u in usage.iter().filter(|u| u.part.is_some()) {
        by.entry(u.role).or_default().push(u);
    }
    by.into_iter()
        .map(|(role, calls)| {
            let part = |u: &&Usage| u.part.clone().unwrap_or_default();
            let first_leg = |p: &str| p.ends_with("leg 1");
            let owned: Vec<Usage> = calls.iter().map(|u| (*u).clone()).collect();
            Sessions {
                role,
                relayed: calls.iter().filter(|u| first_leg(&part(u))).count(),
                legs: calls.iter().filter(|u| part(u).contains("leg ")).count(),
                scouts: calls
                    .iter()
                    .filter(|u| {
                        let p = part(u);
                        p == "scout" || p == "scout, leg 1"
                    })
                    .count(),
                verifies: calls
                    .iter()
                    .filter(|u| {
                        let p = part(u);
                        p.starts_with("verify") && (!p.contains(", leg ") || first_leg(&p))
                    })
                    .count(),
                groups: calls
                    .iter()
                    .filter(|u| {
                        let p = part(u);
                        p.starts_with("group ") && (!p.contains(", leg ") || first_leg(&p))
                    })
                    .count(),
                tokens: calls.iter().map(|u| u.tokens()).sum(),
                usd: super::flow::dollars(&owned),
            }
        })
        .collect()
}

/// Each session of a call broken up: its label, its part, and what it used.
pub fn session_lines(usage: &[Usage]) -> Vec<String> {
    usage
        .iter()
        .filter_map(|u| {
            let part = u.part.as_deref()?;
            let usd = u.usd.map_or_else(String::new, |d| format!(", ${d:.3}"));
            let secs = u
                .secs
                .map_or_else(String::new, |s| format!(", {}", span(s)));
            Some(format!(
                "  round {}: {} [{part}]: {} fresh, {} cached, {} out{usd}{secs}",
                u.round,
                u.label.as_deref().unwrap_or("a call"),
                u.tokens_in + u.cache_write,
                u.cache_read,
                u.tokens_out
            ))
        })
        .collect()
}

/// A draft ticket for a blocker deferred to one: a title and a body, ready
/// to file wherever the repository keeps its tickets.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ticket {
    pub finding: String,
    pub lens: String,
    pub round: u32,
    pub title: String,
    pub body: String,
}

/// The blockers deferred to a ticket in a run, each once, as its latest
/// round has it, with its draft.
pub fn tickets(run: &Run) -> Vec<Ticket> {
    let mut out: Vec<Ticket> = Vec::new();
    for lens in run.lenses.values() {
        for round in lens.rounds.iter().rev() {
            for f in &round.findings {
                if f.state != FindingState::Deferred || out.iter().any(|t| t.finding == f.id) {
                    continue;
                }
                out.push(ticket(run, &lens.id, round.round, f));
            }
        }
    }
    out.sort_by(|a, b| (a.round, &a.finding).cmp(&(b.round, &b.finding)));
    out
}

fn ticket(run: &Run, lens: &str, round: u32, f: &Finding) -> Ticket {
    let at = match (&f.file, f.line) {
        (Some(file), Some(line)) => format!("`{file}:{line}`"),
        (Some(file), None) => format!("`{file}`"),
        _ => "not tied to one place".to_string(),
    };
    let note = if f.note.trim().is_empty() {
        "None given.".to_string()
    } else {
        f.note.trim().to_string()
    };
    let found = if f.detail.trim().is_empty() {
        f.title.trim()
    } else {
        f.detail.trim()
    };
    // The Judge's reason, without what the person ruled, which the note
    // and the opening already say.
    let why = f
        .reason
        .replace("A person deferred it to a ticket.", "")
        .trim()
        .to_string();
    let mut body = format!(
        "Deferred from review {} (round {round}, lens {lens}): a {:?} blocker, real, and left \
         for a follow-up rather than fixed in that change.\n\n\
         **Where:** {at}\n\n\
         **What was found:**\n\n{found}\n\n",
        run.id, f.severity,
    );
    if !why.is_empty() {
        body.push_str(&format!("**Why it blocked:**\n\n{why}\n\n"));
    }
    body.push_str(&format!("**Note from the review:**\n\n{note}\n"));
    Ticket {
        finding: f.id.clone(),
        lens: lens.to_string(),
        round,
        title: f.title.trim().to_string(),
        body,
    }
}

/// How often one role's binding gave an answer the Sizer had to put in the
/// form asked, in a run: a model that misses the format often costs a call
/// each time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Repairs {
    pub role: Role,
    pub binding: String,
    pub model: Option<String>,
    /// Its answers the Sizer put in form.
    pub repairs: usize,
    /// Its calls in all, repairs not counted.
    pub calls: usize,
}

impl Repairs {
    pub fn share(&self) -> f64 {
        if self.calls == 0 {
            return 0.0;
        }
        self.repairs as f64 / self.calls as f64
    }

    /// One line of it, for a person.
    pub fn line(&self) -> String {
        let role = format!("{:?}", self.role).to_lowercase();
        let model = self
            .model
            .as_deref()
            .map_or_else(String::new, |m| format!(", {m}"));
        format!(
            "format repairs: the Sizer put {} of {} {role} answer(s) from {}{model} in form",
            self.repairs, self.calls, self.binding
        )
    }
}

/// The run's format repairs, by the role, binding and model whose answers
/// needed them.
pub fn repairs(usage: &[Usage]) -> Vec<Repairs> {
    let mut by: BTreeMap<super::flow::Repaired, usize> = BTreeMap::new();
    for r in usage.iter().filter_map(|u| u.repaired.as_ref()) {
        *by.entry(r.clone()).or_default() += 1;
    }
    by.into_iter()
        .map(|(r, repairs)| Repairs {
            calls: usage
                .iter()
                .filter(|u| {
                    u.repaired.is_none()
                        && u.role == r.role
                        && u.binding == r.binding
                        && u.fan_out.as_deref() != Some("keep-alive")
                })
                .count(),
            role: r.role,
            binding: r.binding,
            model: r.model,
            repairs,
        })
        .collect()
}

/// Fixes by the binding they went to under `savings.fixer`, easy or hard:
/// their groups, calls and dollars, and how many of their blockers held,
/// not raised again the next round, of those a next round attacked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FixBinding {
    pub binding: String,
    pub easy: bool,
    pub groups: usize,
    pub calls: usize,
    pub usd: Option<f64>,
    pub blockers: usize,
    /// Its blockers a next round attacked, and of those, the ones not
    /// raised again.
    pub judged: usize,
    pub held: usize,
}

impl FixBinding {
    /// One line of it, for a person.
    pub fn line(&self) -> String {
        let cost = self.usd.map_or_else(String::new, |d| format!(", ${d:.3}"));
        format!(
            "fixes on {} ({}): {} group(s), {} call(s){cost}; {} of {} blocker(s) a next round \
             attacked held",
            self.binding,
            if self.easy { "easy" } else { "hard" },
            self.groups,
            self.calls,
            self.held,
            self.judged
        )
    }
}

/// The run's fixes by binding, where `savings.fixer` routed them.
pub fn fix_routes(run: &super::flow::Run) -> Vec<FixBinding> {
    let mut out: Vec<FixBinding> = Vec::new();
    let found = |id: &str| -> Option<(&str, u32, &super::flow::Finding)> {
        run.lenses.values().find_map(|l| {
            l.rounds.iter().find_map(|r| {
                r.findings
                    .iter()
                    .find(|f| f.id == id)
                    .map(|f| (l.id.as_str(), r.round, f))
            })
        })
    };
    for route in &run.routes {
        let at = match out
            .iter()
            .position(|b| b.binding == route.binding && b.easy == route.easy)
        {
            Some(at) => at,
            None => {
                out.push(FixBinding {
                    binding: route.binding.clone(),
                    easy: route.easy,
                    groups: 0,
                    calls: 0,
                    usd: Some(0.0),
                    blockers: 0,
                    judged: 0,
                    held: 0,
                });
                out.len() - 1
            }
        };
        let b = &mut out[at];
        b.groups += 1;
        b.blockers += route.ids.len();
        for id in &route.ids {
            let Some((lens, round, f)) = found(id) else {
                continue;
            };
            let key = super::flow::claim_key(&f.title);
            let Some(next) = run
                .lenses
                .get(lens)
                .and_then(|l| l.rounds.iter().find(|r| r.round == round + 1))
            else {
                continue;
            };
            b.judged += 1;
            if !next
                .findings
                .iter()
                .any(|g| g.blocks() && super::flow::claim_key(&g.title) == key)
            {
                b.held += 1;
            }
        }
    }
    for b in &mut out {
        let tag = if b.easy { "easy" } else { "hard" };
        for u in run
            .usage
            .iter()
            .filter(|u| u.route.as_deref() == Some(tag) && u.binding == b.binding)
        {
            b.calls += 1;
            b.usd = match (b.usd, u.usd) {
                (Some(a), Some(c)) => Some(a + c),
                _ => None,
            };
        }
    }
    out
}

/// A round's units restarted fresh after coming back unusable: how many,
/// what their fresh sessions cost, and why each was restarted. Counted
/// apart from repairs, which put an answer in form without a fresh session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Restarts {
    pub round: u32,
    /// The restarted units, each once, with why.
    pub units: Vec<(String, String)>,
    /// Their fresh sessions' calls.
    pub calls: usize,
    pub tokens: u64,
    pub usd: Option<f64>,
}

impl Restarts {
    /// One line of it, for a person.
    pub fn line(&self) -> String {
        let cost = self.usd.map_or_else(String::new, |d| format!(", ${d:.3}"));
        format!(
            "round {}: restarted fresh: {} unit(s), {} call(s), {} tokens{cost}: {}",
            self.round,
            self.units.len(),
            self.calls,
            self.tokens,
            self.units
                .iter()
                .map(|(unit, why)| format!("{unit} ({why})"))
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

/// A call whose model asked for tools its binding doesn't name: one a
/// gateway added to its requests, or one it made up. Seen, not a failure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnnamedTools {
    pub round: u32,
    pub call: String,
    pub binding: String,
    /// Each tool, and how often it was asked for.
    pub tools: BTreeMap<String, u64>,
}

impl UnnamedTools {
    pub fn line(&self) -> String {
        let tools: Vec<String> = self
            .tools
            .iter()
            .map(|(name, n)| format!("{name} ({n}×)"))
            .collect();
        format!(
            "round {}: {} (binding {}) asked for tool(s) its binding doesn't name: {}",
            self.round,
            self.call,
            self.binding,
            tools.join(", ")
        )
    }
}

/// Every call that asked for a tool its binding doesn't name.
pub fn unnamed_tools(usage: &[Usage]) -> Vec<UnnamedTools> {
    usage
        .iter()
        .filter(|u| !u.unnamed_tools.is_empty())
        .map(|u| UnnamedTools {
            round: u.round,
            call: u.label.clone().unwrap_or_default(),
            binding: u.binding.clone(),
            tools: u.unnamed_tools.clone(),
        })
        .collect()
}

/// Each round's restarts, from the calls marked as a restart's.
pub fn restarts(usage: &[Usage]) -> Vec<Restarts> {
    let mut by: BTreeMap<u32, Restarts> = BTreeMap::new();
    for u in usage {
        let Some(why) = &u.restart else {
            continue;
        };
        let r = by.entry(u.round).or_insert_with(|| Restarts {
            round: u.round,
            units: Vec::new(),
            calls: 0,
            tokens: 0,
            usd: Some(0.0),
        });
        let unit = u
            .call
            .clone()
            .or_else(|| u.label.clone())
            .unwrap_or_default();
        if !r.units.iter().any(|(n, _)| *n == unit) {
            r.units.push((unit, why.clone()));
        }
        r.calls += 1;
        r.tokens += u.tokens_in + u.cache_read + u.tokens_out;
        r.usd = match (r.usd, u.usd) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
    }
    by.into_values().collect()
}

/// One role's calls in a round, or in the whole run: what a review's cost
/// is compared by. Keep-alive calls, and the Sizer's call that finds the
/// defects more than one lens found, are counted apart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoleUsage {
    /// None for the whole run.
    pub round: Option<u32>,
    /// "Sizer", "Orchestrator", "reviewers", "Judges", "dedupe",
    /// "keep-alive" or "Configurer".
    pub role: &'static str,
    pub calls: usize,
    /// Input not read from a cache, written to one included.
    pub fresh: u64,
    /// Input read from a cache.
    pub cached: u64,
    pub output: u64,
    /// None if a call billed in dollars has no price.
    pub usd: Option<f64>,
    /// The shortest and longest call, in seconds.
    pub secs_min: Option<u64>,
    pub secs_max: Option<u64>,
    /// For a fan-out in a round: what its first call read from a cache,
    /// and the rest, on average. The first fills the prefix the rest read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_cached: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest_cached: Option<u64>,
    /// Tokens billed fresh that the provider should have read from its
    /// cache: see `adjusted`.
    pub shortfall: u64,
    /// What the calls would have cost had the provider cached them: derived,
    /// not billed. None if a shortfall has no cached price to go by.
    pub usd_adjusted: Option<f64>,
}

impl RoleUsage {
    /// One line of it, for a person.
    pub fn line(&self) -> String {
        let round = self
            .round
            .map_or_else(|| "all rounds".to_string(), |r| format!("round {r}"));
        let usd = self
            .usd
            .map_or_else(|| "no price".to_string(), |d| format!("${d:.3}"));
        let secs = match (self.secs_min, self.secs_max) {
            (Some(a), Some(b)) if a == b => format!(", {}", span(a)),
            (Some(a), Some(b)) => format!(", {}–{}", span(a), span(b)),
            _ => String::new(),
        };
        let first = match (self.first_cached, self.rest_cached) {
            (Some(f), Some(r)) => format!("; cached: first call {f}, the rest {r} each on average"),
            _ => String::new(),
        };
        let adjusted = match (self.shortfall, self.usd_adjusted) {
            (0, _) => String::new(),
            (n, Some(d)) => format!(
                "; cache-adjusted ${d:.3}, derived: {n} tokens billed fresh that the provider \
                 should have read cached"
            ),
            (n, None) => format!(
                "; {n} tokens billed fresh that the provider should have read cached (no cached \
                 price to adjust by)"
            ),
        };
        format!(
            "{round}, {}: {} call(s), {} fresh, {} cached, {} out, {usd}{secs}{first}{adjusted}",
            self.role, self.calls, self.fresh, self.cached, self.output
        )
    }
}

/// What a cache-adjusted figure is, said wherever one is shown.
/// Said when a run stopped in a round that didn't finish: that round's
/// calls that did are counted, from their answers as they came. None when
/// every call is from a round that finished.
pub fn unfinished_line(usage: &[Usage]) -> Option<String> {
    let late: Vec<&Usage> = usage.iter().filter(|u| u.unfinished).collect();
    let round = late.iter().map(|u| u.round).max()?;
    let usd: f64 = late.iter().filter_map(|u| u.usd).sum();
    Some(format!(
        "round {round} didn't finish: its {} finished call(s), ${usd:.3}, are counted from their \
         answers as they came",
        late.len()
    ))
}

pub const ADJUSTED_IS: &str = "cache-adjusted is derived, not billed: the cost had the \
    provider served from its cache what a turn had just sent, or, for a call after its \
    fan-out's first, the opening that first call sent; the billed figure is what was charged";

/// One call's cost had the provider cached what it should have: derived,
/// beside the billed one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Adjusted {
    /// Tokens billed fresh that it should have read from the cache: from
    /// its second turn on, what its turn before had just sent; and on its
    /// first turn, for a call after its fan-out's first, the opening that
    /// first call sent.
    pub shortfall: u64,
    /// Its billed dollars with those tokens repriced at its cached rate.
    /// None if it has no billed dollars, or a shortfall with no cached rate
    /// to go by.
    pub usd: Option<f64>,
}

/// Each call's `Adjusted`, in order. Rates come from the config's price,
/// else from the call's agent's split of its cost, else from other calls
/// of the same model.
pub fn adjusted(usage: &[Usage]) -> Vec<Adjusted> {
    let per_token = |dollars: Option<f64>, tokens: u64| {
        dollars.filter(|_| tokens > 0).map(|d| d / tokens as f64)
    };
    let model_rate = |model: &Option<String>, cached: bool| -> Option<f64> {
        let same: Vec<&Usage> = usage.iter().filter(|u| u.model == *model).collect();
        let (dollars, tokens) = same.iter().fold((0.0, 0u64), |(d, t), u| {
            let (ud, ut) = if cached {
                (u.usd_cache_read, u.cache_read)
            } else {
                (u.usd_input, u.tokens_in)
            };
            match ud {
                Some(x) if ut > 0 => (d + x, t + ut),
                _ => (d, t),
            }
        });
        (tokens > 0).then(|| dollars / tokens as f64)
    };
    usage
        .iter()
        .map(|u| {
            // The opening its fan-out's first call sent, in the same round.
            let opening = (!u.first)
                .then_some(u.fan_out.as_ref())
                .flatten()
                .and_then(|fan| {
                    usage
                        .iter()
                        .find(|w| w.first && w.round == u.round && w.fan_out.as_ref() == Some(fan))
                })
                .and_then(|w| w.first_turn)
                .map(|t| t.prompt());
            let first = match (opening, u.first_turn) {
                (Some(o), Some(t)) => o.min(t.prompt()).saturating_sub(t.cache_read_tokens),
                _ => 0,
            };
            let shortfall = u.cache_shortfall + first;
            let usd = if u.subscription {
                None
            } else if shortfall == 0 {
                u.usd
            } else {
                let rates = u.price_rates.map(|[f, c]| (f, c)).or_else(|| {
                    let fresh = per_token(u.usd_input, u.tokens_in)
                        .or_else(|| model_rate(&u.model, false))?;
                    let cached = per_token(u.usd_cache_read, u.cache_read)
                        .or_else(|| model_rate(&u.model, true))?;
                    Some((fresh, cached))
                });
                u.usd.zip(rates).map(|(usd, (fresh, cached))| {
                    usd - shortfall as f64 * (fresh - cached).max(0.0)
                })
            };
            Adjusted { shortfall, usd }
        })
        .collect()
}

fn span(secs: u64) -> String {
    if secs < 120 {
        format!("{secs}s")
    } else {
        format!("{:.1} min", secs as f64 / 60.0)
    }
}

/// Which summary a call counts under.
fn group(u: &Usage) -> &'static str {
    if u.fan_out.as_deref() == Some("keep-alive") {
        return "keep-alive";
    }
    match u.role {
        Role::Sizer if u.label.as_deref() == Some(super::flow::DEDUPE) => "dedupe",
        Role::Sizer => "Sizer",
        Role::Orchestrator => "Orchestrator",
        Role::Reviewer => "reviewers",
        Role::Judge => "Judges",
        Role::Configurer => "Configurer",
    }
}

const GROUPS: [&str; 7] = [
    "Sizer",
    "Orchestrator",
    "reviewers",
    "Judges",
    "dedupe",
    "keep-alive",
    "Configurer",
];

fn summed(round: Option<u32>, role: &'static str, calls: &[&Usage], adj: &[Adjusted]) -> RoleUsage {
    let owned: Vec<Usage> = calls.iter().map(|u| (*u).clone()).collect();
    let secs: Vec<u64> = calls.iter().filter_map(|u| u.secs).collect();
    // The first call of a fan-out, and the rest, within one round only.
    let first = calls.iter().find(|u| u.first);
    let rest: Vec<u64> = calls
        .iter()
        .filter(|u| !u.first && u.fan_out.is_some())
        .map(|u| u.cache_read)
        .collect();
    let (first_cached, rest_cached) = match (round, first) {
        (Some(_), Some(f)) if !rest.is_empty() => (
            Some(f.cache_read),
            Some(rest.iter().sum::<u64>() / rest.len() as u64),
        ),
        _ => (None, None),
    };
    RoleUsage {
        round,
        role,
        calls: calls.len(),
        fresh: calls.iter().map(|u| u.tokens_in + u.cache_write).sum(),
        cached: calls.iter().map(|u| u.cache_read).sum(),
        output: calls.iter().map(|u| u.tokens_out).sum(),
        usd: super::flow::dollars(&owned),
        secs_min: secs.iter().min().copied(),
        secs_max: secs.iter().max().copied(),
        first_cached,
        rest_cached,
        shortfall: adj.iter().map(|a| a.shortfall).sum(),
        usd_adjusted: if adj.iter().all(|a| a.usd.is_some() || a.shortfall == 0)
            && adj.iter().any(|a| a.usd.is_some())
        {
            Some(
                calls
                    .iter()
                    .zip(adj)
                    .map(|(u, a)| a.usd.or(u.usd).unwrap_or(0.0))
                    .sum(),
            )
        } else {
            None
        },
    }
}

/// One call's first turn, by its agent's count: what it read from a prompt
/// cache before it did anything else, which says whether a warm-up or an
/// earlier call left the shared prefix where this one could read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FirstTurn {
    pub round: u32,
    /// What the call was, as progress named it, or its role.
    pub call: String,
    pub run_id: String,
    /// Read from a cache.
    pub cached: u64,
    /// The whole prompt: fresh, read from a cache and written to one.
    pub prompt: u64,
    pub output: u64,
}

impl FirstTurn {
    pub fn line(&self) -> String {
        let share = if self.prompt > 0 {
            format!(" ({:.0}%)", self.cached as f64 * 100.0 / self.prompt as f64)
        } else {
            String::new()
        };
        format!(
            "round {}, {}: first turn read {} of {} prompt tokens from the cache{share}",
            self.round, self.call, self.cached, self.prompt
        )
    }
}

/// Each call's first turn, where its agent counted it, in the order made.
pub fn first_turns(usage: &[Usage]) -> Vec<FirstTurn> {
    usage
        .iter()
        .filter_map(|u| {
            let f = u.first_turn?;
            Some(FirstTurn {
                round: u.round,
                call: u
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("{:?}", u.role).to_lowercase()),
                run_id: u.run_id.clone(),
                cached: f.cache_read_tokens,
                prompt: f.prompt(),
                output: f.output_tokens,
            })
        })
        .collect()
}

/// Whether the cache-write figures say nothing: calls read from a cache,
/// but none counted a write. Some providers never report writes, so a 0
/// there is no evidence either way.
pub fn writes_unreported(usage: &[Usage]) -> bool {
    let measured: Vec<&Usage> = usage.iter().filter(|u| u.measured).collect();
    measured.iter().any(|u| u.cache_read > 0) && measured.iter().all(|u| u.cache_write == 0)
}

/// What `acr usage` says under a run whose cache writes all read 0.
pub const WRITES_UNREPORTED: &str = "(cache writes read 0 on every call: some providers \
    don't report writes, so a 0 there says nothing either way)";

/// The run's calls by round and role, then by role over every round if
/// there was more than one.
pub fn by_role(usage: &[Usage]) -> Vec<RoleUsage> {
    let adj = adjusted(usage);
    let mut rounds: Vec<u32> = usage.iter().map(|u| u.round).collect();
    rounds.sort_unstable();
    rounds.dedup();
    let pick = |keep: &dyn Fn(&Usage) -> bool| -> (Vec<&Usage>, Vec<Adjusted>) {
        usage
            .iter()
            .zip(&adj)
            .filter(|(u, _)| keep(u))
            .map(|(u, a)| (u, *a))
            .unzip()
    };
    let mut out = Vec::new();
    for &round in &rounds {
        for role in GROUPS {
            let (calls, a) = pick(&|u| u.round == round && group(u) == role);
            if !calls.is_empty() {
                out.push(summed(Some(round), role, &calls, &a));
            }
        }
    }
    if rounds.len() > 1 {
        for role in GROUPS {
            let (calls, a) = pick(&|u| group(u) == role);
            if !calls.is_empty() {
                out.push(summed(None, role, &calls, &a));
            }
        }
    }
    out
}

fn bindings_used(run: &Run) -> Vec<BindingUsed> {
    let mut used: BTreeMap<(Role, String), BindingUsed> = BTreeMap::new();
    for u in &run.usage {
        used.entry((u.role, u.binding.clone()))
            .or_insert_with(|| BindingUsed {
                role: u.role,
                binding: u.binding.clone(),
                agent: u.agent.clone(),
                model: u.model.clone(),
                calls: 0,
            })
            .calls += 1;
    }
    used.into_values().collect()
}

pub fn report<'a>(run: &'a Run, cfg: &Config) -> Report<'a> {
    Report {
        run_id: &run.id,
        status: run.status,
        waiting_for: run.waiting,
        tier: &run.tier,
        tier_why: &run.tier_why,
        rounds: run.round,
        lenses: run.lenses.values().collect(),
        escalations: &run.escalations,
        disagreements: &run.disagreements,
        bindings_used: bindings_used(run),
        tokens: run.usage.iter().map(Usage::tokens).sum(),
        calls_measured: run.usage.iter().filter(|u| u.measured).count(),
        calls_estimated: run.usage.iter().filter(|u| !u.measured).count(),
        calls_on_subscription: run.usage.iter().filter(|u| u.subscription).count(),
        usd: super::flow::dollars(&run.usage),
        agent_usd: run
            .usage
            .iter()
            .filter_map(|u| u.agent_usd)
            .reduce(|a, b| a + b),
        labels: labels(run, cfg),
        by_role: by_role(&run.usage),
        facts: &run.facts,
        prefixes: &run.prefixes,
        repairs: repairs(&run.usage),
        sessions: sessions(&run.usage),
        unnamed_tools: unnamed_tools(&run.usage),
        reattack: run.reattack_line(),
        weakened: &run.weakened,
        answered_by: &run.answered_by,
        tickets: tickets(run),
        flags: &run.flags,
    }
}

/// The run as it's kept, with its calls summed by role beside them.
#[derive(Serialize)]
pub struct Record<'a> {
    #[serde(flatten)]
    pub run: &'a Run,
    pub by_role: Vec<RoleUsage>,
}

pub fn record(run: &Run) -> Record<'_> {
    Record {
        run,
        by_role: by_role(&run.usage),
    }
}

/// The labels a run calls for, if the repository turned them on.
pub fn labels(run: &Run, cfg: &Config) -> Vec<String> {
    let Some(names) = &cfg.integrations.labels else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let done = run.status == Status::Confirmed;
    let overall = if done { &names.ready } else { &names.not_ready };
    out.extend(overall.clone());
    for lens in run.lenses.values() {
        // A lens that skipped the change as it is now hasn't passed it: it
        // gets neither label.
        if lens.skipped_since_attack().is_some() {
            continue;
        }
        let name = match lens.outcome {
            Outcome::Open => &names.waiting,
            Outcome::Confirmed | Outcome::Waived => &names.confirmed,
        };
        out.extend(name.as_ref().map(|n| n.replace("{lens}", &lens.id)));
    }
    out
}

/// A lens's outcome as said to a person: one that skipped the change as it
/// is now says so, and why, rather than that it passed.
pub fn outcome_of(lens: &LensResult) -> String {
    match lens.skipped_since_attack() {
        Some(s) => format!(
            "Skipped in round {}, not attacked on the change as it is now: {}",
            s.round, s.reason
        ),
        None => format!("{:?}", lens.outcome),
    }
}

/// Every label this config could put on a change, so old ones can come off.
pub fn all_labels(run: &Run, cfg: &Config) -> Vec<String> {
    let Some(names) = &cfg.integrations.labels else {
        return Vec::new();
    };
    let mut out: Vec<String> = names
        .ready
        .iter()
        .chain(&names.not_ready)
        .cloned()
        .collect();
    for lens in run.lenses.values() {
        for name in [&names.waiting, &names.confirmed].into_iter().flatten() {
            out.push(name.replace("{lens}", &lens.id));
        }
    }
    out
}

pub fn finding_line(f: &Finding) -> String {
    let at = f
        .file
        .as_deref()
        .map(|p| format!(" `{p}`"))
        .unwrap_or_default();
    let state = match f.state {
        FindingState::Open => "open",
        FindingState::Folded => "folded in",
        FindingState::Waived => "waived",
        FindingState::Deferred => "deferred to a ticket",
        FindingState::Logged => "logged",
        FindingState::Dropped => "rejected",
    };
    let reason = match f.severity {
        Severity::Rejected => format!(" Reason: {}", f.reason),
        _ => String::new(),
    };
    let ruled = match (f.reused_from, &f.ruled_by) {
        (Some(round), _) => format!(" Ruling carried over from round {round}."),
        (None, Some(binding)) => format!(" Ruled by {binding}."),
        (None, None) => String::new(),
    };
    format!(
        "- **{:?}**{at}: {} ({state}).{reason}{ruled}\n",
        f.severity, f.title
    )
}

/// The PR comment, if the repository turned it on. Reviewers are labeled as
/// the AI agents they are.
pub fn comment(run: &Run, cfg: &Config) -> Option<String> {
    let format = cfg.integrations.comment?;
    let mut out = format!(
        "## ACR: {:?}\n\nTier {} ({}). {} round(s). Reviews by AI agents.\n\n",
        run.status, run.tier, run.tier_why, run.round
    );
    for lens in run.lenses.values() {
        out.push_str(&format!(
            "### {} ({})\n\n{}\n\n",
            lens.id,
            outcome_of(lens),
            lens.criteria
        ));
        if format == CommentFormat::Full {
            for round in &lens.rounds {
                out.push_str(&format!(
                    "Round {}: {}\n\n",
                    round.round,
                    round.reviewers.join("; ")
                ));
                for f in &round.findings {
                    out.push_str(&finding_line(f));
                }
                out.push('\n');
            }
        } else if let Some(round) = lens.rounds.last() {
            for f in round.findings.iter().filter(|f| f.blocks()) {
                out.push_str(&finding_line(f));
            }
        }
    }
    for e in &run.escalations {
        out.push_str(&format!("\n**Escalated:** {}\n", e.reason));
    }
    for d in &run.disagreements {
        out.push_str(&format!(
            "\n**Disagreement on {}:** {}\n",
            d.finding,
            d.sides.join(" / ")
        ));
    }
    // Without the command, which names a path on the reviewing machine.
    for f in &run.flags {
        let state = match f.state {
            FlagState::Open => "not answered",
            FlagState::Kept => "kept",
            FlagState::Reverting | FlagState::Reverted => "reverted",
        };
        out.push_str(&format!(
            "\n**Flagged:** round {}'s fix changed `{}` ({}): {state}\n",
            f.round,
            f.path,
            f.why()
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(
        role: &str,
        round: u32,
        label: Option<&str>,
        fan_out: Option<&str>,
        first: bool,
    ) -> Usage {
        serde_json::from_value(serde_json::json!({
            "role": role, "binding": "b", "agent": "a@here", "model": "m", "run_id": "x",
            "tokens_in": 100, "cache_read": if first { 0 } else { 900 }, "cache_write": 0,
            "tokens_out": 10, "usd": 0.5, "round": round, "secs": if first { 30 } else { 600 },
            "label": label, "fan_out": fan_out, "first": first,
        }))
        .unwrap()
    }

    #[test]
    fn the_cost_had_the_provider_cached_what_it_should_is_beside_the_billed_one() {
        let turn = |input, cached| crate::proto::TurnUsage {
            input_tokens: input,
            output_tokens: 10,
            cache_read_tokens: cached,
            cache_write_tokens: 0,
        };
        // The warm-up: its first turn sends the 40k opening fresh.
        let mut warm = call("reviewer", 1, Some("r 1"), Some("reviewers"), true);
        warm.first_turn = Some(turn(40_000, 0));
        // A reviewer after it whose first turn found nothing cached, and
        // whose later turns lost 10k more: 50k it should have read cached.
        let mut cold = call("reviewer", 1, Some("r 2"), Some("reviewers"), false);
        cold.first_turn = Some(turn(40_000, 0));
        cold.cache_shortfall = 10_000;
        cold.tokens_in = 60_000;
        cold.cache_read = 100_000;
        // $1 per million fresh, $0.10 per million cached, by its agent.
        cold.usd_input = Some(0.06);
        cold.usd_cache_read = Some(0.01);
        cold.usd = Some(0.5);
        // One whose first turn read the opening: nothing to adjust.
        let mut hit = call("reviewer", 1, Some("r 3"), Some("reviewers"), false);
        hit.first_turn = Some(turn(500, 40_000));
        let all = [warm, cold, hit];
        let adj = adjusted(&all);
        assert_eq!(
            adj[0],
            Adjusted {
                shortfall: 0,
                usd: Some(0.5)
            },
            "the warm-up fills it"
        );
        assert_eq!(adj[1].shortfall, 50_000);
        // 50k at $0.90 a million less: 0.5 - 0.045.
        assert!((adj[1].usd.unwrap() - 0.455).abs() < 1e-9, "{:?}", adj[1]);
        assert_eq!(
            adj[2],
            Adjusted {
                shortfall: 0,
                usd: Some(0.5)
            }
        );
        let rows = by_role(&all);
        assert_eq!(rows[0].shortfall, 50_000);
        assert!((rows[0].usd_adjusted.unwrap() - 1.455).abs() < 1e-9);
        let line = rows[0].line();
        assert!(
            line.contains(
                "cache-adjusted $1.455, derived: 50000 tokens billed fresh that the \
                           provider should have read cached"
            ),
            "{line}"
        );
        // No price for the cached tokens anywhere: no adjusted figure.
        let mut unknown = all[1].clone();
        unknown.usd_input = None;
        unknown.usd_cache_read = None;
        assert_eq!(adjusted(&[all[0].clone(), unknown])[1].usd, None);
    }

    #[test]
    fn each_calls_first_turn_says_what_it_read_from_the_cache() {
        let mut warm = call(
            "reviewer",
            1,
            Some("correctness: reviewer 1"),
            Some("reviewers"),
            false,
        );
        warm.first_turn = Some(crate::proto::TurnUsage {
            input_tokens: 205,
            output_tokens: 104,
            cache_read_tokens: 37_376,
            cache_write_tokens: 0,
        });
        let unknown = call("judge", 1, Some("correctness: the Judge"), None, false);
        let firsts = first_turns(&[warm, unknown]);
        assert_eq!(firsts.len(), 1, "only a call its agent counted");
        assert_eq!((firsts[0].cached, firsts[0].prompt), (37_376, 37_581));
        assert_eq!(
            firsts[0].line(),
            "round 1, correctness: reviewer 1: first turn read 37376 of 37581 prompt tokens \
             from the cache (99%)"
        );
    }

    #[test]
    fn cache_writes_that_all_read_zero_are_said_to_say_nothing() {
        let mut read = call("reviewer", 1, Some("r"), None, false);
        read.measured = true;
        assert!(writes_unreported(std::slice::from_ref(&read)));
        let mut wrote = read.clone();
        wrote.cache_write = 4_000;
        assert!(!writes_unreported(&[read.clone(), wrote]));
        // Nothing read from a cache, or nothing counted: nothing to say.
        let mut cold = read.clone();
        cold.cache_read = 0;
        assert!(!writes_unreported(&[cold]));
        let mut guessed = read;
        guessed.measured = false;
        assert!(!writes_unreported(&[guessed]));
    }

    #[test]
    fn calls_are_summed_by_round_and_role_with_dedupe_and_keep_alive_apart() {
        let usage = vec![
            call("sizer", 1, Some("the Sizer, choosing a tier"), None, false),
            call("orchestrator", 1, Some("the Orchestrator"), None, false),
            call(
                "reviewer",
                1,
                Some("alpha: reviewer 1"),
                Some("reviewers"),
                true,
            ),
            call(
                "reviewer",
                1,
                Some("alpha: reviewer 2"),
                Some("reviewers"),
                false,
            ),
            call(
                "reviewer",
                1,
                Some("beta: reviewer 1"),
                Some("reviewers"),
                false,
            ),
            call(
                "reviewer",
                1,
                Some("cache keep-alive: reviewers"),
                Some("keep-alive"),
                false,
            ),
            call("judge", 1, Some("alpha: the Judge"), Some("judges"), true),
            call("sizer", 1, Some(super::super::flow::DEDUPE), None, false),
            // A call from before labels counts by its role.
            call("reviewer", 2, None, None, false),
        ];
        let rows = by_role(&usage);
        let get = |round: Option<u32>, role: &str| {
            rows.iter()
                .find(|r| r.round == round && r.role == role)
                .unwrap_or_else(|| panic!("{round:?} {role}: {rows:?}"))
        };
        let reviewers = get(Some(1), "reviewers");
        assert_eq!(
            (reviewers.calls, reviewers.fresh, reviewers.cached),
            (3, 300, 1800)
        );
        assert_eq!(
            (reviewers.first_cached, reviewers.rest_cached),
            (Some(0), Some(900))
        );
        assert_eq!(
            (reviewers.secs_min, reviewers.secs_max),
            (Some(30), Some(600))
        );
        assert!((reviewers.usd.unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(get(Some(1), "keep-alive").calls, 1);
        assert_eq!(get(Some(1), "dedupe").calls, 1);
        assert_eq!(get(Some(1), "Sizer").calls, 1);
        assert_eq!(
            get(Some(1), "Judges").first_cached,
            None,
            "a fan-out of one has no rest"
        );
        assert_eq!(get(Some(2), "reviewers").calls, 1);
        // Over every round, no first-against-rest.
        let all = get(None, "reviewers");
        assert_eq!((all.calls, all.first_cached), (4, None));
        let order: Vec<&str> = rows
            .iter()
            .filter(|r| r.round == Some(1))
            .map(|r| r.role)
            .collect();
        assert_eq!(
            order,
            [
                "Sizer",
                "Orchestrator",
                "reviewers",
                "Judges",
                "dedupe",
                "keep-alive"
            ]
        );
        let line = reviewers.line();
        assert_eq!(
            line,
            "round 1, reviewers: 3 call(s), 300 fresh, 1800 cached, 30 out, $1.500, 30s–10.0 min; \
             cached: first call 0, the rest 900 each on average"
        );
    }
}
