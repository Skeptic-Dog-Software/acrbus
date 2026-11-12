//! What a review reports: machine-readable, with adapters for the labels and
//! comments a repository turns on.

use std::collections::BTreeMap;

use serde::Serialize;

use super::config::{CommentFormat, Config, Role, Severity};
use super::facts::Prefix;
use super::flow::{
    Disagreement, Escalation, FactsRound, Finding, FindingState, Gate, LensResult, Outcome, Run,
    Status, Usage,
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
        format!(
            "{round}, {}: {} call(s), {} fresh, {} cached, {} out, {usd}{secs}{first}",
            self.role, self.calls, self.fresh, self.cached, self.output
        )
    }
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

fn summed(round: Option<u32>, role: &'static str, calls: &[&Usage]) -> RoleUsage {
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
    }
}

/// The run's calls by round and role, then by role over every round if
/// there was more than one.
pub fn by_role(usage: &[Usage]) -> Vec<RoleUsage> {
    let mut rounds: Vec<u32> = usage.iter().map(|u| u.round).collect();
    rounds.sort_unstable();
    rounds.dedup();
    let mut out = Vec::new();
    for &round in &rounds {
        for role in GROUPS {
            let calls: Vec<&Usage> = usage
                .iter()
                .filter(|u| u.round == round && group(u) == role)
                .collect();
            if !calls.is_empty() {
                out.push(summed(Some(round), role, &calls));
            }
        }
    }
    if rounds.len() > 1 {
        for role in GROUPS {
            let calls: Vec<&Usage> = usage.iter().filter(|u| group(u) == role).collect();
            if !calls.is_empty() {
                out.push(summed(None, role, &calls));
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
        let name = match lens.outcome {
            Outcome::Open => &names.waiting,
            Outcome::Confirmed | Outcome::Waived => &names.confirmed,
        };
        out.extend(name.as_ref().map(|n| n.replace("{lens}", &lens.id)));
    }
    out
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

fn finding_line(f: &Finding) -> String {
    let at = f
        .file
        .as_deref()
        .map(|p| format!(" `{p}`"))
        .unwrap_or_default();
    let state = match f.state {
        FindingState::Open => "open",
        FindingState::Folded => "folded in",
        FindingState::Waived => "waived",
        FindingState::Logged => "logged",
        FindingState::Dropped => "rejected",
    };
    let reason = match f.severity {
        Severity::Rejected => format!(" Reason: {}", f.reason),
        _ => String::new(),
    };
    format!(
        "- **{:?}**{at}: {} ({state}).{reason}\n",
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
            "### {} ({:?})\n\n{}\n\n",
            lens.id, lens.outcome, lens.criteria
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
