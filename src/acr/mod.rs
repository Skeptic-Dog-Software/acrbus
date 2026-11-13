//! `acrbus acr`: adversarial convergence review for any repository. The
//! framework holds no knowledge of a repository. Roles, tiers, lenses and
//! bindings all come from config, in `config`.

mod calibration;
mod carousel;
mod carry;
mod change;
mod config;
mod configure;
mod facts;
mod flow;
mod generated;
pub mod grayboard;
mod keep;
mod learned;
mod plan;
pub mod progress;
mod report;
mod roles;
mod stages;
mod trial;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};

use self::change::Change;
use self::config::{Config, Flags, HumanGate, REPO_CONFIG, ReviewScope, Role, SCHEMA};
use self::flow::{Answer, Call, Caller, Decision, Known, Run, Setting, Status};
use crate::attach::Attachments;
use crate::config::APP_NAME;
use crate::config::{Config as BusConfig, Paths};
use crate::local::{Awaited, Client, Request, Sent};
use crate::proto::{Extras, MAX_BODY, RunOptions, RunStatus};
use crate::util::new_id;

/// The bus agent reviews are sent as, always: a review's traffic is its
/// own, and never lands in a session's inbox.
const SENDER: &str = "acr";
/// How long a review waits for each reply. It counts time queued behind
/// the bridge's other runs, so it's long: the bridge's cap on the run
/// itself is what stops one that runs away.
const REPLY_WAIT: u64 = 24 * 3600;
/// `run` and `resume` exit with these for a review that needs a person.
const WAITING: u8 = 3;
const ESCALATED: u8 = 4;
/// `run` and `resume` exit with this for a review paused by an outage: its
/// calls couldn't reach their model. It's saved, and `acr resume` goes on.
const OUTAGE: u8 = 5;
/// How long a unit whose call couldn't reach its model waits to tell an
/// outage from its own failure, before it restarts.
const OUTAGE_WAIT_SECS: u64 = 60;

#[derive(Args, Clone)]
pub struct Target {
    /// The repository.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// The branch the change goes into. A config in the repository is read
    /// from here, never from the change.
    #[arg(long)]
    base: String,
    /// The repository's config, kept outside it. Left out, it's
    /// `<repository>.yaml` in the `acr` folder beside your config.toml if that's there, or else the
    /// repository's own `.acr/config.yaml`, from `--base`.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// The change's head. The review reads a copy of it, so the checkout can
    /// be on any branch meanwhile; fixes go into the checkout, which must
    /// hold it by then. Left out, the working tree.
    #[arg(long)]
    head: Option<String>,
    #[arg(long, value_parser = parse_gate)]
    human_gate: Option<HumanGate>,
    #[arg(long)]
    max_rounds: Option<u32>,
    /// After each fix, attack again with only the lenses that had blockers
    /// (`blocked`), or with every lens (`all`). Left out, the config says,
    /// or the review asks at its scope gate.
    #[arg(long, value_parser = parse_reattack)]
    reattack: Option<config::Reattack>,
    /// A spend cap for this review, in dollars: past it, no new call
    /// starts, and you're asked to raise it, go on without it, or stop.
    #[arg(long, value_name = "USD")]
    spend_cap_usd: Option<f64>,
    #[arg(long, value_parser = parse_scope)]
    review_scope: Option<ReviewScope>,
    /// Use a binding for a role, as `role=name`. Can be repeated.
    #[arg(long = "binding", value_name = "ROLE=NAME", value_parser = parse_binding)]
    bindings: Vec<(Role, String)>,
    /// What the change says it's for, in a file, such as its pull request's
    /// description. Every role gets it, with the commit messages, ahead of
    /// the diff.
    #[arg(long, value_name = "FILE", conflicts_with = "pr")]
    description: Option<PathBuf>,
    /// The pull request whose title and description say what the change is
    /// for, read with `gh`. `--post-pr` reads its own when this is left out.
    /// Each round's shared facts carry its title, description, review
    /// comments and the issues it links.
    #[arg(long, value_name = "NUMBER")]
    pr: Option<u64>,
    /// The building session's brief for round 1's reviewers: a YAML file
    /// with a `scope` statement, and `lenses`, each lens's where and how
    /// to attack. See docs/acr-repo-setup.md.
    #[arg(long, value_name = "FILE")]
    brief: Option<PathBuf>,
}

/// What the change says it's for: `--description`'s file, or a pull
/// request's title and description. None, if neither is given: then the
/// roles go by the commit messages and the change's own comments.
fn description_of(target: &Target, post_pr: Option<u64>) -> Result<Option<String>> {
    if let Some(file) = &target.description {
        let text = fs::read_to_string(file)
            .with_context(|| format!("reading the description, {}", file.display()))?;
        return Ok(Some(text));
    }
    let Some(pr) = target.pr.or(post_pr) else {
        return Ok(None);
    };
    let out = Command::new("gh")
        .args(["pr", "view", &pr.to_string(), "--json", "title,body"])
        .args(["--jq", r##""# " + .title + "\n\n" + (.body // "")"##])
        .current_dir(&target.repo)
        .stdin(Stdio::null())
        .output()
        .context("running gh, to read the pull request's description")?;
    if !out.status.success() {
        bail!(
            "couldn't read pull request #{pr}'s description with gh: {}. Pass \
             --description <file> instead.",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(Some(
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    ))
}

fn parse_gate(s: &str) -> Result<HumanGate, String> {
    serde_yaml_ng::from_str(s).map_err(|_| "use per-round, ends or exception".to_string())
}

fn parse_reattack(s: &str) -> Result<config::Reattack, String> {
    serde_yaml_ng::from_str(s).map_err(|_| "use blocked or all".to_string())
}

fn parse_scope(s: &str) -> Result<ReviewScope, String> {
    serde_yaml_ng::from_str(s).map_err(|_| "use whole-pr or bottom-of-stack".to_string())
}

fn parse_binding(s: &str) -> Result<(Role, String), String> {
    let (role, name) = s.split_once('=').ok_or("use role=name")?;
    let role = serde_yaml_ng::from_str(role).map_err(|_| format!("{role} isn't a role"))?;
    Ok((role, name.to_string()))
}

#[derive(Subcommand)]
pub enum AcrCmd {
    /// Check a repository's config against the schema and its rules.
    Validate {
        /// The config file. Left out, the repository's, from `--base`.
        file: Option<PathBuf>,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        base: Option<String>,
    },
    /// Whether this machine, and a repository, are ready for review: each
    /// check says it's fine, or what's wrong and the command that fixes it.
    /// It runs the review config's test commands in a copy, as a review
    /// does, and as its reviewers would, and signs in with ACR's token: one
    /// small model call.
    Doctor {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Leave out running the tests, which can take a while.
        #[arg(long)]
        no_tests: bool,
    },
    /// Clear the copies reviews, checks, the doctor and the configurer left,
    /// now rather than at the daemon's scheduled cleanup: those of reviews
    /// that have finished or been stopped, and of checks, doctor runs and
    /// configure runs that have ended. A review running or waiting keeps
    /// its copies.
    Clean {
        /// List what would go, and its size, and remove nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the published schema.
    Schema,
    /// How the pipeline's Judge rates what each key's own review found, set
    /// against how that review rated it: blockers agreed on, missed, and
    /// overrated. A benchmark of the Judge against a past review, which
    /// calibrates nothing. Runs no reviewer: one Judge call for each
    /// dimension.
    JudgeCheck {
        keys: Vec<PathBuf>,
        /// Append each key's result to this file, as a line of JSON.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Also rate each finding in a Judge call of its own, given only its
        /// claim, as `facts.judges: per-finding` does, and set those
        /// verdicts against the per-dimension ones and the key's.
        #[arg(long)]
        per_finding: bool,
        /// Also try a first-pass Judge on this binding, as
        /// `savings.first_pass_judge` would: its verdicts where it settles a
        /// dimension, the Judge's where it sends it on, set against the key's.
        /// Left out, the repository's `savings.first_pass_judge`, if any.
        #[arg(long, value_name = "BINDING")]
        first_pass: Option<String>,
    },
    /// Configure a repository's review from what's in it. The configurer, a
    /// strong model, reviews the whole repository, in a clone of its own, and
    /// proposes its review config: the dimensions that matter, what blocks in
    /// each, and how to work and test in it. Accept it with `acr calibration
    /// --accept`.
    Configure {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Fold what you've said of a repository's reviews, with `acr feedback`,
    /// into its config: the configurer reviews the repository again, with its
    /// config and that feedback, and proposes a revised config.
    Calibrate {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Say how a finished review did, so `acr calibrate` can make the next
    /// ones better. With only the review's ID, it lists the findings to say
    /// it of.
    Feedback {
        /// The review's ID.
        run: String,
        /// A finding, by its number in the list or its ID.
        #[arg(long, requires = "verdict", conflicts_with = "missed")]
        finding: Option<String>,
        /// How the finding did.
        #[arg(long, value_enum, requires = "finding")]
        verdict: Option<configure::Verdict>,
        /// A defect the review missed.
        #[arg(long)]
        missed: Option<String>,
        /// The lens a missed defect belongs to.
        #[arg(long, requires = "missed")]
        lens: Option<String>,
        /// Anything else to say of it.
        #[arg(long)]
        note: Option<String>,
    },
    /// Whether the defects a key's review found, and the blockers a saved
    /// run upheld, are still present in a final version of the change: the
    /// key's head with `--patch` (a review to convergence's), or `--commit`
    /// (what was merged). Check both sides' final versions against the same
    /// list to compare them on the same terms.
    Residual {
        key: PathBuf,
        #[arg(long, conflicts_with = "commit")]
        patch: Option<PathBuf>,
        #[arg(long)]
        commit: Option<String>,
        /// A results file whose last line for the key adds its upheld
        /// blockers to the list.
        #[arg(long)]
        also: Option<PathBuf>,
        /// Append the result to this file, as a line of JSON.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Score saved trials of new work, once its own review has filled in the
    /// key's findings.
    Rescore {
        key: PathBuf,
        /// The results file the trials were written to.
        #[arg(long)]
        results: PathBuf,
    },
    /// Measure review designs on a change, blind, with nobody in the loop,
    /// leaving nothing behind. Prints each design's result as a line of
    /// JSON, scored against the key's findings when it has them.
    Trial {
        /// The answer key: the change, what it's for, and what an earlier
        /// review found. For new work, leave the findings out: the trials
        /// are saved unscored, for `rescore` once its own review is done.
        key: PathBuf,
        /// One design. Left out, the three that review one round, one after
        /// another; `converge` runs only when named.
        #[arg(long, value_enum)]
        arm: Option<trial::Arm>,
        /// Add each result, as a line of JSON, to this file too.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// End a pipeline design's run at its first failure: a reviewer,
        /// Judge or stage that didn't finish its work, or a brief that
        /// couldn't be written. What its sessions left is kept.
        #[arg(long)]
        stop_on_failure: bool,
    },
    /// Check that a trial on this key can run, before spending one on it.
    /// A session is started as a builder is, and one through each bridge and
    /// set of tools the separated design uses: each must start, read the
    /// trial's copy, fail to read a file outside it, and run a command where
    /// it has a shell. A few small calls.
    Check {
        /// The answer key, as for `trial`.
        key: PathBuf,
    },
    /// Allow ACR to review a repository on this machine.
    AddRepo {
        /// The repository's top folder.
        path: PathBuf,
        /// The bridge ACR reaches its models through here. Not `agent`, which
        /// the global `--as` and `ACRBUS_AGENT` would fill.
        #[arg(long, default_value = crate::setup::ACR_AGENT)]
        bridge: String,
    },
    /// Stop allowing ACR to review a repository on this machine.
    RemoveRepo {
        path: PathBuf,
        #[arg(long, default_value = crate::setup::ACR_AGENT)]
        bridge: String,
    },
    /// Show the tier, lenses, bindings and cost, with no model calls.
    Plan {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Review a change. The repository must be calibrated first: see
    /// `acr calibration`.
    Run {
        #[command(flatten)]
        target: Target,
        /// Put the result on this pull request, if the repository turned
        /// that on.
        #[arg(long, value_name = "NUMBER")]
        post_pr: Option<u64>,
        /// Review a repository that isn't calibrated, or whose config has
        /// changed since: nobody has read and accepted the config the review
        /// would use. Calibrated means accepted, not measured.
        #[arg(long)]
        uncalibrated: bool,
    },
    /// Carry work that isn't code into git, so ACR can review it: commits
    /// the version before the work (if given) and the version to review to
    /// a carrier repository, and prints how to review it.
    Carry {
        /// The carrier's name. Its review config is
        /// `<name>.yaml` in the `acr` folder beside your config.toml.
        #[arg(long)]
        name: String,
        /// The version before the work, as a file or folder of text. Left
        /// out, the whole of the work is new.
        #[arg(long, value_name = "PATH")]
        base: Option<PathBuf>,
        /// The version to review, as a file or folder of text.
        #[arg(long, value_name = "PATH")]
        head: PathBuf,
    },
    /// Where a repository's calibration stands: whether its config is
    /// accepted, a proposal waits, or feedback is still to fold in. With
    /// `--accept`, put a waiting proposal in place, or take the config as it
    /// is, and accept it, once you've read it.
    Calibration {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Where a config inside the repository is read from.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// The repository's config, if it's kept somewhere else.
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        #[arg(long)]
        accept: bool,
    },
    /// What a review waiting for a person asks them: its questions, as
    /// question carousels in the shape AskUserQuestion takes, or with
    /// `--text` as a numbered list. Answer with `acr resume <id> --answers`.
    Ask {
        run_id: String,
        #[arg(long)]
        text: bool,
    },
    /// A review's calls summed by round and role: fresh, cached and output
    /// tokens, dollars and times, what each fan-out's calls shared at the
    /// head, and whose answers the Sizer had to put in form. For any review
    /// this bus kept, at work or finished, so two can be set side by side.
    /// `--json` for the rows as the report has them.
    Usage {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Carry on with a review that's waiting for a person.
    Resume {
        run_id: String,
        /// The answers to `acr ask`'s questions, as a JSON object keyed by
        /// each question's text: the JSON itself, `@<file>`, or `-` to read
        /// it from stdin.
        #[arg(long, value_name = "JSON", conflicts_with_all = ["fold", "waive", "defer", "approve", "fixed"])]
        answers: Option<String>,
        /// What the review waits for is fixed, in the working tree, by
        /// whoever ran it: the next round attacks the change as it is now.
        #[arg(long, conflicts_with_all = ["fold", "waive", "defer", "approve", "stop"])]
        fixed: bool,
        /// With `--fixed`: a builder's brief for the next round's lenses, on
        /// what the fixes changed.
        #[arg(long, value_name = "FILE", requires = "fixed")]
        brief: Option<PathBuf>,
        /// Blockers to fix, by ID, with commas.
        #[arg(long, value_delimiter = ',')]
        fold: Vec<String>,
        /// Blockers to waive, by ID, with commas.
        #[arg(long, value_delimiter = ',')]
        waive: Vec<String>,
        /// Blockers to defer to a ticket, by ID, with commas: real, left for
        /// a follow-up, and drafted as tickets in the review's folder.
        #[arg(long, value_delimiter = ',')]
        defer: Vec<String>,
        /// Go on past an escalation, or approve the scope or the result.
        #[arg(long)]
        approve: bool,
        /// End the review here.
        #[arg(long)]
        stop: bool,
        #[arg(long, value_name = "NUMBER")]
        post_pr: Option<u64>,
    },
    /// Stop a review now, in one step, whether it's running or waiting for
    /// a person: its process, if one is driving it, is stopped, which
    /// cancels its calls; the review ends as stopped and lets go of its
    /// claim; and what it left is listed. Nothing it left is touched.
    Stop { run_id: String },
    /// Score a binding set against a commit with known blockers.
    Benchmark {
        #[command(flatten)]
        target: Target,
        /// A YAML list of the blockers it holds: id, pattern and maybe file.
        #[arg(long)]
        known: PathBuf,
    },
}

fn user_config_path(paths: &Paths) -> PathBuf {
    paths.config.with_file_name("acr.yaml")
}

fn run_dir(paths: &Paths, id: &str) -> PathBuf {
    keep::dir(paths).join(id)
}

/// With Grayboard set up, claims the change for review `id`, so a second
/// review of it doesn't start anywhere while this one runs or waits.
/// Refuses when someone else holds it; goes on, saying so, when the claim
/// can't be asked about.
fn claim(paths: &Paths, repo: &str, target: &Target, post_pr: Option<u64>, id: &str) -> Result<()> {
    if !grayboard::configured(paths) {
        return Ok(());
    }
    let head = target.head.as_deref().and_then(|h| {
        change::git(
            &target.repo,
            &["rev-parse", "--verify", &format!("{h}^{{commit}}")],
        )
        .ok()
        .map(|s| s.trim().to_string())
    });
    let key = grayboard::claim_key(repo, target.pr.or(post_pr), head.as_deref());
    let note = format!("{APP_NAME} review {id}");
    let req = crate::local::Request::GrayboardClaim {
        key: key.clone(),
        run: id.to_string(),
        note,
    };
    let answer: grayboard::ClaimAnswer = match crate::local::call(&paths.socket(), &req) {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "{APP_NAME}: couldn't claim {key} on Grayboard ({e:#}); going on without it."
            );
            return Ok(());
        }
    };
    if answer.taken {
        eprintln!("{APP_NAME}: claimed {key} on Grayboard for review {id}.");
        return Ok(());
    }
    if let Some(problem) = answer.problem {
        eprintln!("{APP_NAME}: couldn't claim {key} on Grayboard: {problem}");
        return Ok(());
    }
    let now = crate::util::now_ms();
    bail!(
        "{key} is claimed by {}{}{}: another review of this change is under way. Wait for it \
         to end, or ask its owner.",
        answer.held_by.as_deref().unwrap_or("someone"),
        answer
            .since
            .map(|t| format!(" since {}", crate::util::ago(t, now)))
            .unwrap_or_default(),
        answer.note.map(|n| format!(" ({n})")).unwrap_or_default(),
    )
}

/// Where each lens of a running review's round stands, as text or JSON.
fn lens_states(paths: &Paths, id: &str, text: bool) -> Result<String> {
    let path = run_dir(paths, id).join(flow::LENS_STATES);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(if text {
            format!("Review {id} is running, between rounds: it asks nothing yet.\n")
        } else {
            format!("{}\n", serde_json::json!({"running": true, "lenses": {}}))
        });
    };
    let states: flow::LensStates = serde_json::from_str(&raw)?;
    if !text {
        return Ok(format!(
            "{}\n",
            serde_json::json!({"running": true, "round": states.round, "lenses": states.lenses})
        ));
    }
    let now = crate::util::now_ms();
    let mut out = format!(
        "Review {id} is running, round {}: it asks nothing yet. Each lens:\n",
        states.round
    );
    for (lens, l) in &states.lenses {
        let ago = progress::took(std::time::Duration::from_millis((now - l.at).max(0) as u64));
        out.push_str(&format!(
            "  {lens}: {}, {} of {} reviewer(s) answered, {ago} ago\n",
            l.state, l.answered, l.reviewers
        ));
    }
    Ok(out)
}

/// Refuses to go on with a review that's finished: its files stay a while,
/// to be looked at, but it's over.
fn not_finished(paths: &Paths, id: &str) -> Result<()> {
    if let Some(kept) = keep::kept(paths, id) {
        bail!(
            "review {id} has finished. Its files are kept in {} for {} days, but it can't \
             go on: start a new one",
            kept.display(),
            crate::cleanup::KEEP_DAYS
        );
    }
    Ok(())
}

fn flags(t: &Target) -> Flags {
    Flags {
        human_gate: t.human_gate,
        max_rounds: t.max_rounds,
        reattack: t.reattack,
        spend_cap_usd: t.spend_cap_usd,
        review_scope: t.review_scope,
        bindings: t.bindings.iter().cloned().collect(),
    }
}

/// Where the repository's layer came from.
#[derive(Debug)]
enum Source {
    /// A file outside the repository, whose `not_findings` is beside it.
    File(PathBuf),
    /// The repository's own, from its base.
    Base,
}

/// The repository's layer: a file outside it, if one's named or kept in
/// `acr/`, or else its own, from `base`. Either way nothing in the change can
/// alter it.
fn repo_layer(
    paths: &Paths,
    repo: &Path,
    base: Option<&str>,
    file: Option<&Path>,
) -> Result<(String, Source)> {
    let kept = match file {
        Some(file) => Some(file.to_path_buf()),
        None => {
            let path = outside_config(paths, repo)?;
            path.exists().then_some(path)
        }
    };
    if let Some(file) = kept {
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        return Ok((text, Source::File(file)));
    }
    let base =
        base.with_context(|| format!("give a config file, or --base to read {REPO_CONFIG} from"))?;
    match change::at_base(repo, base, REPO_CONFIG)? {
        Some(text) => Ok((text, Source::Base)),
        // Neither place has one: the repository hasn't been configured,
        // which is what to say, not that a change can't bring its own.
        None => {
            // Said even if naming where it would be fails: that's not the news.
            let kept = outside_config(paths, repo).map_or_else(
                |_| "where it's kept outside the repository".into(),
                |p| p.display().to_string(),
            );
            let name = change::name(repo).unwrap_or_else(|_| repo.display().to_string());
            bail!(
                "{name} has no review config yet: there's none at {kept}, and no {REPO_CONFIG} \
                 on {base}. Have the configurer propose one with `{APP_NAME} acr configure \
                 --repo {}`, then read it and accept it with `{APP_NAME} acr calibration --repo \
                 {} --accept`",
                repo.display(),
                repo.display(),
            )
        }
    }
}

/// Where a repository's config is kept outside it.
fn outside_config(paths: &Paths, repo: &Path) -> Result<PathBuf> {
    let dir = paths.config.with_file_name("acr");
    Ok(dir.join(format!("{}.yaml", change::name(repo)?)))
}

/// What `acr run`'s flags make weaker than the repository's config, each
/// said. Under `answers: person`, in that config or the one the run uses,
/// a person at a terminal confirms them, or the run is refused: an agent
/// that runs the review can't weaken it on its own say.
fn weakened_by_flags(
    paths: &Paths,
    target: &Target,
    cfg: &Config,
    unaccepted: bool,
) -> Result<Vec<String>> {
    let user = config::load_user(&user_config_path(paths))?;
    let unflagged = |file: Option<&Path>| -> Option<Config> {
        let (text, _) = repo_layer(paths, &target.repo, Some(&target.base), file).ok()?;
        config::resolve(&user, config::parse_repo(&text).ok()?, &Flags::default()).ok()
    };
    let own = unflagged(None);
    let given = match &target.config {
        Some(file) => unflagged(Some(file)),
        None => own.clone(),
    };
    let mut out = given
        .as_ref()
        .map(|g| config::weakened(g, cfg))
        .unwrap_or_default();
    if let (Some(file), Some(_)) = (&target.config, &own)
        && outside_config(paths, &target.repo).ok().as_deref() != Some(file.as_path())
    {
        out.push(format!(
            "--config {}, in place of the repository's own config",
            file.display()
        ));
    }
    if unaccepted {
        out.push("--uncalibrated: nobody has accepted the config it uses".into());
    }
    let person = cfg.answers == config::Answers::Person
        || own
            .as_ref()
            .is_some_and(|c| c.answers == config::Answers::Person);
    if person && !out.is_empty() {
        let said = out.join("; ");
        if !at_terminal() {
            bail!(
                "this repository's config takes a person's word only (answers: person), and \
                 this run's flags make the review weaker than its config: {said}. Run it at a \
                 terminal to confirm them, or leave them out"
            );
        }
        eprint!(
            "{APP_NAME}: this run's flags make the review weaker than its config: {said}.\n\
             Type yes to confirm them: "
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("yes") {
            bail!("not confirmed, so the review didn't start");
        }
        out.push("confirmed by a person at the terminal".into());
    }
    Ok(out)
}

/// The config for a review of a change into `base`.
fn load_config(paths: &Paths, t: &Target, flags: &Flags) -> Result<(Config, Source)> {
    let user = config::load_user(&user_config_path(paths))?;
    let (text, source) = repo_layer(paths, &t.repo, Some(&t.base), t.config.as_deref())?;
    let mut cfg = config::resolve(&user, config::parse_repo(&text)?, flags)?;
    cfg.outside = matches!(source, Source::File(_));
    Ok((cfg, source))
}

/// The decided non-issues: beside an outside config, or on the base.
fn not_findings(cfg: &Config, source: &Source, repo: &Path, base: &str) -> Result<String> {
    let Some(path) = cfg.not_findings.as_deref() else {
        return Ok(String::new());
    };
    match source {
        Source::File(file) => {
            let dir = file.parent().unwrap_or(Path::new("."));
            let path = dir.join(path);
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
        }
        Source::Base => flow::from_base(repo, base, Some(path)),
    }
}

/// What a call on a binding asks its bridge for, wherever it works.
/// What a call asks its bridge for: its binding's options, in its
/// repository, and its binding's tools unchanged, unless it asks for none.
fn call_options(call: &Call, keep_logs: bool, timeout: Option<u64>) -> RunOptions {
    let options = RunOptions {
        // Every role works in the repository, so reviewers read the
        // code itself, read-only, and an Orchestrator fixes it there.
        workdir: Some(call.repo.display().to_string()),
        // A trial asks its bridge to copy transcripts into the log too.
        log: Some(keep_logs),
        // Nothing cuts a call off while it works: it asks for as long as
        // its bridge allows, unless its binding caps it, and the bridge
        // ends one that stalls.
        timeout,
        // A reviewer or Judge that hands off past a size: a fresh session
        // carries on from its handoff.
        relay_tokens: call.relay_tokens,
        // A fixer's: counted from its session's first turn, and only once
        // it has edited, since a handoff with nothing changed is no work.
        relay_after_edit: (call.writes && call.relay_tokens.is_some()).then_some(true),
        ..options_for(call.binding)
    };
    // No tools at all: it answers from what it's given.
    if call.no_tools {
        RunOptions {
            tools: Some(Vec::new()),
            ..options
        }
    } else {
        options
    }
}

/// A reviewer's or Judge's call, given the round's shared facts: the facts
/// file, the cache's key for its bridge and model, and, when its roles'
/// tools are matched and its bridge `holds` a call to its own, every tool
/// their calls are given, holding it to its own. A bridge that can't hold
/// one, any but pi's, would refuse the call, so it keeps its own tools.
fn with_shared(options: &mut RunOptions, call: &Call, shared: &flow::Shared, holds: bool) {
    let b = call.binding;
    // A fix's sessions, one per group, all open the same way: its cache's
    // key keeps them on the replica that holds that opening. Nothing else
    // of the round's: the facts are of the change before the fix.
    if call.role == Role::Orchestrator && call.writes {
        options.cache_key = Some(cache_key(&shared.key, &b.agent, b.model.as_deref()));
        return;
    }
    if !matches!(call.role, Role::Reviewer | Role::Judge) || call.writes {
        return;
    }
    options.facts = Some(shared.facts.display().to_string());
    options.cache_key = Some(cache_key(&shared.key, &b.agent, b.model.as_deref()));
    if call.no_tools || !holds {
        return;
    }
    let group = (b.agent.clone(), b.model.clone());
    if let (Some(all), Some(own)) = (shared.tools.get(&group), b.tools.as_ref())
        && all != own
    {
        options.tools = Some(all.clone());
        options.allowed_tools = Some(own.clone());
    }
}

/// What a call given more tools than its own is told, at the end of its
/// brief.
fn tools_note(own: &[String]) -> String {
    format!(
        "\n\nThe tools you may use: {}. You're shown others, which other roles use: a call \
         to one of them is refused.",
        if own.is_empty() {
            "none".to_string()
        } else {
            own.join(", ")
        }
    )
}

/// The prompt cache's key for a run's calls to one bridge and model: the
/// run, and a short hash of the two, which a header can carry.
fn cache_key(run: &str, agent: &str, model: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(format!("{agent}\n{}", model.unwrap_or_default()));
    let short: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let run: String = run
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .take(100)
        .collect();
    format!("acr-{run}-{short}")
}

pub(crate) fn options_for(b: &config::Binding) -> RunOptions {
    RunOptions {
        // The binding's tools, writing ones too: some headless agents may
        // use only the tools they're named, and refuse a write they would
        // otherwise have to ask about. Without a list, an Orchestrator's
        // fixes get what its bridge gives.
        tools: b.tools.clone(),
        session: Some("fresh".into()),
        timeout: b.timeout,
        workdir: None,
        model: b.model.clone(),
        effort: b.effort.clone(),
        log: None,
        // A review runs on one machine, which the bridge checks too.
        review: Some(true),
        facts: None,
        cache_key: None,
        allowed_tools: None,
        relay_tokens: None,
        relay_after_edit: None,
        // The host's MCP servers it names, and none it doesn't.
        mcp: b.mcp.clone(),
    }
}

/// Each binding in a user's ACR config, with what a call on it asks its
/// bridge for.
#[cfg(test)]
pub(crate) fn bindings_options(text: &str) -> Result<Vec<(String, RunOptions)>> {
    Ok(config::parse_user(text)?
        .allowed_bindings
        .iter()
        .map(|(name, b)| (name.clone(), options_for(b)))
        .collect())
}

/// Every agent a review calls is on this machine, and its bridge takes what
/// each binding asks for, working in `repo`. A binding its bridge refuses
/// would fail every call it made, found only once a review was under way.
/// `saved`: the review's bindings are the ones it was saved with.
fn check_machine(paths: &Paths, cfg: &Config, repo: &Path, saved: bool) -> Result<()> {
    check_on(paths, cfg, &this_machine(paths)?, repo, saved)
}

/// [`check_machine`], on a machine already named.
fn check_on(paths: &Paths, cfg: &Config, machine: &str, repo: &Path, saved: bool) -> Result<()> {
    cfg.check_local(machine)?;
    check_bridges(paths, &cfg.chosen_bindings(), machine, repo, saved)
}

/// Each binding on `machine` against its bridge there, by the check a
/// request meets, with its working directory: `workdir`, where the roles
/// work. The copies reviewers work in are allowed when a review starts.
fn check_bridges(
    paths: &Paths,
    bindings: &[(&str, &config::Binding)],
    machine: &str,
    workdir: &Path,
    saved: bool,
) -> Result<()> {
    let workdir = workdir
        .canonicalize()
        .with_context(|| format!("finding {}", workdir.display()))?;
    for (name, b) in bindings {
        let Some((agent, on)) = b.agent.split_once('@') else {
            continue;
        };
        // Another machine's binding is `check_local`'s to refuse.
        if !on
            .split('.')
            .next()
            .unwrap_or(on)
            .eq_ignore_ascii_case(machine)
        {
            continue;
        }
        let options = RunOptions {
            workdir: Some(workdir.display().to_string()),
            ..options_for(b)
        };
        let refused = crate::bridge::would_refuse(paths, agent, &options)
            .with_context(|| format!("checking binding {name} against the {agent} bridge"))?;
        let Some(why) = refused else {
            continue;
        };
        let fix = if saved {
            format!(
                "This review was saved with that binding, so changing it in {} can't help: \
                 allow it in [bridges.{agent}.options] in {} and restart the bridge, or end \
                 the review with --stop",
                user_config_path(paths).display(),
                paths.config.display()
            )
        } else {
            format!(
                "Allow it in [bridges.{agent}.options] in {}, then restart the bridge, or \
                 change the binding in {}",
                paths.config.display(),
                user_config_path(paths).display()
            )
        };
        bail!("the {agent} bridge would refuse every call on binding {name}: {why}. {fix}");
    }
    Ok(())
}

/// What `acr doctor` says of one check.
#[derive(Debug, PartialEq)]
enum Said {
    Ok(String),
    Note(String),
    Fix(String),
}

/// `acr doctor`: prints each check, and returns whether nothing needs a fix.
fn doctor(paths: &Paths, repo: &Path, with_tests: bool) -> Result<bool> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("finding {}", repo.display()))?;
    println!("{APP_NAME} acr doctor: {}", repo.display());
    let mut fixes = 0;
    for said in diagnose(paths, &repo, with_tests) {
        match said {
            Said::Ok(text) => println!("  ok    {text}"),
            Said::Note(text) => println!("  note  {text}"),
            Said::Fix(text) => {
                fixes += 1;
                println!("  fix   {text}");
            }
        }
    }
    match fixes {
        0 => println!(
            "Ready: `{APP_NAME} acr plan --repo {} --base <ref>` shows a review before it runs.",
            repo.display()
        ),
        n => println!("{n} to fix. Run `{APP_NAME} acr doctor` again after."),
    }
    Ok(fixes == 0)
}

/// Each of `acr doctor`'s checks, in order, stopping where the rest can't
/// be checked.
fn diagnose(paths: &Paths, repo: &Path, with_tests: bool) -> Vec<Said> {
    let mut out = Vec::new();
    let shown = repo.display();
    // The bus, and the machine it says this is.
    let machine = match this_machine(paths) {
        Ok(m) => {
            out.push(Said::Ok(format!("the bus answers, on {m}")));
            Some(m)
        }
        Err(e) => {
            out.push(Said::Fix(format!(
                "the bus doesn't answer ({e:#}): start it with `{APP_NAME} service install`"
            )));
            None
        }
    };
    out.extend(copies_note(paths).map(Said::Note));
    // The user's bindings.
    let user_path = user_config_path(paths);
    if !user_path.exists() {
        out.push(Said::Fix(format!(
            "there's no {}, which names the models a review uses: `{APP_NAME} setup acr` writes \
             one, with its bridge and token",
            user_path.display()
        )));
        return out;
    }
    let user = match config::load_user(&user_path) {
        Ok(u) => u,
        Err(e) => {
            out.push(Said::Fix(format!("{e:#}")));
            return out;
        }
    };
    // The repository is one the reviewers' bridge may work in.
    let reviewers = user
        .defaults
        .as_ref()
        .and_then(|d| d.bindings.get(&Role::Reviewer))
        .and_then(|name| user.allowed_bindings.get(name))
        .and_then(|b| b.agent.split_once('@').map(|(a, _)| a.to_string()));
    if let Some(agent) = &reviewers
        && let Ok(settings) = crate::bridge::load(paths, agent)
    {
        let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let allowed = std::iter::once(&settings.working_directory)
            .chain(&settings.options.workdirs)
            .any(|dir| repo.starts_with(real(dir)));
        out.push(match allowed {
            true => Said::Ok(format!("the {agent} bridge may work in it")),
            false => Said::Fix(format!(
                "the {agent} bridge may not work in it: `{APP_NAME} acr add-repo {shown}`"
            )),
        });
    }
    // The repository's review config.
    let (text, source) = match repo_layer(paths, repo, Some("HEAD"), None) {
        Ok(found) => found,
        Err(e) => {
            out.push(Said::Fix(format!("{e:#}")));
            return out;
        }
    };
    let cfg = match config::parse_repo(&text)
        .and_then(|p| config::resolve(&user, p, &Flags::default()))
        .and_then(|c| c.check_bindings().map(|_| c))
    {
        Ok(cfg) => cfg,
        Err(e) => {
            out.push(Said::Fix(format!(
                "its review config doesn't resolve: {e:#}"
            )));
            return out;
        }
    };
    let from = match &source {
        Source::File(f) => f.display().to_string(),
        Source::Base => format!("{REPO_CONFIG} on HEAD"),
    };
    out.push(Said::Ok(format!(
        "its review config, {from}: {} tiers, {} lenses",
        cfg.tiers.len(),
        cfg.lenses.len()
    )));
    if cfg.probes_off {
        out.push(Said::Note(config::PROBES_OFF.to_string()));
    }
    out.extend(cfg.time_notes().into_iter().map(Said::Note));
    // Every binding against its bridge, working here.
    if let Some(machine) = &machine {
        match check_on(paths, &cfg, machine, repo, false) {
            Ok(()) => out.push(Said::Ok(format!(
                "every binding is on {machine}, and its bridge takes what it asks for, here"
            ))),
            Err(e) => {
                let e = format!("{e:#}");
                let add = if e.contains("working directory") {
                    format!(
                        " If the bridge doesn't allow this repository: `{APP_NAME} acr add-repo {shown}`."
                    )
                } else {
                    String::new()
                };
                out.push(Said::Fix(format!("{e}.{add}")));
            }
        }
    }
    // How long its calls may run, and when a stalled one ends: what a
    // person needs to judge a long call by.
    let mut bridges: Vec<&str> = cfg
        .chosen_bindings()
        .iter()
        .filter_map(|(_, b)| b.agent.split_once('@'))
        .map(|(bridge, _)| bridge)
        .collect();
    bridges.sort_unstable();
    bridges.dedup();
    for bridge in bridges {
        if let (Ok(set), Some(longest)) = (
            crate::bridge::load(paths, bridge),
            crate::bridge::longest(paths, bridge),
        ) {
            let took = |secs: u64| progress::took(std::time::Duration::from_secs(secs));
            let stalls = if set.idle_secs == 0 {
                "never ends one for going quiet".to_string()
            } else {
                format!("ends one that's quiet for {}", took(set.idle_secs))
            };
            out.push(Said::Ok(format!(
                "the {bridge} bridge lets a review's call run up to {}, and {stalls}",
                took(longest)
            )));
        }
        if let Ok(set) = crate::bridge::load(paths, bridge) {
            out.extend(doctor_kind(paths, bridge, &set));
            out.push(doctor_prompt(paths, bridge, &set));
        }
    }
    // Calibration.
    let decided = not_findings(&cfg, &source, repo, "HEAD").unwrap_or_default();
    match (change::name(repo), fingerprint(&cfg, &text, &decided)) {
        (Ok(name), Ok(now)) => {
            let record = calibration::load(&paths.state, &name).unwrap_or_default();
            match calibration::standing(&record, &now) {
                calibration::Standing::Calibrated => out.push(Said::Ok(format!("{name} is calibrated"))),
                calibration::Standing::JudgeChanged => out.push(Said::Note(format!(
                    "{name} is calibrated, but on an earlier Judge: its binding, prompt or \
                     rules have changed since its config was accepted. {} `{APP_NAME} acr \
                     calibration --repo {shown} --accept` when you've read its config again",
                    calibration::ON_AN_EARLIER_JUDGE
                ))),
                calibration::Standing::ConfigChanged | calibration::Standing::Never => {
                    out.push(Said::Fix(format!(
                        "{name} isn't calibrated: read its config, then `{APP_NAME} acr calibration \
                         --repo {shown} --accept`"
                    )))
                }
            }
        }
        (Err(e), _) | (_, Err(e)) => out.push(Said::Fix(format!("{e:#}"))),
    }
    // ACR's own Claude Code bridge: its token signs in.
    let mut agents: Vec<&str> = cfg
        .chosen_bindings()
        .iter()
        .filter_map(|(_, b)| b.agent.split_once('@').map(|(a, _)| a))
        .collect();
    agents.dedup();
    for agent in agents {
        if agent != crate::setup::ACR_AGENT {
            continue;
        }
        let Ok(settings) = crate::bridge::load(paths, agent) else {
            continue;
        };
        if let Some(token) = settings.secrets.get("CLAUDE_CODE_OAUTH_TOKEN") {
            match crate::setup::check_token(&settings.command, token) {
                Ok(()) => out.push(Said::Ok(format!("the {agent} bridge's token signs in"))),
                Err(e) => out.push(Said::Fix(format!(
                    "the {agent} bridge's token doesn't sign in ({e:#}): `rm {}` and \
                     `{APP_NAME} setup acr` for a new one",
                    token.display()
                ))),
            }
        }
    }
    out.extend(doctor_toolchain(paths, &cfg, repo));
    if with_tests {
        out.extend(doctor_tests(paths, &cfg, repo));
    }
    out
}

/// The reviewers' bridge, if they run things: only then do their own
/// commands need a toolchain.
fn reviewers_bridge(paths: &Paths, cfg: &Config) -> Option<(String, crate::bridge::Settings)> {
    let b = cfg
        .binding(Role::Reviewer, None)
        .ok()
        .filter(|b| b.changes_files())?;
    let agent = b.agent.split_once('@')?.0.to_string();
    let settings = crate::bridge::load(paths, &agent).ok()?;
    Some((agent, settings))
}

/// The environment a bridge's runs get, as near as a check can make it:
/// its HOME, PATH and env, and the few variables it passes on.
fn bridge_env(settings: &crate::bridge::Settings) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), settings.home.display().to_string()),
        ("PATH".into(), settings.path.clone()),
        ("TMPDIR".into(), std::env::temp_dir().display().to_string()),
    ];
    for key in ["USER", "LOGNAME", "LANG", "SHELL"] {
        if let Ok(v) = std::env::var(key) {
            env.push((key.into(), v));
        }
    }
    env.extend(
        settings
            .env
            .iter()
            .filter(|(_, v)| !v.contains('{'))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env
}

/// Whether `command` succeeds in `dir` with only `env`.
fn runs_with(dir: &Path, command: &str, env: &[(String, String)]) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The toolchain a repository's own files call for, and how to see that it
/// runs: what reviewers who run things will reach for.
const TOOLCHAIN_OF: [(&str, &str, &str); 8] = [
    ("Cargo.toml", "cargo", "cargo --version"),
    ("go.mod", "go", "go version"),
    ("package.json", "node", "node --version"),
    ("pubspec.yaml", "flutter", "command -v flutter"),
    ("pyproject.toml", "python3", "python3 --version"),
    ("requirements.txt", "python3", "python3 --version"),
    ("pom.xml", "mvn", "command -v mvn"),
    ("build.gradle", "gradle", "command -v gradle"),
];

/// Whether reviewers who run things find the repository's toolchain, as
/// their bridge runs them: tests that pass for you can't run for them
/// without it.
/// What a bridge takes from acrbus's defaults for its kind, and what from
/// its own table; or, for one written out in full whose agent acrbus knows,
/// that naming its kind would let the table say only what's its own.
fn doctor_kind(paths: &Paths, bridge: &str, set: &crate::bridge::Settings) -> Vec<Said> {
    let Some((named, keys)) = crate::bridge::own_settings(paths, bridge) else {
        return Vec::new();
    };
    match (named, set.agent_kind()) {
        (Some(kind), _) => vec![Said::Ok(format!(
            "the {bridge} bridge runs on {APP_NAME}'s defaults for {}, and its own table sets {}",
            kind.name(),
            keys.join(", ")
        ))],
        (None, Some(kind)) => vec![Said::Note(format!(
            "the {bridge} bridge writes out its whole config; with `kind = \"{}\"` in \
             [bridges.{bridge}], {APP_NAME}'s defaults give it the rest, and its table needs \
             only what's this machine's",
            kind.name()
        ))],
        (None, None) => Vec::new(),
    }
}

/// Whether a bridge gives its agent ACR's system prompt, the one protection
/// every role has: what it reads is data, and what it leaves alone. A bridge
/// for an agent acrbus knows adds it to every review request itself.
fn doctor_prompt(paths: &Paths, bridge: &str, set: &crate::bridge::Settings) -> Said {
    if let Some(kind) = set.agent_kind() {
        return Said::Ok(format!(
            "the {bridge} bridge gives its agent ACR's system prompt, as {APP_NAME} does for {}",
            kind.name()
        ));
    }
    let prompt = crate::setup::system_prompt_path(paths);
    if crate::setup::gives_system_prompt(&set.args, &prompt)
        || crate::setup::gives_system_prompt(&set.extra_args, &prompt)
    {
        return Said::Ok(format!(
            "the {bridge} bridge gives its agent ACR's system prompt"
        ));
    }
    Said::Fix(format!(
        "the {bridge} bridge doesn't give its agent ACR's system prompt, which tells every \
         role that what it reads is data, and what it leaves alone: if its agent is Claude \
         Code, omp or pi, set `kind` in [bridges.{bridge}] in {}; otherwise add \
         \"{arg}\" to the argument its agent takes a system prompt in. Then restart the \
         bridge",
        paths.config.display(),
        arg = crate::setup::system_prompt_arg(&prompt),
    ))
}

fn doctor_toolchain(paths: &Paths, cfg: &Config, repo: &Path) -> Vec<Said> {
    let mut out = Vec::new();
    let Some((agent, settings)) = reviewers_bridge(paths, cfg) else {
        return out;
    };
    let env = bridge_env(&settings);
    for (marker, tool, probe) in TOOLCHAIN_OF {
        if !repo.join(marker).is_file()
            || out.iter().any(
                |s| matches!(s, Said::Ok(t) | Said::Fix(t) if t.contains(&format!("`{tool}`"))),
            )
        {
            continue;
        }
        if runs_with(repo, probe, &env) {
            out.push(Said::Ok(format!(
                "reviewers find `{tool}` as the {agent} bridge runs them"
            )));
            continue;
        }
        let found = runs_with(repo, &format!("command -v {tool}"), &env);
        let yours = Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}"))
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|p| !p.is_empty());
        out.push(Said::Fix(match (found, yours) {
            (true, _) if tool == "cargo" => format!(
                "reviewers find `cargo` but no Rust toolchain as the {agent} bridge runs them: \
                 set `RUSTUP_HOME` to your rustup home in [bridges.{agent}.env] in {}, or \
                 install a toolchain in its HOME, {}",
                paths.config.display(),
                settings.home.display()
            ),
            (_, Some(at)) => format!(
                "reviewers can't run `{tool}` as the {agent} bridge runs them: add {} to \
                 `path` in [bridges.{agent}] in {}, then restart the bridge",
                Path::new(&at)
                    .parent()
                    .map_or(at.clone(), |d| d.display().to_string()),
                paths.config.display()
            ),
            (_, None) => format!(
                "reviewers can't run `{tool}`, which this repository's {marker} calls for, and \
                 neither can you: install it, and put its folder in `path` in \
                 [bridges.{agent}] in {}",
                paths.config.display()
            ),
        }));
    }
    out
}

/// The review config's test commands, run in a copy of the repository as a
/// review runs them, and as its reviewers would, with their bridge's HOME
/// and PATH: tests that pass for you can fail for them.
fn doctor_tests(paths: &Paths, cfg: &Config, repo: &Path) -> Vec<Said> {
    let mut out = Vec::new();
    let tests = &cfg.tests;
    let Some(command) = tests.command.clone().or_else(|| stages::test_command(repo)) else {
        out.push(Said::Note(
            "no test command, and none could be found: set tests.command in the review config, \
             or reviews can't run the tests"
                .into(),
        ));
        return out;
    };
    let root = copies_root(paths).join(crate::util::stamped(&format!(
        "doctor-{}",
        crate::util::new_id()
    )));
    let copy = root.join(repo.file_name().unwrap_or_default());
    let limit = std::time::Duration::from_secs(tests.timeout.unwrap_or(900));
    if let Err(e) = change::copy_of(repo, &copy) {
        out.push(Said::Fix(format!(
            "couldn't copy the repository to run its tests: {e:#}"
        )));
        return out;
    }
    let ran = |what: &str, cmd: &str, env: Option<&[(String, String)]>| {
        let log = root.join(format!("{what}.log"));
        let result = stages::tests_pass_in(&copy, cmd, &[], limit, &log, env);
        (result, log)
    };
    let mut steps: Vec<(&str, String)> = Vec::new();
    if let Some(setup) = &tests.setup {
        steps.push(("setup", setup.clone()));
    }
    steps.push(("command", command.clone()));
    if let Some(coverage) = &tests.coverage {
        steps.push(("coverage", coverage.clone()));
    }
    let mut all = true;
    for (what, cmd) in &steps {
        let (result, log) = ran(what, cmd, None);
        match result {
            Ok(Some(true)) => out.push(Said::Ok(format!(
                "tests.{what} runs in a copy, as a review runs it"
            ))),
            Ok(Some(false)) => {
                all = false;
                out.push(Said::Fix(format!(
                    "tests.{what} fails in a copy of the repository, as a review runs it: see {}",
                    log.display()
                )));
            }
            Ok(None) => {
                all = false;
                out.push(Said::Fix(format!(
                    "tests.{what} ran past its {} s limit: raise tests.timeout, or make it faster \
                     ({})",
                    limit.as_secs(),
                    log.display()
                )));
            }
            Err(e) => {
                all = false;
                out.push(Said::Fix(format!("tests.{what} couldn't run: {e:#}")));
            }
        }
        if !all {
            break;
        }
    }
    // As reviewers run it: only those that can run things do.
    if all && let Some((agent, settings)) = reviewers_bridge(paths, cfg) {
        let env = bridge_env(&settings);
        // A Rust repository's crates, in the cache their runs use.
        if repo.join("Cargo.toml").is_file()
            && runs_with(&copy, "cargo --version", &env)
            && !runs_with(&copy, "cargo fetch --offline --locked", &env)
        {
            out.push(Said::Fix(format!(
                "the {agent} bridge's crate cache lacks this repository's crates, so reviewers \
                 can't build it offline: from the repository, once, with the network, `HOME={} \
                 cargo fetch --locked`",
                settings.home.display()
            )));
        }
        let (result, log) = ran("as-reviewers", &command, Some(&env));
        match result {
            Ok(Some(true)) => out.push(Said::Ok(format!(
                "the tests run as the {agent} bridge's reviewers would, with its HOME and PATH"
            ))),
            _ => out.push(Said::Fix(format!(
                "the tests pass for you but not as the {agent} bridge's reviewers would run them, \
                 with HOME {} and PATH {}: what they need, a toolchain or a filled cache, isn't \
                 there. Fill it in that HOME, or have the runbook and tests.command set it \
                 ({})",
                settings.home.display(),
                settings.path,
                log.display()
            ))),
        }
    }
    // The copy and its logs stay, to be looked at, until the daemon's cleanup.
    out
}

/// This machine's name on the tailnet, as the bus knows it.
fn this_machine(paths: &Paths) -> Result<String> {
    let status: crate::local::Status = crate::local::call(&paths.socket(), &Request::Status)
        .context("asking the bus which machine this is")?;
    status
        .machine
        .context("the bus doesn't know this machine's name yet")
}

fn reload(cfg: &Config, repo: &Path, base: &str, head: Option<&str>) -> Result<Change> {
    change::load(repo, base, head, cfg.review_scope, &cfg.generated)
}

/// Reaches models through bridges, as `acrbus call` does. Each call is a
/// fresh session, limited to its binding's tools.
struct BridgeCaller {
    /// The review's spend cap, set as it runs.
    cap: std::sync::Mutex<Option<config::SpendCap>>,
    socket: PathBuf,
    agent: String,
    store: Attachments,
    /// Attachments this put in the store, or got back there, to remove when
    /// it's done: a review leaves nothing behind.
    made: std::sync::Mutex<Vec<String>>,
    /// Asks each bridge to keep its run's log and transcripts, which a
    /// review otherwise leaves none of: for a trial, to see what each
    /// reviewer did.
    keep_logs: bool,
    /// Where the bridges' settings are, for how long a call may run.
    paths: Paths,
    /// What a person following the review is told as it goes.
    progress: progress::Progress,
    /// Each role's share of the most the plan allows, for one call, to warn
    /// of a call that passes it.
    shares: std::sync::Mutex<Vec<(Role, u64)>>,
    /// The requests sent and not yet answered, and the file they're kept
    /// in, so that stopping the review stops them too, even if this
    /// process is gone by then.
    calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    calls_file: std::sync::Mutex<Option<PathBuf>>,
    /// The round's shared facts, for its reviewers and Judges.
    shared: std::sync::Mutex<Option<flow::Shared>>,
    /// By label, each call's bridge and request once sent, and whether it
    /// has been answered: to tell when a fan-out's first has filled the
    /// prompt cache.
    sent: std::sync::Mutex<std::collections::HashMap<String, (String, String, bool)>>,
    /// Agents asked to relay that can't be steered mid-call, said once each.
    unrelayed: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Calls, by label, restarted after their model couldn't be reached:
    /// they go with a new cache key.
    rekeyed: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl Drop for BridgeCaller {
    fn drop(&mut self) {
        let made = std::mem::take(&mut *self.made.lock().unwrap_or_else(|e| e.into_inner()));
        for sha in made {
            let _ = fs::remove_file(self.store.path(&sha));
        }
    }
}

impl BridgeCaller {
    fn new(paths: &Paths) -> Result<BridgeCaller> {
        let bus = BusConfig::load(&paths.config)?;
        Ok(BridgeCaller {
            socket: paths.socket(),
            // Its own name, never a session's: what it sends and gets back
            // is the review's, and no session should see it or act on it.
            agent: SENDER.to_string(),
            store: Attachments::new(
                paths.attachments(),
                bus.max_attachment_bytes,
                bus.attachment_store_bytes,
            ),
            made: std::sync::Mutex::default(),
            keep_logs: false,
            paths: Paths {
                state: paths.state.clone(),
                config: paths.config.clone(),
            },
            progress: progress::Progress::new(paths),
            shares: Default::default(),
            calls: Default::default(),
            calls_file: std::sync::Mutex::new(None),
            shared: std::sync::Mutex::new(None),
            sent: Default::default(),
            unrelayed: Default::default(),
            rekeyed: Default::default(),
            cap: Default::default(),
        })
    }

    /// The plan's estimate for this review of `change`, for its progress to
    /// say the run's total against, and each call's share of the most it
    /// allows: one call's estimate for its role, scaled as the most scales
    /// the least. `spent`: what the review used before, when resumed.
    fn budget(
        &self,
        paths: &Paths,
        cfg: &Config,
        change: &Change,
        repo: &Path,
        before: &[flow::Usage],
    ) {
        // A call that logs no new turn for this long is warned of: the
        // repository's setting, 0 for never.
        let stall = cfg.sessions.stall_warn_secs;
        self.progress
            .stall_after((stall > 0).then(|| std::time::Duration::from_secs(stall)));
        let Ok(plan) = plan::dry_run(cfg, change, &history(paths, repo)) else {
            return;
        };
        // Tokens not read from a cache, as the plan estimates them, on both
        // sides: an agent's cache reads can be many times the rest.
        let scale = plan.most.fresh as f64 / plan.least.fresh.max(1) as f64;
        let shares = plan
            .first_round
            .iter()
            .chain(plan.fix.iter())
            .filter(|c| c.calls > 0)
            .map(|c| {
                let one = c.fresh() as f64 / c.calls as f64;
                (c.role, (one * scale).ceil() as u64)
            })
            .collect();
        let spent = progress::Spent {
            fresh: before
                .iter()
                .map(|u| u.tokens_in + u.cache_write + u.tokens_out)
                .sum(),
            cached: before.iter().map(|u| u.cache_read).sum(),
            usd: before.iter().filter_map(|u| u.usd).sum(),
        };
        *self.shares.lock().unwrap_or_else(|e| e.into_inner()) = shares;
        let budget = progress::Budget {
            expected: plan.expected.map(|e| e.fresh),
            most: plan.most.fresh,
        };
        self.progress.budget(budget, spent);
        let expected = budget
            .expected
            .map(|e| format!("about {} tokens expected, ", progress::short(e)))
            .unwrap_or_default();
        self.progress.line(&format!(
            "the plan: {expected}{} at most, not counting tokens read from a cache",
            progress::short(budget.most)
        ));
    }

    fn share_of(&self, role: Role) -> Option<u64> {
        self.shares
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, share)| *share)
    }

    /// Keeps the requests in flight in `path`, and on a signal to stop,
    /// cancels them on their bridges before it exits.
    fn stops_calls(&self, path: PathBuf) {
        *self.calls_file.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
        let (socket, agent, calls) = (
            self.socket.clone(),
            self.agent.clone(),
            std::sync::Arc::clone(&self.calls),
        );
        let watched = crate::bridge::on_stop_signal(move |_| {
            let ids = calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for id in &ids {
                let _ = crate::call::cancel_run(&socket, &agent, id);
            }
            eprintln!(
                "{APP_NAME}: stopped, and cancelled {} call(s) still at work.",
                ids.len()
            );
            std::process::exit(130);
        });
        if let Err(e) = watched {
            eprintln!("{APP_NAME}: stopping this review won't stop its calls: {e:#}");
        }
    }

    /// A call answered: kept at once, a line in the run's folder beside
    /// the calls in flight, so a run stopped before its round ends still
    /// counts what that round's finished calls used.
    fn answered(&self, call: &Call, answer: &flow::Answer) {
        let Some(dir) = self
            .calls_file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|f| f.parent().map(Path::to_path_buf))
        else {
            return;
        };
        let kept = flow::Answered {
            id: answer.run_id.clone(),
            role: call.role,
            binding: call.binding_name.to_string(),
            agent: call.binding.agent.clone(),
            model: call.binding.model.clone(),
            subscription: call.binding.subscription,
            label: call.label.clone(),
            secs: answer.secs,
            usage: answer.usage.clone(),
        };
        let Ok(line) = serde_json::to_string(&kept) else {
            return;
        };
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(flow::ANSWERED))
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// A request sent, or answered: the list in flight, and its file.
    fn in_flight(&self, id: &str, sent: bool) {
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner());
        if sent {
            calls.push(id.to_string());
        } else {
            calls.retain(|c| c != id);
        }
        if let Some(path) = self
            .calls_file
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = fs::write(path, calls.join("\n"));
        }
    }

    /// How long a call on `b` asks its bridge to let it run: its binding's
    /// cap, or else the longest the bridge allows, never the bridge's
    /// default, which may be minutes.
    fn asks_for(&self, b: &config::Binding, bridge: &str) -> Option<u64> {
        b.timeout
            .or_else(|| crate::bridge::longest(&self.paths, bridge))
    }

    /// Writes the review's progress to `path` as well as to stderr.
    fn log_to(&self, path: PathBuf) {
        self.progress.log_to(path);
    }

    /// Adds an attachment, noting it to remove later if it wasn't there
    /// already, since something else might need one that was.
    fn attach(&self, name: &str, bytes: &[u8]) -> Result<crate::proto::Attachment> {
        use sha2::{Digest, Sha256};
        let sha: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let there = self.store.path(&sha).exists();
        let a = self.store.add(name, &mut &bytes[..], bytes.len() as u64)?;
        if !there {
            self.noted(&a.sha256);
        }
        Ok(a)
    }

    /// Waits for the reply to `id` again, on a new connection to the daemon,
    /// giving it a minute to come back.
    fn await_again(&self, id: &str, wait: u64) -> Result<Awaited> {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let tried = Client::connect(&self.socket).and_then(|mut client| {
                client.send(&Request::Await {
                    agent: self.agent.clone(),
                    id: id.to_string(),
                    wait_secs: wait,
                })?;
                client.reply::<Awaited>()
            });
            match tried {
                Ok(awaited) => return Ok(awaited),
                Err(e) if std::time::Instant::now() >= until => {
                    return Err(e)
                        .with_context(|| format!("the {APP_NAME} daemon went away mid-call"));
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
            }
        }
    }

    fn noted(&self, sha: &str) {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        made.push(sha.to_string());
    }
}

impl Caller for BridgeCaller {
    fn fingerprint(&self, repo: &Path) -> Option<String> {
        change::fingerprint(repo).ok()
    }

    fn snapshot(&self, repo: &Path) -> Option<String> {
        change::worktree_tree(repo).ok()
    }

    fn delta(&self, repo: &Path, from: &str, to: &str) -> Option<(Vec<String>, String)> {
        change::tree_diff(repo, from, to).ok()
    }

    fn changed(&self, repo: &Path, from: &str, to: &str) -> Option<Vec<change::Changed>> {
        change::tree_changes(repo, from, to).ok()
    }

    fn revert(&self, repo: &Path, from: &str, path: &str, existed: bool) -> Result<()> {
        change::revert(repo, from, path, existed)
    }

    fn note(&self, what: &str) {
        self.progress.note(what);
    }

    fn say(&self, what: &str) {
        self.progress.line(what);
    }

    fn share(&self, shared: Option<flow::Shared>) -> bool {
        // Only if every agent the round's calls go to takes them: one that
        // didn't would lose what the calls leave out for them.
        let shared = shared.filter(|sh| {
            let bytes = fs::metadata(&sh.facts).map_or(u64::MAX, |m| m.len());
            sh.agents.iter().all(|a| {
                let bridge = a.split('@').next().unwrap_or_default();
                crate::bridge::takes_facts(&self.paths, bridge, bytes)
            })
        });
        let given = shared.is_some();
        *self.shared.lock().unwrap_or_else(|e| e.into_inner()) = shared;
        given
    }

    fn warmed(&self, label: &str) -> Option<bool> {
        let sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        let Some((bridge, id, answered)) = sent.get(label).cloned() else {
            return Some(false);
        };
        drop(sent);
        if answered {
            return Some(true);
        }
        // Its agent records a turn's tokens once the turn is done, and acrbus's
        // pi extension logs it as it ends: by then, the provider has cached
        // what it read.
        Some(matches!(
            crate::bridge::standing(&self.paths, &bridge, &id),
            crate::bridge::Standing::Running { tokens: Some(t), .. }
                if t.turns > 0 || t.input + t.cache_read + t.cache_write > 0
        ))
    }

    fn rekey(&self, label: &str) {
        self.rekeyed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(label.to_string());
    }

    fn over_cap(&self) -> Option<String> {
        let cap = (*self.cap.lock().unwrap_or_else(|e| e.into_inner()))?;
        let spent = self.progress.spent();
        cap.reached(spent.usd, spent.fresh + spent.cached)
    }

    fn set_cap(&self, cap: Option<config::SpendCap>) {
        *self.cap.lock().unwrap_or_else(|e| e.into_inner()) = cap;
    }

    fn has_room(&self, binding: &config::Binding) -> bool {
        let bridge = binding.agent.split('@').next().unwrap_or_default();
        // One slot always left over for the review's own calls.
        crate::bridge::has_room(&self.paths, bridge, 1)
    }

    fn call(&self, call: &Call) -> Result<Answer> {
        let bridge = call.binding.agent.split('@').next().unwrap_or_default();
        let share = self.share_of(call.role);
        let token = self
            .progress
            .start(&call.label, call.binding_name, bridge, share);
        let answer = self.call_once(call, bridge, token);
        if let Ok(a) = &answer {
            self.answered(call, a);
        }
        let failed = answer.as_ref().err().map(|e| format!("{e:#}"));
        let used = answer
            .as_ref()
            .ok()
            .and_then(|a| a.usage.as_ref())
            .map(crate::bridge::LiveTokens::of);
        self.progress.end(token, failed.as_deref(), used);
        answer
    }
}

/// The exit code acrbus's pi extension ends a call with when its model
/// requests keep getting no response.
const NO_RESPONSE_EXIT: i32 = 75;

/// What an agent says when it couldn't reach its model: a connection
/// refused, reset or timed out, or a name that didn't resolve.
fn unreachable_said(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "econnrefused",
        "econnreset",
        "enotfound",
        "eai_again",
        "etimedout",
        "ehostunreach",
        "enetunreach",
        "fetch failed",
        "connection error",
        "connection refused",
        "connection reset",
        "network is unreachable",
        "socket hang up",
        "getaddrinfo",
    ]
    .iter()
    .any(|m| text.contains(m))
}

impl BridgeCaller {
    fn call_once(&self, call: &Call, bridge: &str, token: progress::Token) -> Result<Answer> {
        // A label an earlier round used is this call's now.
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&call.label);
        let b = call.binding;
        // Fixes only from a binding the user lets write.
        if call.writes && !b.writes {
            bail!(
                "the {} binding can't apply fixes: give it `writes: true` in your acr.yaml",
                call.binding_name
            );
        }
        let mut options = call_options(call, self.keep_logs, self.asks_for(b, bridge));
        if let Some(shared) = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let holds = crate::bridge::holds_tools(&self.paths, bridge);
            with_shared(&mut options, call, shared, holds);
        }
        // A restart after its model couldn't be reached: a new key, so it
        // may land on another replica than the one that didn't answer.
        if self
            .rekeyed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&call.label)
            && let Some(key) = options.cache_key.as_mut()
        {
            key.push_str("-fresh");
        }
        let mut attachments = Vec::new();
        if let Some(path) = call.artifact {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "change.diff".into());
            let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            attachments.push(self.attach(&name, &bytes)?);
        }
        let mut body = call.prompt.to_string();
        // Given every tool its bridge's roles are, so their prompts share a
        // prefix: told which are its own, last, and held to them.
        if let Some(own) = &options.allowed_tools {
            body.push_str(&tools_note(own));
        }
        if body.len() > MAX_BODY {
            attachments.push(self.attach("brief.txt", body.as_bytes())?);
            body = "Your brief is in the attached file brief.txt.".to_string();
        }
        let ext = Extras {
            options: Some(options),
            attachments,
            ..Extras::default()
        };
        let wait = REPLY_WAIT;
        let asked = std::time::Instant::now();
        let unreachable = |e: anyhow::Error| {
            anyhow::Error::new(flow::CallFailed {
                failure: flow::Failure::Unreachable,
                message: format!("the bus couldn't be reached: {e:#}"),
            })
        };
        let mut client = Client::connect(&self.socket).map_err(unreachable)?;
        client
            .send(&Request::Call {
                agent: self.agent.clone(),
                to: b.agent.clone(),
                body,
                reply_to: None,
                expects_reply: Some(true),
                ext: Some(Box::new(ext)),
                wait_secs: Some(wait),
            })
            .map_err(unreachable)?;
        let sent: Sent = client.reply().map_err(unreachable)?;
        self.progress.sent(token, &sent.id);
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).insert(
            call.label.clone(),
            (bridge.to_string(), sent.id.clone(), false),
        );
        let label = call.label.clone();
        let _marked = OnDrop(|| {
            if let Some(entry) = self
                .sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&label)
            {
                entry.2 = true;
            }
        });
        self.in_flight(&sent.id, true);
        let request = sent.id.clone();
        let _answered = OnDrop(|| self.in_flight(&request, false));
        let awaited = match client.reply::<Awaited>() {
            Ok(awaited) => awaited,
            // The daemon went away mid-wait, as it does when it's updated.
            // The reply is kept for this agent, so wait for it again.
            Err(_) => {
                let left = wait.saturating_sub(asked.elapsed().as_secs()).max(1);
                self.await_again(&sent.id, left).map_err(unreachable)?
            }
        };
        match awaited {
            Awaited::Reply { item } => {
                let msg = &item.msg;
                let result = msg.result();
                let status = result.map_or(RunStatus::Ok, |r| r.status);
                if status != RunStatus::Ok {
                    let said = |text: &str| unreachable_said(text);
                    let failure = match status {
                        RunStatus::Timeout => flow::Failure::TimedOut,
                        RunStatus::Refused => flow::Failure::Refused,
                        RunStatus::Cancelled => flow::Failure::Cancelled,
                        _ if result.and_then(|r| r.exit_code) == Some(NO_RESPONSE_EXIT) => {
                            flow::Failure::NoResponse
                        }
                        _ if said(&msg.body)
                            || result
                                .and_then(|r| r.stderr_tail.as_deref())
                                .is_some_and(said) =>
                        {
                            flow::Failure::Unreachable
                        }
                        _ => flow::Failure::Failed,
                    };
                    let message = if status == RunStatus::Timeout {
                        format!(
                            "{} timeout: {}. It ran as long as {bridge} allows, so raise its \
                             timeout_secs and max_timeout_secs if a call needs longer",
                            b.agent, msg.body
                        )
                    } else {
                        format!("{} {}: {}", b.agent, status.as_str(), msg.body)
                    };
                    return Err(anyhow::Error::new(flow::CallFailed { failure, message }));
                }
                let whole = result
                    .and_then(|r| r.full_reply.as_ref())
                    .and_then(|sha| msg.attachments().iter().find(|a| &a.sha256 == sha));
                let text = match whole.and_then(|a| a.path.as_ref()) {
                    Some(path) => fs::read_to_string(path)?,
                    None => msg.body.clone(),
                };
                // The bridge made it for this reply alone.
                if let Some(a) = whole {
                    self.noted(&a.sha256);
                }
                // Asked to relay, on an agent its bridge can't steer: the
                // call ran whole, and a person following the review is told.
                if call.relay_tokens.is_some()
                    && result
                        .and_then(|r| r.applied.as_ref())
                        .is_some_and(|a| a.relay_tokens.is_none())
                    && self
                        .unrelayed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(b.agent.clone())
                {
                    self.progress.line(&format!(
                        "{}: its agent can't be steered mid-call, so its calls don't relay and \
                         run whole",
                        b.agent
                    ));
                }
                let usage = result.and_then(|r| r.usage.clone());
                // A tool its model reached for that its binding doesn't
                // name: said, not refused here, since pi's bridge refuses it
                // and others can't. It shows a gateway adding tools to its
                // requests, or a model making one up.
                if let Some(used) = &usage {
                    let unnamed = flow::unnamed_tools(b, &used.tools);
                    if !unnamed.is_empty() {
                        let list: Vec<String> = unnamed
                            .iter()
                            .map(|(name, n)| format!("{name} ({n}×)"))
                            .collect();
                        self.progress.line(&format!(
                            "{}: its model asked for tool(s) its binding doesn't name: {}",
                            call.label,
                            list.join(", ")
                        ));
                    }
                }
                Ok(Answer {
                    text,
                    run_id: sent.id,
                    usage,
                    secs: Some(asked.elapsed().as_secs()),
                })
            }
            Awaited::Undelivered { why } => Err(anyhow::Error::new(flow::CallFailed {
                failure: flow::Failure::Unreachable,
                message: format!("{}: {why}", b.agent),
            })),
            Awaited::Unanswered => Err(anyhow::Error::new(flow::CallFailed {
                failure: flow::Failure::Failed,
                message: format!("{} marked it handled without answering", b.agent),
            })),
            Awaited::TimedOut => Err(anyhow::Error::new(flow::CallFailed {
                failure: flow::Failure::TimedOut,
                message: format!("{} didn't answer in {wait} seconds", b.agent),
            })),
        }
    }
}

/// A review's record, from its folder while it's kept there, or from its
/// archive once it's finished and packed.
fn kept_run(paths: &Paths, id: &str) -> Result<Run> {
    let folder = run_dir(paths, id).join("run.json");
    let bytes = match fs::read(&folder) {
        Ok(bytes) => bytes,
        Err(_) => {
            let kept = keep::kept(paths, id).with_context(|| format!("no review {id}"))?;
            if kept.is_dir() {
                fs::read(kept.join("run.json"))?
            } else {
                let name = kept.file_name().unwrap_or_default().to_string_lossy();
                crate::util::packed_file(
                    &kept,
                    &format!("{}/run.json", name.trim_end_matches(".tar.gz")),
                )?
            }
        }
    };
    serde_json::from_slice(&bytes).with_context(|| format!("reading review {id}'s record"))
}

fn save(paths: &Paths, run: &Run, cfg: &Config) -> Result<()> {
    let dir = run_dir(paths, &run.id);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("run.json"),
        serde_json::to_vec_pretty(&report::record(run))?,
    )?;
    fs::write(dir.join("config.json"), serde_json::to_vec_pretty(cfg)?)?;
    let report = report::report(run, cfg);
    fs::write(dir.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    Ok(())
}

/// Puts a result on a pull request, as the repository's config says.
fn post(run: &Run, cfg: &Config, pr: u64) -> Result<()> {
    let number = pr.to_string();
    let wanted = report::labels(run, cfg);
    let stale: Vec<String> = report::all_labels(run, cfg)
        .into_iter()
        .filter(|l| !wanted.contains(l))
        .collect();
    let gh = |args: &[&str], input: Option<&str>| -> Result<()> {
        let mut command = Command::new("gh");
        command.args(args).current_dir(&run.repo);
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn().context("running gh")?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            use std::io::Write;
            stdin.write_all(text.as_bytes())?;
        }
        if !child.wait()?.success() {
            bail!("gh {} failed", args.join(" "));
        }
        Ok(())
    };
    if !wanted.is_empty() {
        let (add, remove) = (wanted.join(","), stale.join(","));
        let mut args = vec!["pr", "edit", number.as_str(), "--add-label", add.as_str()];
        if !stale.is_empty() {
            args.extend(["--remove-label", remove.as_str()]);
        }
        gh(&args, None)?;
    }
    if let Some(text) = report::comment(run, cfg) {
        gh(&["pr", "comment", &number, "--body-file", "-"], Some(&text))?;
    }
    Ok(())
}

/// A person's answers: the JSON itself, `@<file>`, or `-` for stdin.
fn read_answers(given: &str) -> Result<std::collections::BTreeMap<String, serde_json::Value>> {
    let text = match given {
        "-" => std::io::read_to_string(std::io::stdin()).context("reading the answers")?,
        file if file.starts_with('@') => fs::read_to_string(&file[1..])
            .with_context(|| format!("reading the answers from {}", &file[1..]))?,
        json => json.to_string(),
    };
    serde_json::from_str(&text).context(
        "the answers are a JSON object, each question's text mapped to its answer, as \
         AskUserQuestion gives them",
    )
}

/// Keeps a person's verdicts for the repository's calibration. A failure to
/// keep them is said, and the review goes on.
fn keep_feedback(paths: &Paths, run: &Run, feedback: &[configure::Feedback]) {
    if feedback.is_empty() {
        return;
    }
    let kept = change::name(&run.repo).and_then(|repo| {
        feedback
            .iter()
            .try_for_each(|f| configure::add_feedback(&paths.state, &repo, f))
    });
    match kept {
        Ok(()) => eprintln!(
            "{APP_NAME}: kept {} verdict(s) as feedback: `{APP_NAME} acr calibrate` folds them \
             into the config when you're ready.",
            feedback.len()
        ),
        Err(e) => eprintln!("{APP_NAME}: couldn't keep your verdicts as feedback: {e:#}"),
    }
}

/// Whether a person is at this terminal to ask.
fn at_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// While the review waits for a person at this terminal, asks them, and
/// carries on. A review run by an agent, or in the background, ends at the
/// gate instead, for `acr ask` and `acr resume`.
fn attend(
    paths: &Paths,
    run: &mut Run,
    cfg: &Config,
    s: &flow::Setting,
    caller: &BridgeCaller,
    reloading: &dyn Fn(bool) -> Result<change::Change>,
) -> Result<()> {
    while run.status == flow::Status::Waiting && at_terminal() {
        save(paths, run, cfg)?;
        let line = |prompt: &str| -> Result<String> {
            eprint!("{prompt}");
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            Ok(answer.trim().to_string())
        };
        if run.waiting == Some(flow::Gate::Fix) {
            eprintln!(
                "\n{APP_NAME}: fold these in, in the working tree:\n{}",
                flow::fix_brief(run)
            );
            let answer = line("Press Enter once they're fixed, or type stop: ")?;
            if answer.eq_ignore_ascii_case("stop") {
                flow::decide(
                    run,
                    &Decision {
                        stop: true,
                        ..Decision::default()
                    },
                )?;
            } else {
                flow::fixed_by_session(run, s, caller, Default::default())?;
            }
        } else {
            let ask = carousel::ask(run, cfg, &APP_NAME.to_string());
            eprintln!("\n{}", ask.summary);
            let mut answers = std::collections::BTreeMap::new();
            for q in ask.carousels.iter().flat_map(|c| &c.questions) {
                eprintln!("\n[{}] {}", q.header, q.question);
                for (n, o) in q.options.iter().enumerate() {
                    eprintln!("  {}) {}: {}", n + 1, o.label, o.description);
                }
                let hint = if q.multi_select {
                    "numbers, with commas, or Enter for none"
                } else {
                    "a number, or your own words"
                };
                loop {
                    let typed_in = line(&format!("Your answer ({hint}): "))?;
                    match typed(q, &typed_in) {
                        Some(value) => {
                            answers.insert(q.question.clone(), value);
                            break;
                        }
                        None if q.multi_select => break,
                        None => eprintln!("Choose one, or say it in your own words."),
                    }
                }
            }
            // Recorded at the gate it answers; taken back if they're refused.
            run.answered("a person at the terminal");
            match carousel::apply(run, cfg, &answers, crate::util::now_ms()) {
                Ok(applied) => {
                    keep_feedback(paths, run, &applied.feedback);
                    for note in &applied.notes {
                        eprintln!("{APP_NAME}: {note}");
                    }
                }
                Err(e) => {
                    run.answered_by.pop();
                    eprintln!("{APP_NAME}: {e:#}");
                    continue;
                }
            }
        }
        flow::drive(run, s, caller, reloading)?;
    }
    Ok(())
}

/// What a person typed at the terminal, as an answer: numbers pick
/// choices, and anything else is their own words.
fn typed(q: &carousel::Question, text: &str) -> Option<serde_json::Value> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let picks: Option<Vec<String>> = text
        .split(',')
        .map(|p| {
            p.trim()
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| q.options.get(n))
                .map(|o| o.label.clone())
        })
        .collect();
    match picks {
        Some(labels) if q.multi_select => Some(serde_json::json!(labels)),
        Some(labels) if labels.len() == 1 => Some(serde_json::json!(labels[0])),
        _ => Some(serde_json::json!(text)),
    }
}

/// Runs its closure when dropped, however the scope ends.
struct OnDrop<F: FnMut()>(F);

impl<F: FnMut()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// Cancels the calls a review left at work when its process ended without
/// them, such as on a kill, so that they don't hold their bridges.
fn cancel_left(paths: &Paths, id: &str) {
    let file = run_dir(paths, id).join("calls");
    let Ok(text) = fs::read_to_string(&file) else {
        return;
    };
    let socket = paths.socket();
    let ids: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    for call in &ids {
        let _ = crate::call::cancel_run(&socket, SENDER, call);
    }
    if !ids.is_empty() {
        eprintln!(
            "{APP_NAME}: cancelled {} call(s) review {id} left at work when it stopped.",
            ids.len()
        );
    }
    let _ = fs::remove_file(&file);
}

/// How a review's process is stopped: first as Ctrl-C stops it, which
/// cancels its calls before it exits; then, if it hasn't gone, as a kill;
/// and last by force. Each signal gets this long to work.
const STOP_WAITS: [(libc::c_int, std::time::Duration); 3] = [
    (libc::SIGINT, std::time::Duration::from_secs(30)),
    (libc::SIGTERM, std::time::Duration::from_secs(10)),
    (libc::SIGKILL, std::time::Duration::from_secs(5)),
];

fn signal_name(signal: libc::c_int) -> &'static str {
    match signal {
        libc::SIGINT => "an interrupt",
        libc::SIGTERM => "a kill",
        libc::SIGKILL => "a forced kill",
        _ => "a signal",
    }
}

/// Stops the process driving review `id`, if one is, and waits for it to
/// go. Each signal goes only to the process its mark names, and only while
/// it's still that one: never by a name or a pattern. The process this
/// stopped, or None if none was driving it.
fn stop_live(
    paths: &Paths,
    id: &str,
    waits: &[(libc::c_int, std::time::Duration)],
) -> Result<Option<i32>> {
    let Some(pid) = keep::live(paths, id) else {
        return Ok(None);
    };
    for (signal, wait) in waits {
        if keep::live(paths, id) != Some(pid) {
            break;
        }
        eprintln!(
            "{APP_NAME}: stopping review {id}: {} to process {pid}, which is running it",
            signal_name(*signal)
        );
        // SAFETY: kill only sends a signal, to the process the mark names
        // and that was just confirmed to be the review's own.
        unsafe { libc::kill(pid, *signal) };
        let until = std::time::Instant::now() + *wait;
        while std::time::Instant::now() < until && keep::live(paths, id) == Some(pid) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    if keep::live(paths, id) == Some(pid) {
        bail!(
            "review {id}'s process {pid} hasn't stopped: it may be stuck in the system. Its \
             calls may still be at work"
        );
    }
    keep::clear_stale(paths, id);
    Ok(Some(pid))
}

/// What a stopped review left, for a person to look at or clear: its kept
/// files, the copies it worked in, and anything uncommitted in its
/// checkout, which may hold an Orchestrator's fixes. Nothing here touches
/// any of it.
fn left_behind(paths: &Paths, run: &Run) -> String {
    let id = &run.id;
    let mut out = format!("{APP_NAME}: what review {id} left, untouched:\n");
    match keep::kept(paths, id) {
        Some(kept) => out.push_str(&format!(
            "- its record and files: {}, kept {} days\n",
            kept.display(),
            crate::cleanup::KEEP_DAYS
        )),
        None => out.push_str(&format!(
            "- its record and files: {}\n",
            run_dir(paths, id).display()
        )),
    }
    let copies: Vec<PathBuf> = fs::read_dir(copies_root(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            crate::util::unstamped(&name).starts_with(id.as_str())
        })
        .map(|e| e.path())
        .collect();
    if copies.is_empty() {
        out.push_str("- no copies of the repository\n");
    } else {
        out.push_str(&format!(
            "- {} copy or copies of the repository it worked in, under {}: `{APP_NAME} acr \
             clean` clears them, or the daemon's cleanup does in {} days\n",
            copies.len(),
            copies_root(paths).display(),
            crate::cleanup::KEEP_DAYS
        ));
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&run.repo)
        .args(["status", "--porcelain"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let repo = run.repo.display();
    match status {
        Some(text) if !text.trim().is_empty() => {
            let lines: Vec<&str> = text.lines().collect();
            let fixes = if run.fixed() || !run.steelmans.is_empty() {
                "; the Orchestrator's fixes are among them, beside any work of your own"
            } else {
                ", which may be your own work"
            };
            out.push_str(&format!(
                "- {} uncommitted change(s) in the checkout {repo}{fixes}:\n",
                lines.len()
            ));
            for line in lines.iter().take(20) {
                out.push_str(&format!("    {line}\n"));
            }
            if lines.len() > 20 {
                out.push_str(&format!("    and {} more\n", lines.len() - 20));
            }
            out.push_str(&format!(
                "  See them with `git -C {repo} diff` and `git -C {repo} status`. To discard \
                 them, once you're sure none is yours: `git -C {repo} restore --staged \
                 --worktree .`, then `git -C {repo} clean -n` to list new files, and `-f` to \
                 remove them.\n"
            ));
        }
        Some(_) => out.push_str(&format!("- nothing uncommitted in the checkout {repo}\n")),
        None => out.push_str(&format!("- couldn't read the checkout {repo} with git\n")),
    }
    out
}

/// Writes a ticket draft for each blocker deferred to one, in the review's
/// folder: `tickets/<finding>.md`, title then body, ready to file. Nothing
/// is filed. The drafts written.
fn write_tickets(paths: &Paths, run: &Run) -> Vec<PathBuf> {
    let drafts = report::tickets(run);
    if drafts.is_empty() {
        return Vec::new();
    }
    let dir = run_dir(paths, &run.id).join("tickets");
    if fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    drafts
        .iter()
        .filter_map(|t| {
            let name: String = t
                .finding
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            let path = dir.join(format!("{name}.md"));
            fs::write(&path, format!("# {}\n\n{}", t.title, t.body))
                .ok()
                .map(|()| path)
        })
        .collect()
}

/// Starts a review's progress log in its folder, and says where it is, and
/// what in its bindings could cut its calls off.
fn following(caller: &BridgeCaller, paths: &Paths, id: &str, cfg: &Config, how: &str) {
    let log = run_dir(paths, id).join("progress.log");
    if let Some(dir) = log.parent() {
        let _ = fs::create_dir_all(dir);
    }
    caller.log_to(log.clone());
    caller.stops_calls(run_dir(paths, id).join("calls"));
    caller.note(&format!(
        "review {id} {how}. It says what it's doing as it goes, at least once a minute, \
         here and in {}",
        log.display()
    ));
    for note in cfg.time_notes() {
        caller.note(&note);
    }
}

/// Prints a run's report, and says what to do next.
/// `run`, with every call its caller saw answered that it hasn't recorded:
/// those of a round it stopped in before the round's end, counted from
/// their answers and marked as from a round that didn't finish.
fn with_unrecorded(paths: &Paths, run: &Run) -> Run {
    let mut whole = run.clone();
    let recorded: std::collections::HashSet<String> =
        run.usage.iter().map(|u| u.run_id.clone()).collect();
    for a in flow::Answered::read(&run_dir(paths, &run.id)) {
        if !recorded.contains(&a.id) && !whole.usage.iter().any(|u| u.run_id == a.id) {
            whole.usage.push(a.usage(run.round));
        }
    }
    whole
}

/// A review an outage paused: saved as it stands, with what its round's
/// units finished kept in its folder, and said how to go on. None if it
/// wasn't paused, and an error that isn't an outage's, as it was.
fn paused(paths: &Paths, run: &Run, cfg: &Config, driven: Result<()>) -> Result<Option<ExitCode>> {
    match driven {
        Ok(()) => Ok(None),
        Err(e) if flow::is_outage(&e) => {
            save(paths, run, cfg)?;
            eprintln!(
                "{APP_NAME}: gateway unreachable: saved, continue with `{APP_NAME} acr resume {}` \
                 once it's back. Round {} keeps what its reviewers and Judges finished, and runs \
                 only the rest. ({e:#})",
                run.id, run.round
            );
            Ok(Some(ExitCode::from(OUTAGE)))
        }
        Err(e) => Err(e),
    }
}

fn finish(paths: &Paths, run: &Run, cfg: &Config, pr: Option<u64>) -> Result<ExitCode> {
    // A run stopped mid-round counts that round's finished calls too.
    let run = &with_unrecorded(paths, run);
    save(paths, run, cfg)?;
    // Waiting for you, or over: Grayboard hears, if it's set up.
    grayboard::on_finish(paths, run, cfg);
    let drafts = write_tickets(paths, run);
    keep::finished(paths, run, true)?;
    if !drafts.is_empty() {
        let at = keep::kept(paths, &run.id).map_or_else(
            || {
                run_dir(paths, &run.id)
                    .join("tickets")
                    .display()
                    .to_string()
            },
            |kept| format!("tickets/ in {}", kept.display()),
        );
        eprintln!(
            "{APP_NAME}: {} blocker(s) deferred to a ticket: a draft of each is in {at}, and in \
             the report's `tickets`. Nothing was filed.",
            drafts.len()
        );
    }
    let summary = report::report(run, cfg);
    for line in summary.by_role.iter().map(report::RoleUsage::line) {
        eprintln!("{APP_NAME}: {line}");
    }
    if let Some(line) = report::unfinished_line(&run.usage) {
        eprintln!("{APP_NAME}: {line}");
    }
    if summary.by_role.iter().any(|r| r.shortfall > 0) {
        eprintln!("{APP_NAME}: ({})", report::ADJUSTED_IS);
    }
    for r in &summary.repairs {
        eprintln!("{APP_NAME}: {}", r.line());
    }
    eprintln!("{APP_NAME}: {}", run.reattack_line());
    if !run.weakened.is_empty() {
        eprintln!(
            "{APP_NAME}: it ran weaker than its config: {}",
            run.weakened.join("; ")
        );
    }
    for a in &run.answered_by {
        eprintln!(
            "{APP_NAME}: round {}'s {} gate was answered by {}",
            a.round, a.gate, a.by
        );
    }
    for u in &summary.unnamed_tools {
        eprintln!("{APP_NAME}: {}", u.line());
    }
    for x in &summary.sessions {
        eprintln!("{APP_NAME}: {}", x.line());
    }
    for p in &run.prefixes {
        eprintln!("{APP_NAME}: round {}: {}", p.round, p.line());
    }
    for f in &run.flags {
        eprintln!("{APP_NAME}: {}", report::flag_line(f));
    }
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if let Some(pr) = pr {
        post(run, cfg, pr)?;
    }
    eprintln!("{APP_NAME}: {}", next_steps(run, cfg));
    Ok(match run.status {
        Status::Confirmed => ExitCode::SUCCESS,
        Status::Waiting => ExitCode::from(match run.waiting {
            Some(flow::Gate::Escalation) => ESCALATED,
            _ => WAITING,
        }),
        Status::Running | Status::Stopped => ExitCode::FAILURE,
    })
}

/// What a review's end means, and what to do next, in a sentence or two.
fn next_steps(run: &Run, cfg: &Config) -> String {
    let id = &run.id;
    let blockers: Vec<String> = run
        .lenses
        .values()
        .filter(|l| !l.blockers().is_empty())
        .map(|l| format!("{} ({})", l.id, l.blockers().len()))
        .collect();
    let open: Vec<&str> = run
        .lenses
        .values()
        .filter(|l| l.outcome == flow::Outcome::Open)
        .map(|l| l.id.as_str())
        .collect();
    let writes = cfg
        .binding(Role::Orchestrator, None)
        .is_ok_and(|b| b.writes);
    match (run.status, run.waiting) {
        (Status::Confirmed, _) => {
            format!("confirmed: every lens agrees. Its findings: `{APP_NAME} acr feedback {id}`.")
        }
        (Status::Waiting, Some(flow::Gate::Fix)) => format!(
            "waiting for the fixes: fold what `{APP_NAME} acr ask {id}` lists into the working \
             tree, then `{APP_NAME} acr resume {id} --fixed`, or `--stop`."
        ),
        (Status::Waiting, Some(gate)) => {
            let what = match gate {
                flow::Gate::Scope => format!(
                    "to approve its scope, tier {} ({}), before any reviewer runs",
                    run.tier, run.tier_why
                ),
                flow::Gate::Fold => format!(
                    "to decide its blockers: {}{}",
                    blockers.join(", "),
                    if writes || run.fixer == flow::Fixer::Session {
                        ""
                    } else {
                        " (the Orchestrator's binding can't write, so give it `writes: true` in \
                         acr.yaml, or fix them yourself)"
                    }
                ),
                flow::Gate::Final => "to approve the result".to_string(),
                flow::Gate::Escalation | flow::Gate::Fix => {
                    let unseen: Vec<&str> = run
                        .escalations
                        .iter()
                        .filter(|e| !e.seen)
                        .map(|e| e.reason.as_str())
                        .collect();
                    format!("on what it escalated: {}", unseen.join("; "))
                }
            };
            format!(
                "waiting for you {what}. `{APP_NAME} acr ask {id}` gives its questions, for a \
                 question carousel, or as text with `--text`; answer with `{APP_NAME} acr resume \
                 {id} --answers '<json>'`, or end it with `--stop`."
            )
        }
        (_, _) if blockers.is_empty() => format!(
            "ended with {} still open: `{APP_NAME} acr feedback {id}` lists what each found.",
            open.join(", ")
        ),
        (_, _) => format!(
            "ended with blockers left: {}. Fix them and review again{}; `{APP_NAME} acr \
             feedback {id}` lists them.",
            blockers.join(", "),
            if writes {
                ""
            } else {
                ", or give the Orchestrator's binding `writes: true` in acr.yaml so a review can fix them itself"
            }
        ),
    }
}

/// Where each repository's record of what its reviewers open is kept.
fn learned_root(paths: &Paths) -> PathBuf {
    paths.state.join("acr-learned")
}

/// Where reviewers that can change files get their copies of the checkout.
fn copies_root(paths: &Paths) -> PathBuf {
    paths.state.join("acr-copies")
}

/// A copy kept under [`copies_root`], and whether it's left over.
struct KeptCopy {
    path: PathBuf,
    /// Apparent size, in bytes: on macOS, copies are clones that share
    /// blocks with the repository, so they use less disk than this.
    bytes: u64,
    /// Why it's no longer needed, if it isn't.
    leftover: Option<String>,
}

/// A copy of a check, the doctor or the configurer younger than this may
/// still be in use: none of them runs this long.
const IN_USE: std::time::Duration = std::time::Duration::from_secs(600);

/// Every copy kept, with its size, and why each that's left over is.
fn kept_copies(paths: &Paths) -> Vec<KeptCopy> {
    let running = keep::active(paths);
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(copies_root(paths)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let base = crate::util::unstamped(&name);
        let review = base
            .get(..26)
            .filter(|id| crate::util::is_id(id) && base[26..].starts_with("-r"));
        let leftover = match review {
            Some(id) if running.iter().any(|r| r == id) => None,
            Some(id) if keep::is_finished(paths, id) => Some("its review has finished".into()),
            // Waiting for a person, and a fix may still need its copies.
            Some(id) if run_dir(paths, id).exists() => None,
            Some(_) => Some("its review is gone".into()),
            None => ["check", "doctor", "configure"]
                .into_iter()
                .find(|what| base.starts_with(&format!("{what}-")))
                .filter(|_| crate::util::older_than(&path, IN_USE))
                .map(|what| format!("its {what} has ended")),
        };
        out.push(KeptCopy {
            bytes: apparent_size(&path),
            path,
            leftover,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The bytes of every file under `path`, links not followed.
fn apparent_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(entry.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// `12 MB`, `1.4 GB`.
fn size_text(bytes: u64) -> String {
    match bytes {
        0..1_000_000 => format!("{} KB", bytes.div_ceil(1000)),
        1_000_000..1_000_000_000 => format!("{} MB", bytes / 1_000_000),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

const CLONES: &str = "on macOS they're clones that share blocks with the repository, so they \
                      use less disk than that";

/// `acr clean`: removes the copies left over, or with `dry_run`, lists them.
fn clean(paths: &Paths, dry_run: bool) -> Result<()> {
    let kept = kept_copies(paths);
    let leftovers: Vec<&KeptCopy> = kept.iter().filter(|k| k.leftover.is_some()).collect();
    if leftovers.is_empty() {
        println!(
            "No copies to clear: {} kept, each for a review running or waiting, or a check \
             still in use.",
            kept.len()
        );
        return Ok(());
    }
    let mut cleared = 0;
    let mut bytes = 0;
    for k in &leftovers {
        let name = k.path.file_name().unwrap_or_default().to_string_lossy();
        let why = k.leftover.as_deref().unwrap_or_default();
        if dry_run {
            println!("  would remove {name}: {}, {why}", size_text(k.bytes));
        } else {
            match fs::remove_dir_all(&k.path) {
                Ok(()) => {
                    println!("  removed {name}: {}, {why}", size_text(k.bytes));
                    cleared += 1;
                    bytes += k.bytes;
                }
                Err(e) => println!("  couldn't remove {name}: {e}"),
            }
        }
    }
    let total: u64 = leftovers.iter().map(|k| k.bytes).sum();
    if dry_run {
        println!(
            "{} copies would go, {} apparent; {CLONES}. Nothing was removed.",
            leftovers.len(),
            size_text(total)
        );
    } else {
        println!(
            "Removed {cleared} copies, {} apparent; {CLONES}.",
            size_text(bytes)
        );
    }
    Ok(())
}

/// What doctor says of the copies kept, if there are any.
fn copies_note(paths: &Paths) -> Option<String> {
    let kept = kept_copies(paths);
    if kept.is_empty() {
        return None;
    }
    let total: u64 = kept.iter().map(|k| k.bytes).sum();
    let leftover = kept.iter().filter(|k| k.leftover.is_some()).count();
    let clear = if leftover > 0 {
        format!(
            "; {leftover} of them are left over from finished work, and `{APP_NAME} acr clean` \
             removes those now"
        )
    } else {
        String::new()
    };
    Some(format!(
        "{} copies kept in {}, {} apparent ({CLONES}){clear}. The daemon's cleanup clears each \
         {} days after it's made",
        kept.len(),
        copies_root(paths).display(),
        size_text(total),
        crate::cleanup::KEEP_DAYS
    ))
}

/// The bridges that reviewers who can change files go through: each must
/// let them work in the copies.
fn copy_bridges(cfg: &Config) -> Result<Vec<String>> {
    let mut bridges = Vec::new();
    for lens in &cfg.lenses {
        let b = cfg.binding(Role::Reviewer, Some(lens))?;
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if b.changes_files() && !bridges.contains(&bridge) {
            bridges.push(bridge);
        }
    }
    Ok(bridges)
}

/// Lets a bridge work in any trial's copy, which needs a restart only the
/// first time: after that, a trial's copy is already allowed, and no trial
/// restarts a bridge that other agents' runs are going through.
fn allow_trials(paths: &Paths, bridge: &str) -> Result<()> {
    crate::setup::acr_dir(paths, &paths.state.join("acr-trials"), bridge)?;
    Ok(())
}

/// Lets those bridges work in the copies, which needs a bridge restart only
/// the first time.
fn allow_copies(paths: &Paths, cfg: &Config) -> Result<()> {
    for bridge in copy_bridges(cfg)? {
        crate::setup::acr_dir(paths, &copies_root(paths), &bridge)?;
    }
    Ok(())
}

/// What a run holds to, kept with it: its non-issues are read once, at the
/// start, so resuming can't pick up a list that's changed since.
fn setting<'a>(
    paths: &Paths,
    cfg: &'a Config,
    run_id: &str,
    list: Option<String>,
) -> Result<Setting<'a>> {
    let dir = run_dir(paths, run_id);
    let kept = dir.join("not-findings.md");
    let not_findings = match list {
        Some(text) => {
            fs::create_dir_all(&dir)?;
            fs::write(&kept, &text)?;
            text
        }
        None => fs::read_to_string(&kept).unwrap_or_default(),
    };
    Ok(Setting {
        cfg,
        not_findings,
        work_dir: dir,
        description: None,
        converge: false,
        stop_on_failure: false,
        outage_wait_secs: OUTAGE_WAIT_SECS,
        copies: Some(copies_root(paths)),
        head_copy: None,
        pr: None,
        learned: Some(learned_root(paths)),
        builder_brief: None,
    })
}

/// A copy of `commit` for review `id` to read, under the copies, where it
/// stays until the daemon's cleanup clears it, as a round's copies do. Every
/// bridge the review's roles go through may work there, since every role
/// reads the change there, not only the reviewers that change files.
fn head_copy(paths: &Paths, cfg: &Config, id: &str, repo: &Path, commit: &str) -> Result<PathBuf> {
    let name = repo
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_owned());
    let dir = flow::fresh_copy_dir(copies_root(paths).join(format!("{id}-head")));
    let copy = dir.join(name);
    change::copy_at(repo, commit, &copy).with_context(|| {
        format!(
            "making a copy of {} to review",
            &commit[..12.min(commit.len())]
        )
    })?;
    for bridge in reading_bridges(cfg)? {
        crate::setup::acr_dir(paths, &copies_root(paths), &bridge)?;
    }
    Ok(copy)
}

/// The local bridges a review's roles go through: every reviewer's, and
/// the Judge's, the Sizer's and the Orchestrator's, each once.
fn reading_bridges(cfg: &Config) -> Result<Vec<String>> {
    let mut bindings = Vec::new();
    for lens in &cfg.lenses {
        bindings.push(cfg.binding(Role::Reviewer, Some(lens))?);
    }
    for role in [Role::Judge, Role::Sizer, Role::Orchestrator] {
        if let Ok(b) = cfg.binding(role, None) {
            bindings.push(b);
        }
    }
    let mut bridges: Vec<String> = Vec::new();
    for b in bindings {
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if !bridges.contains(&bridge) {
            bridges.push(bridge);
        }
    }
    Ok(bridges)
}

/// What earlier reviews of `repo` say, from ACR's history.
fn history(paths: &Paths, repo: &Path) -> plan::History {
    plan::History::of(keep::history(paths, repo))
}

/// 1234567 as 1,234,567.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    out
}

fn describe_plan(plan: &plan::Plan) {
    let how = if plan.tier_decided {
        "from its paths"
    } else {
        "assumed: the Sizer decides on a real run, and this is the highest it could be"
    };
    println!(
        "Tier: {} ({how}), {} reviewer(s) per lens",
        plan.tier, plan.fan_out
    );
    println!("Files: {}", plan.files);
    if !plan.generated.is_empty() {
        let added: u64 = plan.generated.iter().map(|g| g.added).sum();
        let removed: u64 = plan.generated.iter().map(|g| g.removed).sum();
        println!(
            "Generated, left out of what reviewers get: {} file(s), +{}/-{} lines",
            plan.generated.len(),
            thousands(added),
            thousands(removed)
        );
        for g in &plan.generated {
            println!("  {} (+{} -{}): {}", g.path, g.added, g.removed, g.why);
        }
    }
    println!("Lenses:");
    for lens in &plan.lenses {
        println!("  {} on {} ({:?})", lens.id, lens.binding, lens.why);
    }
    let tokens = |cost: &plan::RoleCost| {
        let usd = match (cost.subscription, cost.usd) {
            (true, _) => "subscription".to_string(),
            (false, Some(u)) => format!("${u:.4}"),
            (false, None) => "no price".to_string(),
        };
        let cached = match cost.cache_read {
            0 => String::new(),
            n => format!(", {} read from cache,", thousands(n)),
        };
        format!(
            "about {} tokens in{cached} and {} out, {usd}",
            thousands(cost.tokens_in),
            thousands(cost.tokens_out)
        )
    };
    println!("First round:");
    for cost in &plan.first_round {
        println!(
            "  {} on {}: {} call(s), {}",
            cost.role.as_str(),
            cost.binding,
            cost.calls,
            tokens(cost)
        );
    }
    if let Some(fix) = &plan.fix {
        println!(
            "Each fix between rounds: orchestrator on {}, {}",
            fix.binding,
            tokens(fix)
        );
        if let Some(f) = &plan.fixer {
            println!(
                "Easy fix groups go to {} instead: each blocker {:?} or lower, citing at most {} \
                 file(s), its cause stated, no one's call and not recurring",
                f.binding, f.max_severity, f.max_files
            );
        }
    }
    // Tokens always. Dollars only for calls billed in them.
    let figure = |e: &plan::Estimate| {
        let usd = match (plan.dollars, e.usd) {
            (false, _) => String::new(),
            (true, Some(u)) => format!(" (${u:.2})"),
            (true, None) => " (dollars: a binding has no price)".to_string(),
        };
        format!("about {} tokens{usd}", thousands(e.tokens))
    };
    println!("Tokens:");
    println!(
        "  At least, with every lens done in one round: {}",
        figure(&plan.least)
    );
    match &plan.expected {
        None => {
            println!("  Expected: can't say yet, with no finished reviews of this repository here")
        }
        Some(e) => println!(
            "  Expected, from the last {} finished review(s) here: {}",
            plan.history_runs,
            figure(e)
        ),
    }
    println!(
        "  At most, with every lens through every round and a fix between: {}",
        figure(&plan.most)
    );
    let reattack = match plan.reattack {
        Some(config::Reattack::All) => "every lens attacks again after each fix (reattack: all)",
        Some(config::Reattack::Blocked) => {
            "only the lenses with blockers attack again after a fix (reattack: blocked)"
        }
        None => {
            "which lenses attack again after a fix is asked at the scope gate; left unanswered, \
             only those with blockers"
        }
    };
    println!("Re-attack: {reattack}.");
    if let Some(cap) = &plan.spend_cap {
        println!(
            "Spend cap: {}. Past it, no new call starts; calls at work finish, and you're asked \
             to raise it, go on without it, or stop.",
            cap.line()
        );
    }
    if let (Some(e), Some(all)) = (&plan.expected, &plan.expected_all) {
        println!(
            "  Expected with every lens attacking after each fix: {}, against {} with only \
             those that had blockers",
            figure(all),
            figure(e)
        );
    }
    if !plan.unpriced.is_empty() {
        println!(
            "No price for {}: add it under costs.prices, or mark its binding `subscription: true`",
            plan.unpriced.join(", ")
        );
    }
    if plan.per_call.is_empty() {
        println!(
            "These count only the text ACR sends and reads back. What an agent's tools read and \
             what it thinks add more: set costs.per_call to count them."
        );
    }
    for (role, extra) in &plan.per_call {
        println!(
            "Each {} call adds {} tokens in, {} read from cache and {} out, by costs.per_call",
            role.as_str(),
            extra.input_tokens,
            extra.cache_read_tokens,
            extra.output_tokens
        );
    }
    for e in &plan.escalates {
        println!("Escalates: {e}");
    }
}

/// How many reviews are under way on this machine now.
pub fn active_reviews(paths: &Paths) -> usize {
    keep::active(paths).len()
}

/// Checks the text of a user's ACR config, as `acrbus setup acr` writes it.
pub fn check_user_config(text: &str) -> Result<()> {
    config::parse_user(text).map(|_| ())
}

/// The daemon's cleanup of what reviews leave: finished reviews' files,
/// reviewers' and checks' copies, trials, and findings kept for feedback,
/// once they're past keeping.
/// Returns how many went.
pub fn sweep(paths: &Paths) -> usize {
    keep::prune(paths)
        + flow::prune_copies(&copies_root(paths))
        + trial::prune(paths)
        + configure::prune_findings(&paths.state)
        + grayboard::prune(paths)
}

/// `starter`: the bus name of the session running the command, when it's
/// one, which a review records as its builder.
pub fn run(paths: &Paths, command: AcrCmd, starter: Option<String>) -> Result<ExitCode> {
    // Every role a review starts reads the system prompt this version has.
    if let Err(e) = crate::setup::keep_system_prompt(paths) {
        eprintln!("{APP_NAME}: couldn't keep ACR's system prompt: {e:#}");
    }
    match command {
        AcrCmd::Schema => print!("{SCHEMA}"),
        AcrCmd::Doctor { repo, no_tests } => {
            if !doctor(paths, &repo, !no_tests)? {
                return Ok(ExitCode::FAILURE);
            }
        }
        AcrCmd::AddRepo { path, bridge } => crate::setup::acr_repo(paths, &path, true, &bridge)?,
        AcrCmd::RemoveRepo { path, bridge } => {
            crate::setup::acr_repo(paths, &path, false, &bridge)?
        }
        AcrCmd::Validate { file, repo, base } => {
            let user = config::load_user(&user_config_path(paths))?;
            let (text, source) = repo_layer(paths, &repo, base.as_deref(), file.as_deref())?;
            let cfg = config::resolve(&user, config::parse_repo(&text)?, &Flags::default())?;
            let from = match &source {
                Source::File(f) => f.display().to_string(),
                Source::Base => format!("{REPO_CONFIG} on the base"),
            };
            println!(
                "Valid: {} tiers, {} lenses, from {from}.",
                cfg.tiers.len(),
                cfg.lenses.len()
            );
            if cfg.probes_off {
                println!("{}", config::PROBES_OFF);
            }
            for note in cfg.time_notes() {
                println!("{note}");
            }
            if cfg.allowed_bindings.is_empty() {
                println!(
                    "Bindings not checked: there's no {}.",
                    user_config_path(paths).display()
                );
            }
            match this_machine(paths) {
                Ok(machine) => check_on(paths, &cfg, &machine, &repo, false)?,
                Err(e) => println!("Not checked that every agent is on this machine: {e:#}."),
            }
        }
        AcrCmd::Plan { target, json } => {
            let (cfg, source) = load_config(paths, &target, &flags(&target))?;
            check_machine(paths, &cfg, &target.repo, false)?;
            let change = reload(&cfg, &target.repo, &target.base, target.head.as_deref())?;
            let plan = plan::dry_run(&cfg, &change, &history(paths, &target.repo))?;
            if !json && let Source::File(file) = &source {
                println!("Config: {}", file.display());
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                if cfg.probes_off {
                    println!("{}", config::PROBES_OFF);
                }
                for note in cfg.time_notes() {
                    println!("{note}");
                }
                describe_plan(&plan);
            }
        }
        AcrCmd::Run {
            target,
            post_pr,
            uncalibrated,
        } => {
            let (cfg, source) = load_config(paths, &target, &flags(&target))?;
            cfg.check_bindings()?;
            let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
            let (text, _) = repo_layer(
                paths,
                &target.repo,
                Some(&target.base),
                target.config.as_deref(),
            )?;
            let name = change::name(&target.repo)?;
            let now = fingerprint(&cfg, &text, &list)?;
            let unaccepted = calibrated(paths, &name, &now, uncalibrated)?;
            if let Some(note) = &unaccepted {
                eprintln!("{note}");
            }
            let weakened = weakened_by_flags(paths, &target, &cfg, unaccepted.is_some())?;
            check_machine(paths, &cfg, &target.repo, false)?;
            // The commit under review resolves before anything is made.
            let commit = change::reviewed_commit(
                &target.repo,
                &target.base,
                target.head.as_deref(),
                cfg.review_scope,
            )?;
            allow_copies(paths, &cfg)?;
            let description = description_of(&target, post_pr)?;
            let id = new_id();
            claim(paths, &name, &target, post_pr, &id)?;
            let mut s = setting(paths, &cfg, &id, Some(list))?;
            s.description = description;
            s.pr = target.pr.or(post_pr);
            // A review of a commit reads a copy of it, so the checkout can be
            // on any branch, with any changes, while it runs.
            if let Some(commit) = &commit {
                s.head_copy = Some(head_copy(paths, &cfg, &id, &target.repo, commit)?);
            }
            s.builder_brief = target
                .brief
                .as_deref()
                .map(stages::read_builder_brief)
                .transpose()?;
            // Under way until this ends, so an update to acrbus waits for it.
            let _active = keep::active_guard(paths, &id)?;
            let (repo, base, head) = (&target.repo, target.base.as_str(), target.head.as_deref());
            let change = reload(&cfg, repo, base, head)?;
            let caller = BridgeCaller::new(paths)?;
            following(&caller, paths, &id, &cfg, "started");
            if s.builder_brief.is_none() {
                caller.note(
                    "no builder's brief: the session that built the change knows best where it \
                     could be wrong. Write one, as docs/acr-repo-setup.md's section 5a shows, and \
                     pass it with --brief next time",
                );
            }
            caller.budget(paths, &cfg, &change, repo, &[]);
            let mut run = match flow::start(&id, repo, base, head, &s, &change, &caller) {
                // Before the review exists there's nothing to save: said, and
                // the same exit as a paused round's.
                Err(e) if flow::is_outage(&e) => {
                    eprintln!(
                        "{APP_NAME}: gateway unreachable before the review could start, so \
                         nothing was saved: run it again once it's back. ({e:#})"
                    );
                    return Ok(ExitCode::from(OUTAGE));
                }
                started => started?,
            };
            run.starter = starter;
            if !weakened.is_empty() {
                caller.note(&format!(
                    "this review runs weaker than its config: {}",
                    weakened.join("; ")
                ));
            }
            run.weakened = weakened;
            if let Some(cap) = &run.spend_cap {
                caller.note(&format!(
                    "spend cap: {}; past it, no new call starts, and you're asked to raise it, \
                     go on without it, or stop",
                    cap.line()
                ));
            }
            // After a fix, the change is the working tree.
            let reloading = |fixed: bool| reload(&cfg, repo, base, head.filter(|_| !fixed));
            save(paths, &run, &cfg)?;
            let driven = flow::drive(&mut run, &s, &caller, &reloading)
                .and_then(|()| attend(paths, &mut run, &cfg, &s, &caller, &reloading));
            if let Some(code) = paused(paths, &run, &cfg, driven)? {
                return Ok(code);
            }
            return finish(paths, &run, &cfg, post_pr);
        }
        AcrCmd::Clean { dry_run } => clean(paths, dry_run)?,
        AcrCmd::Usage { run_id, json } => {
            let run = with_unrecorded(paths, &kept_run(paths, &run_id)?);
            let rows = report::by_role(&run.usage);
            let repairs = report::repairs(&run.usage);
            let firsts = report::first_turns(&run.usage);
            let restarts = report::restarts(&run.usage);
            let unnamed = report::unnamed_tools(&run.usage);
            let fixes = report::fix_routes(&run);
            let unreported = report::writes_unreported(&run.usage);
            let sessions = report::sessions(&run.usage);
            let adj = report::adjusted(&run.usage);
            let shortfall: u64 = adj.iter().map(|a| a.shortfall).sum();
            let billed: f64 = run.usage.iter().filter_map(|u| u.usd).sum();
            let cost_adjusted: Option<f64> = adj
                .iter()
                .zip(&run.usage)
                .map(|(a, u)| {
                    if a.shortfall == 0 {
                        Some(u.usd.unwrap_or(0.0))
                    } else {
                        a.usd
                    }
                })
                .sum();
            if json {
                // Each session of a call broken up, as its own call.
                let parts: Vec<&flow::Usage> =
                    run.usage.iter().filter(|u| u.part.is_some()).collect();
                let calls: Vec<serde_json::Value> = run
                    .usage
                    .iter()
                    .zip(&adj)
                    .map(|(u, a)| {
                        serde_json::json!({
                            "round": u.round, "role": u.role, "call": u.label,
                            "unfinished": u.unfinished,
                            "cost": u.usd, "cost_adjusted": a.usd,
                            "cache_shortfall_tokens": a.shortfall,
                            "turns": u.turns, "tool_calls": u.tool_calls,
                        })
                    })
                    .collect();
                let out = serde_json::json!({
                    "run_id": run.id, "by_role": rows, "facts": run.facts,
                    "prefixes": run.prefixes, "repairs": repairs,
                    "first_turns": firsts, "cache_writes_unreported": unreported,
                    "restarts": restarts, "fix_routes": fixes,
                    "unnamed_tools": unnamed,
                    "sessions": sessions, "session_calls": parts,
                    "calls": calls, "cost": billed, "cost_adjusted": cost_adjusted,
                    "cache_shortfall_tokens": shortfall,
                    "cost_adjusted_is": report::ADJUSTED_IS,
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                for row in &rows {
                    println!("{}", row.line());
                }
                if let Some(line) = report::unfinished_line(&run.usage) {
                    println!("{line}");
                }
                for p in &run.prefixes {
                    println!("round {}: {}", p.round, p.line());
                }
                for r in &repairs {
                    println!("{}", r.line());
                }
                for r in &restarts {
                    println!("{}", r.line());
                }
                for u in &unnamed {
                    println!("{}", u.line());
                }
                for f in &fixes {
                    println!("{}", f.line());
                }
                for f in &firsts {
                    println!("{}", f.line());
                }
                if unreported {
                    println!("{}", report::WRITES_UNREPORTED);
                }
                for x in &sessions {
                    println!("{}", x.line());
                }
                for line in report::session_lines(&run.usage) {
                    println!("{line}");
                }
                if shortfall > 0 {
                    println!(
                        "in all: ${billed:.3} billed; {}; {shortfall} tokens billed fresh that \
                         the provider should have read cached",
                        cost_adjusted.map_or_else(
                            || "no cache-adjusted figure (no cached price to go by)".to_string(),
                            |d| format!("${d:.3} cache-adjusted")
                        )
                    );
                    println!("({})", report::ADJUSTED_IS);
                }
                if run.usage.iter().any(|u| u.label.is_none()) {
                    println!(
                        "(calls recorded before 0.1.21 carry no label, so the Sizer's call that \
                         finds defects more than one lens found counts under the Sizer)"
                    );
                }
            }
        }
        AcrCmd::Ask { run_id, text } => {
            not_finished(paths, &run_id)?;
            // Running, it asks nothing yet: where each lens stands instead.
            if keep::active(paths).contains(&run_id) {
                print!("{}", lens_states(paths, &run_id, text)?);
                return Ok(ExitCode::SUCCESS);
            }
            let dir = run_dir(paths, &run_id);
            let read =
                |name: &str| fs::read(dir.join(name)).with_context(|| format!("no run {run_id}"));
            let run: Run = serde_json::from_slice(&read("run.json")?)?;
            let cfg: Config = stored_config(&read("config.json")?)?;
            let ask = carousel::ask(&run, &cfg, &APP_NAME.to_string());
            if text {
                print!("{}", carousel::as_text(&ask));
            } else {
                println!("{}", serde_json::to_string_pretty(&ask)?);
            }
        }
        AcrCmd::Stop { run_id } => {
            not_finished(paths, &run_id)?;
            if !run_dir(paths, &run_id).join("run.json").exists() {
                bail!("no review {run_id} here");
            }
            run(
                paths,
                AcrCmd::Resume {
                    run_id: run_id.clone(),
                    answers: None,
                    fixed: false,
                    brief: None,
                    fold: Vec::new(),
                    waive: Vec::new(),
                    defer: Vec::new(),
                    approve: false,
                    stop: true,
                    post_pr: None,
                },
                None,
            )?;
            eprintln!("{APP_NAME}: review {run_id} is stopped.");
            return Ok(ExitCode::SUCCESS);
        }
        AcrCmd::Resume {
            run_id,
            answers,
            fixed,
            brief,
            fold,
            waive,
            defer,
            approve,
            stop,
            post_pr,
        } => {
            not_finished(paths, &run_id)?;
            let dir = run_dir(paths, &run_id);
            let read =
                |name: &str| fs::read(dir.join(name)).with_context(|| format!("no run {run_id}"));
            if keep::live(paths, &run_id).is_some() {
                if !stop {
                    bail!(
                        "review {run_id} is still running in another process: stop it with \
                         `{APP_NAME} acr stop {run_id}`, or Ctrl-C where it runs, which cancels \
                         its calls"
                    );
                }
                stop_live(paths, &run_id, &STOP_WAITS)?;
            }
            keep::clear_stale(paths, &run_id);
            // Read after any process driving it has stopped, as it left it.
            let mut run: Run = serde_json::from_slice(&read("run.json")?)?;
            let _active = keep::active_guard(paths, &run_id)?;
            cancel_left(paths, &run_id);
            let cfg: Config = stored_config(&read("config.json")?)?;
            // A person's answers are applied, and their verdicts kept for the
            // calibration, before anything is called.
            let mut stop = stop;
            let answered = answers.is_some();
            // Where they came from: a Grayboard reply's sender, or the flag.
            let from = answers
                .as_deref()
                .and_then(|a| a.strip_prefix('@'))
                .and_then(|file| grayboard::answered_from(paths, Path::new(file)));
            let deciding = !fold.is_empty() || !waive.is_empty() || !defer.is_empty() || approve;
            if cfg.answers == config::Answers::Person && !stop {
                if answered && from.is_none() {
                    bail!(
                        "{run_id}'s config takes answers from a person only (answers: person), \
                         so --answers is refused: answer at a terminal with `{APP_NAME} acr \
                         resume {run_id}`, or on Grayboard"
                    );
                }
                if deciding {
                    bail!(
                        "{run_id}'s config takes answers from a person only (answers: person), \
                         so --fold, --waive, --defer and --approve are refused: answer at a \
                         terminal with `{APP_NAME} acr resume {run_id}`, or on Grayboard"
                    );
                }
            }
            if answered || deciding {
                let by = match (&from, answered) {
                    (Some(sender), _) => format!("a Grayboard reply from {sender}"),
                    (None, true) => "acr resume --answers".to_string(),
                    (None, false) => {
                        "acr resume's --fold, --waive, --defer or --approve".to_string()
                    }
                };
                run.answered(by);
            }
            if let Some(answers) = answers {
                let applied = carousel::apply(
                    &mut run,
                    &cfg,
                    &read_answers(&answers)?,
                    crate::util::now_ms(),
                )?;
                keep_feedback(paths, &run, &applied.feedback);
                for note in &applied.notes {
                    eprintln!("{APP_NAME}: {note}");
                }
                stop |= applied.stopped;
            }
            let fixing = fixed || run.waiting == Some(flow::Gate::Fix);
            // Ending a review calls nothing and reads no code, so neither its
            // agents nor its checkout has to be as it was.
            if stop {
                eprintln!(
                    "{APP_NAME}: ending review {run_id}, as asked. `{APP_NAME} acr feedback \
                     {run_id}` lists its findings."
                );
            } else {
                // Until a fix, the review is of its head: a copy of it, made
                // again if the cleanup took it, or, for a review saved
                // before reviews read a copy, the checkout, which then must
                // hold it.
                if !fixing && !run.fixed() {
                    match &run.head_copy {
                        Some(copy) if !copy.exists() => {
                            let commit = change::reviewed_commit(
                                &run.repo,
                                &run.base,
                                run.head.as_deref(),
                                cfg.review_scope,
                            )?
                            .context("a review with a copy of its commit has a head")?;
                            change::copy_at(&run.repo, &commit, copy)?;
                        }
                        Some(_) => {}
                        None => change::checked_out(
                            &run.repo,
                            &run.base,
                            run.head.as_deref(),
                            cfg.review_scope,
                        )?,
                    }
                }
                check_machine(paths, &cfg, &run.repo, true)?;
            }
            // Stopping ends it where it stands, waiting for someone or left
            // running by a process that's gone.
            if stop {
                flow::end_here(&mut run);
            } else if !answered && !fixed && run.outage.is_none() {
                let decision = Decision {
                    fold,
                    waive,
                    defer,
                    approve,
                    stop,
                };
                flow::decide(&mut run, &decision)?;
            }
            let mut s = setting(paths, &cfg, &run_id, None)?;
            s.description = run.description.clone();
            // The builder's brief it started with: round 1 runs under a
            // resume whenever the scope gate stopped it.
            s.builder_brief = run.builder_brief.clone();
            let (repo, base) = (run.repo.clone(), run.base.clone());
            let caller = BridgeCaller::new(paths)?;
            following(&caller, paths, &run_id, &cfg, "resumed");
            if let Ok(change) = reload(&cfg, &repo, &base, run.head.as_deref()) {
                caller.budget(paths, &cfg, &change, &repo, &run.usage);
            }
            if fixed {
                let briefs = brief
                    .as_deref()
                    .map(stages::read_builder_brief)
                    .transpose()?
                    .map(|b| b.lenses)
                    .unwrap_or_default();
                flow::fixed_by_session(&mut run, &s, &caller, briefs)?;
            }
            // Under way again, as its record says to anyone who looks.
            run.status = flow::Status::Running;
            run.waiting = None;
            save(paths, &run, &cfg)?;
            let head = run.head.clone();
            let reloading =
                |fixed: bool| reload(&cfg, &repo, &base, head.as_deref().filter(|_| !fixed));
            let driven = flow::drive(&mut run, &s, &caller, &reloading)
                .and_then(|()| attend(paths, &mut run, &cfg, &s, &caller, &reloading));
            if let Some(code) = paused(paths, &run, &cfg, driven)? {
                return Ok(code);
            }
            let code = finish(paths, &run, &cfg, post_pr)?;
            if stop {
                eprint!("{}", left_behind(paths, &run));
            }
            return Ok(code);
        }
        AcrCmd::Benchmark { target, known } => {
            let known: Vec<Known> = serde_yaml_ng::from_str(
                &fs::read_to_string(&known)
                    .with_context(|| format!("reading {}", known.display()))?,
            )
            .context("in the known blockers")?;
            let description = description_of(&target, None)?;
            let run = one_round(paths, &target, description)?;
            let score = flow::score(&run, &known)?;
            println!("{}", serde_json::to_string_pretty(&score)?);
        }
        AcrCmd::Carry { name, base, head } => {
            let c = carry::carry(&paths.state, &name, base.as_deref(), &head)?;
            println!(
                "Carried into {}: base {}, head {}.\nReview it with `{APP_NAME} acr run --repo {} \
                 --base {} --head {}`, once {name}'s config is in place and calibrated, and \
                 `{APP_NAME} acr add-repo {}` lets ACR review it.",
                c.repo.display(),
                c.base,
                c.head,
                c.repo.display(),
                c.base,
                c.head,
                c.repo.display()
            );
        }
        AcrCmd::Calibration {
            repo,
            base,
            config,
            accept,
        } => {
            let name = change::name(&repo)?;
            let kept_at = match &config {
                Some(file) => file.clone(),
                None => outside_config(paths, &repo)?,
            };
            let mut folded = None;
            if accept {
                // A waiting proposal goes in first, so it's what's accepted.
                if let Some(applied) = configure::apply(&kept_at, crate::util::now_ms())? {
                    println!("Put the proposed config in place at {}.", kept_at.display());
                    folded = applied.folded;
                }
            }
            let target = Target {
                repo: repo.clone(),
                base: base.clone(),
                config: config.clone(),
                head: None,
                human_gate: None,
                max_rounds: None,
                reattack: None,
                spend_cap_usd: None,
                review_scope: None,
                bindings: Vec::new(),
                description: None,
                pr: None,
                brief: None,
            };
            // A proposal waiting where no config is yet: say so, not "none".
            let waiting = configure::pending(&kept_at);
            if !accept && !kept_at.exists() && waiting.config.exists() {
                println!(
                    "{name} has no accepted review config yet, but a proposal waits at {}, \
                     with its reasons in {}. Read them, then accept it: `{APP_NAME} acr \
                     calibration --repo {} --accept`.",
                    waiting.config.display(),
                    waiting.why.display(),
                    repo.display()
                );
                return Ok(ExitCode::FAILURE);
            }
            let (cfg, source) = load_config(paths, &target, &flags(&target))?;
            let (text, _) = repo_layer(paths, &repo, Some(&base), config.as_deref())?;
            let decided = not_findings(&cfg, &source, &repo, &base)?;
            let now = fingerprint(&cfg, &text, &decided)?;
            if accept {
                calibration::accept(&paths.state, &name, &now, crate::util::now_ms(), folded)?;
                println!("Accepted {name}'s review config.");
            }
            let record = calibration::load(&paths.state, &name)?;
            let unfolded =
                configure::feedback(&paths.state, &name, record.feedback_folded_at)?.len();
            let waiting = configure::pending(&kept_at).config;
            println!(
                "{}",
                calibration::report(
                    &name,
                    &record,
                    &now,
                    unfolded,
                    waiting.exists().then_some(waiting.as_path())
                )
            );
        }
        AcrCmd::JudgeCheck {
            keys,
            out,
            per_finding,
            first_pass,
        } => {
            for key in keys {
                let result = calibrate(paths, &key, per_finding, first_pass.as_deref())?;
                println!(
                    "{}: blockers agreed {}, missed {}, overrated {}; non-blockers agreed {}; unrated {}",
                    result["key"].as_str().unwrap_or_default(),
                    result["agreed_blockers"],
                    result["missed_blockers"],
                    result["overrated"],
                    result["agreed_non_blockers"],
                    result["unrated"],
                );
                if let Some(each) = result.get("per_finding") {
                    println!(
                        "  per finding: blockers agreed {}, missed {}, overrated {}; non-blockers \
                         agreed {}; unrated {}; same verdict as per dimension {} of {}",
                        each["agreed_blockers"],
                        each["missed_blockers"],
                        each["overrated"],
                        each["agreed_non_blockers"],
                        each["unrated"],
                        each["same_as_per_dimension"],
                        each["compared"],
                    );
                }
                if let Some(each) = result.get("first_pass") {
                    println!(
                        "  with a first pass on {}: blockers agreed {}, missed {}, overrated {}; \
                         non-blockers agreed {}; unrated {}; it settled {} of {} dimensions, \
                         and sent {} on to the Judge",
                        each["binding"].as_str().unwrap_or_default(),
                        each["agreed_blockers"],
                        each["missed_blockers"],
                        each["overrated"],
                        each["agreed_non_blockers"],
                        each["unrated"],
                        each["settled"],
                        each["dimensions"],
                        each["sent_on"],
                    );
                }
                if let Some(out) = &out {
                    let mut file = fs::OpenOptions::new().create(true).append(true).open(out)?;
                    std::io::Write::write_all(&mut file, format!("{result}\n").as_bytes())?;
                }
            }
        }
        AcrCmd::Configure { repo } => configure_repo(paths, &repo, false)?,
        AcrCmd::Calibrate { repo } => configure_repo(paths, &repo, true)?,
        AcrCmd::Feedback {
            run,
            finding,
            verdict,
            missed,
            lens,
            note,
        } => {
            let kept = configure::kept(&paths.state, &run)?;
            let mut f = configure::Feedback {
                at: crate::util::now_ms(),
                run: run.clone(),
                finding: None,
                verdict: None,
                missed: None,
                lens: None,
                note,
            };
            match (finding, verdict, missed) {
                (Some(which), Some(v), _) => {
                    f.finding = Some(configure::finding(&kept, &which)?.clone());
                    f.verdict = Some(v);
                }
                (_, _, Some(m)) => {
                    f.missed = Some(m);
                    f.lens = lens;
                }
                _ if f.note.is_some() => {}
                _ => {
                    println!("{}", configure::listed(&kept));
                    return Ok(ExitCode::SUCCESS);
                }
            }
            configure::add_feedback(&paths.state, &kept.repo, &f)?;
            println!(
                "Noted: {}\n`{APP_NAME} acr calibrate` folds it into {}'s config when you're \
                 ready.",
                f.described(),
                kept.repo
            );
        }
        AcrCmd::Residual {
            key,
            patch,
            commit,
            also,
            out,
        } => {
            let mut k = trial::read_key(&key)?;
            let name = key
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let run: Option<trial::Outcome> = match &also {
                Some(results) => fs::read_to_string(results)?
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| serde_json::from_str::<trial::Outcome>(l).ok())
                    .rfind(|o| o.key.starts_with(name.trim_end_matches("-briefed"))),
                None => None,
            };
            let defects = trial::defects(&k, run.as_ref());
            k.patch = patch;
            if let Some(commit) = commit {
                k.head = commit;
            }
            let mut result = trial::residual(paths, k, &defects)?;
            result["key"] = serde_json::json!(name);
            println!(
                "{name}: {} present ({} blockers), {} absent, {} unclear",
                result["present"], result["present_blockers"], result["absent"], result["unclear"]
            );
            if let Some(out) = &out {
                let mut file = fs::OpenOptions::new().create(true).append(true).open(out)?;
                std::io::Write::write_all(&mut file, format!("{result}\n").as_bytes())?;
            }
        }
        AcrCmd::Rescore { key, results } => {
            let n = trial::rescore(paths, &key, &results)?;
            println!(
                "Scored {n} trial(s) of {} in {}.",
                key.display(),
                results.display()
            );
        }
        AcrCmd::Trial {
            key,
            arm,
            out,
            stop_on_failure,
        } => {
            let arms = match arm {
                Some(arm) => vec![arm],
                None => vec![
                    trial::Arm::OneSize,
                    trial::Arm::Sized,
                    trial::Arm::Separated,
                ],
            };
            let pipeline = |tree: &Path, key: &trial::Key, arm| {
                pipeline_arm(paths, tree, key, arm, stop_on_failure)
            };
            for arm in arms {
                let outcome = trial::run(paths, &key, arm, &pipeline)?;
                let line = serde_json::to_string(&outcome)?;
                println!("{line}");
                if let Some(out) = &out {
                    let mut file = fs::OpenOptions::new().create(true).append(true).open(out)?;
                    std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())?;
                }
            }
        }
        AcrCmd::Check { key } => {
            if !check(paths, &key)? {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `acrbus acr check`: prints how each session did. Returns whether all did
/// as they should.
/// A check's findings for a review's session, with reading outside its
/// folders taken out unless its bridge confines it: an agent nothing keeps
/// in may read outside by the person's choice, which a check says rather
/// than fails. Returns the rest, and whether it read outside.
fn outside_is_a_note(wrong: Vec<String>, confined: bool) -> (Vec<String>, bool) {
    if confined {
        return (wrong, false);
    }
    let read_outside = wrong.iter().any(|w| w == trial::READ_OUTSIDE);
    let rest = wrong
        .into_iter()
        .filter(|w| w != trial::READ_OUTSIDE)
        .collect();
    (rest, read_outside)
}

fn check(paths: &Paths, key_path: &Path) -> Result<bool> {
    let (key, s) = trial::copy(paths, trial::resolved(trial::read_key(key_path)?)?)?;
    let canary = trial::Canary::new(paths)?;
    let mut ok = true;
    let mut report = |what: &str, started: std::time::Instant, wrong: Result<Vec<String>>| {
        let secs = started.elapsed().as_secs();
        match wrong {
            Ok(wrong) if wrong.is_empty() => println!("{what}: ok, in {secs} s"),
            Ok(wrong) => {
                ok = false;
                println!("{what}: wrong, in {secs} s");
                for w in wrong {
                    println!("  {w}");
                }
            }
            Err(e) => {
                ok = false;
                println!("{what}: failed after {secs} s: {e:#}");
            }
        }
    };
    // Only a trial needs a builder of its own: in a review, the session
    // that runs it is the builder.
    if let Some(why) = trial::builder_missing(paths) {
        println!(
            "a trial's builder session: not checked, since {why}. A review doesn't need it: \
             the session that runs `{APP_NAME} acr run` is its builder"
        );
    } else {
        let started = std::time::Instant::now();
        let wrong = trial::check_session(paths, &s, &key, &canary);
        report("a builder's session", started, wrong);
    }

    // The separated design's bindings: one call for each bridge and set of
    // tools, since that's what decides how a session starts.
    let target = Target {
        repo: s.tree.clone(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        reattack: None,
        spend_cap_usd: None,
        review_scope: None,
        bindings: Vec::new(),
        description: None,
        pr: None,
        brief: None,
    };
    let (cfg, _) = load_config(paths, &target, &flags(&target))?;
    let mut names: Vec<&String> = cfg
        .bindings
        .values()
        .chain(cfg.lenses.iter().filter_map(|l| l.binding.as_ref()))
        .collect();
    names.sort();
    names.dedup();
    // A reviewer that can change files works in a copy of its own, which
    // its bridge must let it into, so it's checked there.
    let reviewers: Vec<&str> = cfg
        .lenses
        .iter()
        .filter_map(|l| cfg.binding_name(Role::Reviewer, Some(l)).ok())
        .collect();
    let in_copy = |name: &str, b: &config::Binding| b.changes_files() && reviewers.contains(&name);
    let mut chosen: Vec<(&String, &config::Binding)> = Vec::new();
    for name in names {
        let Some(b) = cfg.allowed_bindings.get(name) else {
            continue;
        };
        if !chosen.iter().any(|(n, c)| {
            c.agent == b.agent && c.tools == b.tools && in_copy(n, c) == in_copy(name, b)
        }) {
            chosen.push((name, b));
        }
    }
    let mut allowed = AllowedFor {
        paths,
        tree: s.tree.clone(),
        bridges: Vec::new(),
    };
    for (_, b) in &chosen {
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if !allowed.bridges.contains(&bridge) {
            allow_trials(paths, &bridge)?;
            crate::setup::acr_repo(paths, &s.tree, true, &bridge)?;
            allowed.bridges.push(bridge);
        }
    }
    allow_copies(paths, &cfg)?;
    let copies = copies_root(paths).join(crate::util::stamped(&format!("check-{}", new_id())));
    let caller = BridgeCaller::new(paths)?;
    for (name, b) in chosen {
        // Without a list, it gets whatever its bridge gives, a shell maybe.
        let bash = b
            .tools
            .as_ref()
            .is_none_or(|t| t.iter().any(|t| t == "Bash"));
        let binding = config::Binding {
            timeout: Some(trial::CHECK_SECS),
            ..b.clone()
        };
        let in_copy = in_copy(name, b);
        let tree = if in_copy {
            let copy = copies.join(s.tree.file_name().unwrap_or_default());
            change::copy_of(&s.tree, &copy)?;
            copy
        } else {
            s.tree.clone()
        };
        let prompt = trial::check_prompt(&tree, &canary.path, bash, None);
        let started = std::time::Instant::now();
        let answer = caller.call(&Call {
            role: Role::Reviewer,
            binding_name: name,
            binding: &binding,
            prompt: &prompt,
            artifact: None,
            writes: false,
            no_tools: false,
            relay_tokens: None,
            repo: &tree,
            label: format!(
                "the check: {name}{place}",
                place = if in_copy { ", in a copy" } else { "" }
            ),
        });
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent);
        let confined = crate::bridge::load(paths, bridge).is_ok_and(|set| {
            set.args
                .iter()
                .chain(&set.extra_args)
                .any(|a| a == "--settings")
        });
        let mut outside = false;
        let mut run_id = None;
        let wrong = answer.map(|a| {
            let (wrong, read_outside) = outside_is_a_note(
                trial::check_answer(&a.text, &key.head, &canary.secret, bash),
                confined,
            );
            outside = read_outside;
            run_id = (!wrong.is_empty()).then_some(a.run_id);
            wrong
        });
        let place = if in_copy {
            ", in a copy of its own"
        } else {
            ""
        };
        report(
            &format!("{name} through {}{place}", b.agent),
            started,
            wrong,
        );
        // Kept by its bridge, to see what the agent did.
        if let Some(id) = run_id {
            println!(
                "  its run: `{APP_NAME} bridge log {bridge} {id}`, and its session in {}",
                crate::bridge::state_dir(paths, bridge)
                    .join("runs")
                    .join(&id)
                    .display()
            );
        }
        if outside {
            println!(
                "  note: it can read files outside its folders. Nothing keeps this agent's \
                 reads to the code under review, which is yours to judge"
            );
        }
    }
    // The copies stay, to be looked at, until the daemon's cleanup.
    // A review to convergence's steelman, which writes in the copy.
    if let Some(b) = cfg.allowed_bindings.get(STEELMAN_BINDING) {
        let bridge = b.agent.split('@').next().unwrap_or(&b.agent).to_string();
        if !allowed.bridges.contains(&bridge) {
            allow_trials(paths, &bridge)?;
            crate::setup::acr_repo(paths, &s.tree, true, &bridge)?;
            allowed.bridges.push(bridge);
        }
        let binding = config::Binding {
            timeout: Some(trial::CHECK_SECS),
            ..b.clone()
        };
        let result = s.tree.join(".acr-check-steelman.json");
        let prompt = trial::check_prompt(&s.tree, &canary.path, true, Some(&result));
        let started = std::time::Instant::now();
        let answer = caller.call(&Call {
            role: Role::Orchestrator,
            binding_name: STEELMAN_BINDING,
            binding: &binding,
            prompt: &prompt,
            artifact: None,
            writes: true,
            no_tools: false,
            relay_tokens: None,
            repo: &s.tree,
            label: "the check: the steelman".to_string(),
        });
        let wrong = answer.map(|a| {
            let mut wrong = trial::check_answer(&a.text, &key.head, &canary.secret, true);
            if fs::read_to_string(&result).is_err() {
                wrong.push(format!("it didn't write {} in the copy", result.display()));
            }
            wrong
        });
        report(
            &format!("{STEELMAN_BINDING} through {}, writing", b.agent),
            started,
            wrong,
        );
    }
    drop(allowed);
    if !ok {
        println!("{}", s.kept());
    }
    Ok(ok)
}

/// The most rounds a review to convergence may take: a cap no review should
/// reach, so one that does is a review that isn't converging.
const CONVERGE_CAP: u32 = 20;

/// The Orchestrator's binding in a review to convergence. It must be able to
/// write, since its steelman folds findings into the change.
pub const STEELMAN_BINDING: &str = "steelman";

/// One round of the pipeline, as a change first comes out, with nobody
/// fixing anything and escalations taken as seen. Leaves nothing behind.
fn one_round(paths: &Paths, target: &Target, description: Option<String>) -> Result<Run> {
    pipeline_run(paths, target, description, false, false, false)
}

/// The pipeline with nobody in the loop: one round, or to convergence. An
/// escalation is recorded and taken as seen, and the review goes on.
fn pipeline_run(
    paths: &Paths,
    target: &Target,
    description: Option<String>,
    converge: bool,
    keep_logs: bool,
    stop_on_failure: bool,
) -> Result<Run> {
    let mut flags = flags(target);
    flags.max_rounds = Some(if converge { CONVERGE_CAP } else { 1 });
    flags.human_gate = Some(HumanGate::Exception);
    let (cfg, source) = load_config(paths, target, &flags)?;
    cfg.check_bindings()?;
    check_machine(paths, &cfg, &target.repo, false)?;
    if converge {
        let name = cfg.binding_name(Role::Orchestrator, None)?;
        if !cfg.binding(Role::Orchestrator, None)?.writes {
            bail!(
                "a review to convergence needs an Orchestrator that can fold findings in, \
                 and {name} can't: give it `writes: true` in acr.yaml"
            );
        }
    }
    change::checked_out(
        &target.repo,
        &target.base,
        target.head.as_deref(),
        cfg.review_scope,
    )?;
    allow_copies(paths, &cfg)?;
    let id = new_id();
    let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
    let mut s = setting(paths, &cfg, &id, Some(list))?;
    s.description = description;
    s.converge = converge;
    s.stop_on_failure = stop_on_failure;
    // A trial's arms are measured blind: none reads what another learned.
    s.learned = None;
    s.builder_brief = target
        .brief
        .as_deref()
        .map(stages::read_builder_brief)
        .transpose()?;
    // Under way until this ends, so an update to acrbus waits for it.
    let _active = keep::active_guard(paths, &id)?;
    let (repo, base, head) = (&target.repo, target.base.as_str(), target.head.as_deref());
    let change = reload(&cfg, repo, base, head)?;
    let mut caller = BridgeCaller::new(paths)?;
    caller.keep_logs = keep_logs;
    let mut run = flow::start(&id, repo, base, head, &s, &change, &caller)?;
    // After a steelman, the change is the working tree, with any file it
    // added in the diff too.
    let reloading = |fixed: bool| {
        if fixed {
            change::git(repo, &["add", "--intent-to-add", "--all"])?;
        }
        reload(&cfg, repo, base, head.filter(|_| !fixed))
    };
    loop {
        for e in &mut run.escalations {
            e.seen = true;
        }
        flow::drive(&mut run, &s, &caller, &reloading)?;
        match run.waiting {
            None => break,
            Some(flow::Gate::Escalation) => {}
            Some(gate) => bail!(
                "run {} is waiting at {gate:?}, which nobody here answers",
                run.id
            ),
        }
    }
    for e in &mut run.escalations {
        e.seen = true;
    }
    save(paths, &run, &cfg)?;
    // Forced through with nobody in the loop, it says nothing about how many
    // rounds reviews take.
    keep::finished(paths, &run, false)?;
    Ok(run)
}

/// Allows a trial's worktree through every local bridge its bindings use,
/// for as long as it lives.
struct AllowedFor<'a> {
    paths: &'a Paths,
    tree: PathBuf,
    bridges: Vec<String>,
}

impl Drop for AllowedFor<'_> {
    fn drop(&mut self) {
        for bridge in &self.bridges {
            let _ = crate::setup::acr_repo(self.paths, &self.tree, false, bridge);
        }
    }
}

/// What each lens means in the repository at `tree`, from its review config,
/// for a trial's builder, which is otherwise told only the dimensions. Empty
/// if the repository has none.
fn lens_criteria(paths: &Paths, tree: &Path, key: &trial::Key) -> Vec<(String, String)> {
    let target = Target {
        repo: tree.to_path_buf(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        reattack: None,
        spend_cap_usd: None,
        review_scope: None,
        bindings: Vec::new(),
        description: None,
        pr: None,
        brief: None,
    };
    match load_config(paths, &target, &flags(&target)) {
        Ok((cfg, _)) => cfg
            .lenses
            .iter()
            .map(|l| (l.id.clone(), l.criteria.clone()))
            .collect(),
        Err(e) => {
            eprintln!("{APP_NAME}: no review criteria for the builder: {e:#}");
            Vec::new()
        }
    }
}

/// The pipeline's designs on a trial's worktree: one round, or to
/// convergence.
fn pipeline_arm(
    paths: &Paths,
    tree: &Path,
    key: &trial::Key,
    arm: trial::Arm,
    stop_on_failure: bool,
) -> Result<trial::Ran> {
    let converge = arm == trial::Arm::Converge;
    let target = Target {
        repo: tree.to_path_buf(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        reattack: None,
        spend_cap_usd: None,
        review_scope: None,
        bindings: if converge {
            vec![(Role::Orchestrator, STEELMAN_BINDING.to_string())]
        } else {
            Vec::new()
        },
        description: None,
        pr: None,
        brief: key.brief.clone(),
    };
    let (cfg, _) = load_config(paths, &target, &flags(&target))?;
    let mut bridges: Vec<String> = cfg
        .bindings
        .values()
        .filter_map(|name| cfg.allowed_bindings.get(name))
        .map(|b| b.agent.split('@').next().unwrap_or(&b.agent).to_string())
        .collect();
    bridges.sort();
    bridges.dedup();
    let mut allowed = AllowedFor {
        paths,
        tree: tree.to_path_buf(),
        bridges: Vec::new(),
    };
    for bridge in &bridges {
        allow_trials(paths, bridge)?;
        crate::setup::acr_repo(paths, tree, true, bridge)?;
        allowed.bridges.push(bridge.clone());
    }
    // The key's context, frozen before any review, stands in for the pull
    // request's description, which may have changed since.
    let run = pipeline_run(
        paths,
        &target,
        Some(key.context.clone()),
        converge,
        true,
        stop_on_failure,
    )?;
    drop(allowed);
    let mut found = Vec::new();
    let mut failures: Vec<String> = run.escalations.iter().map(|e| e.reason.clone()).collect();
    let mut notes: Vec<String> = Vec::new();
    for lens in run.lenses.values() {
        for round in &lens.rounds {
            for failure in &round.failures {
                let failure = format!("{}, round {}: {failure}", lens.id, round.round);
                if !failures.iter().any(|f| f.contains(&failure)) {
                    failures.push(failure);
                }
            }
            for note in &round.notes {
                notes.push(format!("{}, round {}: {note}", lens.id, round.round));
            }
            for f in &round.findings {
                found.push(trial::Found {
                    dimension: lens.id.clone(),
                    title: f.title.clone(),
                    detail: f.detail.clone(),
                    file: f.file.clone().unwrap_or_default(),
                    blocker: f.blocks(),
                    round: round.round,
                });
            }
        }
    }
    let mut counts = trial::Counts::default();
    let mut models = Vec::new();
    let mut by_round: Vec<u64> = Vec::new();
    for u in &run.usage {
        counts.input += u.tokens_in;
        counts.cache_read += u.cache_read;
        counts.cache_write += u.cache_write;
        counts.output += u.tokens_out;
        if let Some(m) = &u.model
            && !models.contains(m)
        {
            models.push(m.clone());
        }
        let at = u.round.max(1) as usize - 1;
        if by_round.len() <= at {
            by_round.resize(at + 1, 0);
        }
        by_round[at] += u.tokens();
    }
    counts.total = counts.input + counts.output + counts.cache_read + counts.cache_write;
    let converged = if converge {
        // What the steelmans left, as a patch on the key's head.
        change::git(tree, &["add", "--all"])?;
        let patch = change::git(tree, &["diff", "--cached", "--binary", &key.head])?;
        let summary = trial::Converged {
            rounds: run.round,
            converged: run.status == flow::Status::Confirmed,
            run: run.id.clone(),
            patch: None,
            tokens_by_round: by_round,
            steelmans: run.steelmans.clone(),
            disagreements: run.disagreements.clone(),
        };
        Some((summary, patch))
    } else {
        None
    };
    Ok(trial::Ran {
        findings: found,
        tokens: counts,
        models,
        failures,
        converged,
        notes,
    })
}

/// `acr configure` and `acr calibrate`: the configurer reviews the repository
/// in a clone of its own, with its history, and proposes its review config,
/// which waits beside the config for a person to read and accept. A
/// recalibration also hands it the config and the feedback not yet folded in.
fn configure_repo(paths: &Paths, repo: &Path, recalibrate: bool) -> Result<()> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("finding {}", repo.display()))?;
    let name = change::name(&repo)?;
    let user = config::load_user(&user_config_path(paths))?;
    let (binding_name, binding) = configure::binding(&user)?;
    let kept_at = outside_config(paths, &repo)?;
    let current = fs::read_to_string(&kept_at).ok();
    // The configurer is held to the models this repository's review may
    // use: its config's, or the user's for every repository.
    let configuring = vec![(binding_name.as_str(), &binding)];
    let allowed = current
        .as_deref()
        .and_then(|text| config::parse_repo(text).ok())
        .and_then(|p| p.allowed_models)
        .or_else(|| {
            user.defaults
                .as_ref()
                .and_then(|d| d.allowed_models.clone())
        })
        .unwrap_or_default();
    config::check_models(&allowed, &configuring)?;
    let record = calibration::load(&paths.state, &name)?;
    let feedback = if recalibrate {
        if current.is_none() {
            bail!(
                "{name} has no review config at {} to recalibrate: `{APP_NAME} acr configure \
                 --repo {}` proposes one",
                kept_at.display(),
                repo.display()
            );
        }
        let feedback = configure::feedback(&paths.state, &name, record.feedback_folded_at)?;
        if feedback.is_empty() {
            bail!(
                "there's no feedback on {name}'s reviews since its config was last \
                 recalibrated: say how a review did with `{APP_NAME} acr feedback <review>`"
            );
        }
        feedback
    } else {
        Vec::new()
    };
    let folds = feedback.iter().map(|f| f.at).max();
    let root = copies_root(paths).join(crate::util::stamped(&format!(
        "configure-{}",
        crate::util::new_id()
    )));
    // The clone stays, with what the configurer did in it, until the
    // daemon's cleanup.
    fs::create_dir_all(&root)?;
    let clone = root.join(&name);
    let cloned = std::process::Command::new("git")
        .args(["clone", "--quiet", "--no-hardlinks"])
        .arg(&repo)
        .arg(&clone)
        .output()
        .context("running git clone")?;
    if !cloned.status.success() {
        bail!(
            "cloning {} for the configurer: {}",
            repo.display(),
            String::from_utf8_lossy(&cloned.stderr).trim()
        );
    }
    let bridge = binding
        .agent
        .split('@')
        .next()
        .unwrap_or(&binding.agent)
        .to_string();
    crate::setup::acr_dir(paths, &copies_root(paths), &bridge)?;
    // An hour of the configurer's work is no time to find its bridge says
    // no. It runs here, as every role does, and works in its clone.
    let machine = this_machine(paths)?;
    for (name, b) in &configuring {
        let on = b
            .agent
            .split_once('@')
            .map(|(_, m)| m.split('.').next().unwrap_or(m));
        if on.is_some_and(|m| !m.eq_ignore_ascii_case(&machine)) {
            bail!(
                "ACR runs on one machine: binding {name} reaches {}, but the code is on \
                 {machine}. Bind the configurer to an agent on {machine}.",
                b.agent
            );
        }
    }
    check_bridges(paths, &configuring, &machine, &copies_root(paths), false)?;
    let caller = BridgeCaller::new(paths)?;
    let brief = configure::prompt(&name, current.as_deref().filter(|_| recalibrate), &feedback);
    eprintln!(
        "{APP_NAME}: {binding_name} is reviewing {name} to {}. It reads the whole \
         repository, so this takes a while.",
        if recalibrate {
            format!(
                "fold {} piece(s) of feedback into its config",
                feedback.len()
            )
        } else {
            "configure its review".to_string()
        }
    );
    let proposal = configure::propose(&caller, &user, (&binding_name, &binding), &clone, &brief)?;
    let waiting = configure::save(&kept_at, &proposal, folds)?;
    if current.is_some() {
        let diff = std::process::Command::new("git")
            .args(["diff", "--no-index", "--no-color", "--"])
            .arg(&kept_at)
            .arg(&waiting.config)
            .output()?;
        println!("{}", String::from_utf8_lossy(&diff.stdout));
    }
    println!("{}", proposal.why);
    println!(
        "\nProposed a review config for {name}: {}, with its reasons in {}. Read them, change \
         the config if you like, then accept it with `{APP_NAME} acr calibration --repo {} \
         --accept`.",
        waiting.config.display(),
        waiting.why.display(),
        repo.display()
    );
    Ok(())
}

fn calibrate(
    paths: &Paths,
    key_path: &Path,
    per_finding: bool,
    first_pass: Option<&str>,
) -> Result<serde_json::Value> {
    use serde_json::json;
    let (key, s) = trial::copy(paths, trial::resolved(trial::read_key(key_path)?)?)?;
    if key.findings.is_empty() {
        bail!("{} has no findings to calibrate on", key_path.display());
    }
    let target = Target {
        repo: s.tree.clone(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
        reattack: None,
        spend_cap_usd: None,
        review_scope: None,
        bindings: Vec::new(),
        description: None,
        pr: None,
        brief: None,
    };
    let (cfg, source) = load_config(paths, &target, &flags(&target))?;
    let judge = cfg.binding(Role::Judge, None)?;
    let bridge = judge
        .agent
        .split('@')
        .next()
        .unwrap_or(&judge.agent)
        .to_string();
    let mut allowed = AllowedFor {
        paths,
        tree: s.tree.clone(),
        bridges: Vec::new(),
    };
    allow_trials(paths, &bridge)?;
    crate::setup::acr_repo(paths, &s.tree, true, &bridge)?;
    allowed.bridges.push(bridge);
    let change = reload(&cfg, &s.tree, &key.base, Some(&key.head))?;
    let intent = change::intent(Some(&key.context), &change.commits);
    let lines = stages::changed_lines(&change.diff);
    let artifact = s.dir.join("calibrate.diff");
    fs::write(&artifact, format!("{intent}# The diff\n\n{}", change.diff))?;
    let decided = not_findings(&cfg, &source, &s.tree, &key.base)?;
    let (text, _) = repo_layer(paths, &s.tree, Some(&key.base), None)?;
    let fp = fingerprint(&cfg, &text, &decided)?;
    let mut caller = BridgeCaller::new(paths)?;
    caller.keep_logs = true;
    let tag = regex::Regex::new(r"\[K(\d+)\]").expect("a fixed pattern");
    let mut judged: Vec<Option<(bool, String)>> = vec![None; key.findings.len()];
    let mut each: Vec<Option<(bool, String)>> = vec![None; key.findings.len()];
    let mut dimensions: Vec<&str> = key.findings.iter().map(|f| f.dimension.as_str()).collect();
    dimensions.sort();
    dimensions.dedup();
    let judge_name = cfg.binding_name(Role::Judge, None)?;
    // The first pass to try, if any: named here, or the repository's.
    let first_name = first_pass
        .map(str::to_string)
        .or_else(|| cfg.savings.first_pass_judge.clone());
    let first = match &first_name {
        Some(name) => Some((
            name.as_str(),
            cfg.allowed_bindings.get(name).with_context(|| {
                format!("--first-pass: binding {name} isn't in your allowed_bindings")
            })?,
        )),
        None => None,
    };
    let mut cascade: Vec<Option<(bool, String)>> = vec![None; key.findings.len()];
    let (mut settled, mut sent_on, mut dimension_count) = (0usize, 0usize, 0usize);
    let keep_tags = "\n\nKeep each finding's title as it's given, with its [K..] tag, and judge \
                     each on its own: don't merge them.";
    let raw = |i: usize, f: &trial::KeyFinding| roles::Raw {
        title: format!(
            "[K{i}] {}",
            f.what
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>()
        ),
        detail: f.what.clone(),
        file: None,
        line: None,
    };
    // A Judge's ratings, by the key's findings they name: `only`, for a
    // call given one finding, takes its first rating whatever its title.
    let rule = |lens: &config::Lens,
                ratings: Vec<roles::Rating>,
                only: Option<usize>,
                into: &mut Vec<Option<(bool, String)>>| {
        for r in ratings {
            let tagged = tag
                .captures(&r.title)
                .and_then(|c| c[1].parse::<usize>().ok())
                .filter(|i| *i < into.len());
            let Some(i) = tagged.or(only) else {
                continue;
            };
            if only.is_some() && into[i].is_some() {
                continue;
            }
            let flow::Ruled {
                severity, action, ..
            } = flow::ruling(
                &cfg,
                lens,
                &flow::grounded(r, &lines, lens.blocker.as_deref()),
            );
            let blocks = matches!(
                action,
                config::Action::Blocks | config::Action::BlocksUnlessWaived
            );
            into[i] = Some((blocks, format!("{severity:?}")));
        }
    };
    for d in dimensions {
        let lens = cfg
            .lenses
            .iter()
            .find(|l| l.id == d)
            .cloned()
            .unwrap_or(config::Lens {
                id: d.to_string(),
                criteria: "(this repository's review config has no criteria for it)".into(),
                attack: None,
                blocker: None,
                triggers: config::Triggers::default(),
                binding: None,
                severity: Default::default(),
                include_generated: false,
            });
        let mine: Vec<(usize, roles::Raw)> = key
            .findings
            .iter()
            .enumerate()
            .filter(|(_, f)| f.dimension == d)
            .map(|(i, f)| (i, raw(i, f)))
            .collect();
        let raws: Vec<roles::Raw> = mine.iter().map(|(_, r)| r.clone()).collect();
        let prompt = roles::judge_prompt(&lens, &[raws], &decided)? + keep_tags;
        let answer = caller.call(&Call {
            role: Role::Judge,
            binding_name: cfg.binding_name(Role::Judge, None)?,
            binding: judge,
            prompt: &prompt,
            artifact: Some(&artifact),
            writes: false,
            no_tools: false,
            relay_tokens: None,
            repo: &s.tree,
            label: format!("{}: the Judge, on the key's findings", lens.id),
        })?;
        rule(&lens, roles::read_ratings(&answer.text)?, None, &mut judged);
        dimension_count += 1;
        if let Some((name, binding)) = first {
            // The first pass on the same prompt, with its note. Where it
            // settles the dimension its verdicts stand; where it sends it on,
            // the Judge's, from the call above, stand.
            let first_prompt = format!("{prompt}{}", roles::FIRST_PASS_NOTE);
            let ratings = caller
                .call(&Call {
                    role: Role::Judge,
                    binding_name: name,
                    binding,
                    prompt: &first_prompt,
                    artifact: Some(&artifact),
                    writes: false,
                    no_tools: false,
                    relay_tokens: None,
                    repo: &s.tree,
                    label: format!("{}: the first-pass Judge, on the key's findings", lens.id),
                })
                .and_then(|a| roles::read_ratings(&a.text));
            match ratings {
                Ok(ratings)
                    if !ratings
                        .iter()
                        .any(|r| flow::first_pass_escalates(&cfg, &lens, r, &lines)) =>
                {
                    settled += 1;
                    rule(&lens, ratings, None, &mut cascade);
                }
                other => {
                    if let Err(e) = other {
                        eprintln!("{APP_NAME}: {}'s first pass: {e:#}", lens.id);
                    }
                    sent_on += 1;
                    for (i, _) in &mine {
                        cascade[*i] = judged[*i].clone();
                    }
                }
            }
        }
        if !per_finding {
            continue;
        }
        // Each finding on its own, all at once, as a review's are.
        let answers: Vec<(usize, Result<Vec<roles::Rating>>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = mine
                .iter()
                .map(|(i, r)| {
                    let (caller, lens, artifact, tree) = (&caller, &lens, &artifact, &s.tree);
                    let decided = &decided;
                    scope.spawn(move || {
                        let prompt = roles::join(&roles::judge_finding_parts(lens, r, "", decided));
                        let rated = caller.call(&Call {
                            role: Role::Judge,
                            binding_name: judge_name,
                            binding: judge,
                            prompt: &prompt,
                            artifact: Some(artifact),
                            writes: false,
                            no_tools: false,
                            relay_tokens: None,
                            repo: tree,
                            label: format!("{}: the Judge, on key finding {i}", lens.id),
                        });
                        (*i, rated.and_then(|a| roles::read_ratings(&a.text)))
                    })
                })
                .collect();
            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        for (i, rated) in answers {
            match rated {
                Ok(ratings) => rule(&lens, ratings, Some(i), &mut each),
                Err(e) => eprintln!("{APP_NAME}: key finding {i}'s own Judge: {e:#}"),
            }
        }
    }
    drop(allowed);
    let tally = |judged: &[Option<(bool, String)>]| {
        let mut rows = Vec::new();
        let (mut agreed_b, mut missed, mut over, mut agreed_n, mut unrated) = (0, 0, 0, 0, 0);
        for (f, j) in key.findings.iter().zip(judged) {
            match (f.blocker, j.as_ref().map(|(b, _)| *b)) {
                (true, Some(true)) => agreed_b += 1,
                (true, Some(false)) => missed += 1,
                (false, Some(true)) => over += 1,
                (false, Some(false)) => agreed_n += 1,
                (_, None) => unrated += 1,
            }
            rows.push(json!({
                "id": f.id, "dimension": f.dimension, "key_blocker": f.blocker,
                "judged_blocker": j.as_ref().map(|(b, _)| *b),
                "severity": j.as_ref().map(|(_, s)| s.clone()),
            }));
        }
        let dropped = judged
            .iter()
            .filter(|j| j.as_ref().is_some_and(|(_, s)| s == "Rejected"))
            .count();
        json!({
            "agreed_blockers": agreed_b, "missed_blockers": missed, "overrated": over,
            "agreed_non_blockers": agreed_n, "unrated": unrated, "dropped": dropped,
            "findings": rows,
        })
    };
    let mut result = tally(&judged);
    result["key"] = json!(
        key_path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
    );
    result["repo"] = json!(change::name(&s.tree)?);
    result["repo_path"] = json!(key.repo);
    result["fingerprint"] = json!(fp);
    if per_finding {
        let mut alone = tally(&each);
        let pairs: Vec<(bool, bool)> = judged
            .iter()
            .zip(&each)
            .filter_map(|(a, b)| Some((a.as_ref()?.0, b.as_ref()?.0)))
            .collect();
        alone["compared"] = json!(pairs.len());
        alone["same_as_per_dimension"] = json!(pairs.iter().filter(|(a, b)| a == b).count());
        alone["differ"] = json!(
            key.findings
                .iter()
                .zip(judged.iter().zip(&each))
                .filter(|(_, (a, b))| matches!((a, b), (Some(a), Some(b)) if a.0 != b.0))
                .map(|(f, _)| f.id.clone())
                .collect::<Vec<_>>()
        );
        result["per_finding"] = alone;
    }
    if let Some(name) = &first_name {
        let mut with = tally(&cascade);
        with["binding"] = json!(name);
        with["dimensions"] = json!(dimension_count);
        with["settled"] = json!(settled);
        with["sent_on"] = json!(sent_on);
        with["differ_from_judge"] = json!(
            key.findings
                .iter()
                .zip(judged.iter().zip(&cascade))
                .filter(|(_, (a, b))| matches!((a, b), (Some(a), Some(b)) if a.0 != b.0))
                .map(|(f, _)| f.id.clone())
                .collect::<Vec<_>>()
        );
        result["first_pass"] = with;
    }
    Ok(result)
}

/// What a calibration of a repository is made against: its config and
/// decided non-issues, and its Judge, as its binding, prompt and rules.
fn fingerprint(cfg: &Config, text: &str, decided: &str) -> Result<calibration::Fingerprint> {
    let judge = cfg.binding(Role::Judge, None)?;
    // With a blocker definition, so the part of the prompt that gives it is
    // fingerprinted too.
    let lens = config::Lens {
        id: "lens".into(),
        criteria: String::new(),
        attack: None,
        blocker: Some("a blocker".into()),
        triggers: config::Triggers::default(),
        binding: None,
        severity: Default::default(),
        include_generated: false,
    };
    let mut prompt = roles::judge_prompt(&lens, &[Vec::new()], "")?;
    // A Judge for each finding is a Judge with its own brief. The default
    // adds nothing, so a calibration made before it was one stands.
    if cfg.facts.judges == config::JudgeMode::PerFinding {
        let claim = roles::Raw {
            title: String::new(),
            detail: String::new(),
            file: None,
            line: None,
        };
        prompt.push_str(&roles::join(&roles::judge_finding_parts(
            &lens, &claim, "", "",
        )));
    }
    Ok(calibration::Fingerprint {
        config: calibration::hash(&[text, decided]),
        judge: calibration::hash(&[
            &judge.agent,
            judge.model.as_deref().unwrap_or_default(),
            judge.effort.as_deref().unwrap_or_default(),
            &prompt,
            &flow::rules_source(),
        ]),
    })
}

/// Whether `acr run` may rule on a repository: never calibrated, or its
/// config changed since, it refuses unless told to go anyway. A changed
/// Judge only gets a note: that's acrbus's doing, not the repository's.
fn calibrated(
    paths: &Paths,
    name: &str,
    now: &calibration::Fingerprint,
    anyway: bool,
) -> Result<Option<String>> {
    use calibration::Standing;
    let record = calibration::load(&paths.state, name)?;
    let how = format!(
        "`{APP_NAME} acr configure` proposes a config from a review of the whole repository, \
         and `{APP_NAME} acr calibration --accept` accepts it, or a config you wrote yourself \
         (section 7 of docs/acr-repo-setup.md)"
    );
    let unfolded = configure::feedback(&paths.state, name, record.feedback_folded_at)?.len();
    let feedback = if unfolded > 0 {
        format!(
            " {unfolded} piece(s) of your feedback on its reviews aren't in its config yet: \
             `{APP_NAME} acr calibrate` folds them in."
        )
    } else {
        String::new()
    };
    match calibration::standing(&record, now) {
        Standing::Calibrated => Ok((!feedback.is_empty()).then(|| format!("Note:{feedback}"))),
        Standing::JudgeChanged => Ok(Some(format!(
            "Note: the Judge (its binding, prompt or rules) has changed since {name}'s config was \
             accepted. {} Check its config still fits, then accept it again with `{APP_NAME} acr \
             calibration --accept`.{feedback}",
            calibration::ON_AN_EARLIER_JUDGE
        ))),
        _ if anyway => Ok(Some(format!(
            "Note: {name}'s config isn't one accepted for it, so it hasn't been checked for \
             this repository."
        ))),
        Standing::Never => bail!(
            "{name}'s review config was never accepted for it. {how}. `--uncalibrated` reviews \
             it anyway."
        ),
        Standing::ConfigChanged => bail!(
            "{name}'s review config has changed since it was accepted. Read it, then accept it \
             with `{APP_NAME} acr calibration --accept`. `--uncalibrated` reviews it anyway."
        ),
    }
}

/// A run's config, as it was when it started: resuming can't pick up a
/// config that's changed since.
fn stored_config(bytes: &[u8]) -> Result<Config> {
    serde_json::from_slice(bytes).map_err(|e| anyhow!("reading the run's config: {e}"))
}

#[cfg(test)]
mod tests {
    use super::change::tests::{branch, commit, repo_with};
    use super::*;

    #[test]
    fn a_call_that_couldnt_reach_its_model_is_told_by_what_its_agent_said() {
        for said in [
            "Error: fetch failed (ECONNREFUSED 127.0.0.1:8080)",
            "getaddrinfo ENOTFOUND api.example.com",
            "Connection error.",
            "socket hang up",
        ] {
            assert!(unreachable_said(said), "{said}");
        }
        for said in ["the answer has no JSON", "exit 1: out of memory"] {
            assert!(!unreachable_said(said), "{said}");
        }
    }

    const LENS: &str = "version: 1\ntiers: [{id: t, match: {paths: ['**']}, fan_out: 1}]\n";

    fn lens(id: &str) -> String {
        format!(
            "{LENS}not_findings: decided.md\nlenses: [{{id: {id}, criteria: c, triggers: {{paths: ['**']}}}}]\n"
        )
    }

    fn paths() -> Paths {
        let dir = std::env::temp_dir().join(format!("tb-acr-paths-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        Paths {
            state: dir.join("state"),
            config: dir.join("config.toml"),
        }
    }

    /// A bridge here, `agent-x`, that may work only in `workdir`, and a
    /// user's bindings: `fits`, which it takes, `edits`, which asks for a
    /// tool it doesn't allow, and `away`, on another machine.
    fn bridge_and_bindings(paths: &Paths, workdir: &Path) -> config::UserConfig {
        let dir = paths.config.parent().unwrap().to_path_buf();
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        let bin = dir.join("agent");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        fs::write(
            &paths.config,
            format!(
                "[bridges.agent-x]\nowner = \"o\"\ncommand = \"{}\"\nargs = [\"--model\", \"{{MODEL}}\"]\nmodel = \"m\"\n\
                 working_directory = \"{h}\"\nhome = \"{h}\"\npath = \"/bin\"\n\
                 [bridges.agent-x.options]\ntools = [\"Read\", \"Bash\"]\n\
                 tools_args = [\"--tools\", \"{{TOOLS}}\"]\n\
                 effort_args = [\"--effort\", \"{{EFFORT}}\"]\n\
                 models = [\"m\"]\nefforts = [\"high\"]\nmax_timeout_secs = 600\n\
                 workdirs = [\"{w}\"]\n",
                bin.display(),
                h = home.display(),
                w = workdir.canonicalize().unwrap().display()
            ),
        )
        .unwrap();
        config::parse_user(
            "version: 1\nallowed_bindings:\n\
             \x20 fits: {agent: agent-x@here, model: m, effort: high, tools: [Read, Bash], timeout: 600}\n\
             \x20 edits: {agent: agent-x@here, model: m, effort: high, tools: [Read, Edit]}\n\
             \x20 away: {agent: agent-x@there, model: m, tools: [Edit]}\n\
             defaults:\n  bindings: {sizer: fits, reviewer: fits, orchestrator: fits, judge: fits}\n",
        )
        .unwrap()
    }

    #[test]
    fn a_reviews_end_says_what_it_waits_for_and_how_to_answer() {
        let cfg = config::resolve(
            &config::parse_user(include_str!("../../examples/acr/user.yaml")).unwrap(),
            config::parse_repo(
                "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                 lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n",
            )
            .unwrap(),
            &Flags::default(),
        )
        .unwrap();
        let run = |status: &str, waiting: Option<&str>, escalations: serde_json::Value| -> Run {
            serde_json::from_value(serde_json::json!({
                "id": "r1", "repo": "/r", "base": "main", "head": null,
                "tier": "core", "tier_why": "the Sizer: it touches storage", "fan_out": 1,
                "round": 1, "stage": "review", "status": status, "waiting": waiting,
                "lenses": {}, "escalations": escalations, "disagreements": [], "usage": []
            }))
            .unwrap()
        };
        let none = serde_json::json!([]);
        let scope = next_steps(&run("waiting", Some("scope"), none.clone()), &cfg);
        assert!(
            scope.contains("approve its scope, tier core")
                && scope.contains("acr ask r1")
                && scope.contains("acr resume r1 --answers"),
            "{scope}"
        );
        // The fold says how to let the Orchestrator fix, only where it can't.
        let fold = next_steps(&run("waiting", Some("fold"), none.clone()), &cfg);
        assert!(
            fold.contains("decide its blockers") && !fold.contains("writes: true"),
            "{fold}"
        );
        // Waiting for the session's own fixes, it says how to say they're in.
        let fix = next_steps(&run("waiting", Some("fix"), none.clone()), &cfg);
        assert!(fix.contains("acr resume r1 --fixed"), "{fix}");
        let mut reads = cfg.clone();
        let name = reads
            .binding_name(Role::Orchestrator, None)
            .unwrap()
            .to_string();
        reads.allowed_bindings.get_mut(&name).unwrap().writes = false;
        let fold = next_steps(&run("waiting", Some("fold"), none.clone()), &reads);
        assert!(fold.contains("writes: true"), "{fold}");
        let up = next_steps(
            &run(
                "waiting",
                Some("escalation"),
                serde_json::json!([{"reason": "a public API change", "seen": false}]),
            ),
            &cfg,
        );
        assert!(up.contains("escalated: a public API change"), "{up}");
        assert!(next_steps(&run("confirmed", None, none.clone()), &cfg).contains("confirmed"));
        let ended = next_steps(&run("stopped", None, none), &cfg);
        assert!(ended.contains("ended with"), "{ended}");
    }

    #[test]
    fn a_waiting_proposal_is_named_not_called_no_config() {
        use super::change::tests::repo_with;
        let paths = paths();
        let repo = repo_with(&[("a.txt", "one\n")]);
        let kept = outside_config(&paths, &repo).unwrap();
        fs::create_dir_all(kept.parent().unwrap()).unwrap();
        fs::write(configure::pending(&kept).config, "version: 1\n").unwrap();
        let asked = AcrCmd::Calibration {
            repo: repo.clone(),
            base: "HEAD".into(),
            config: None,
            accept: false,
        };
        // It says where the proposal is, and exits 1, rather than failing to
        // find a config.
        assert_eq!(super::run(&paths, asked, None).unwrap(), ExitCode::FAILURE);
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_doctor_checks_reviewers_find_the_repositorys_toolchain() {
        use super::change::tests::repo_with;
        let paths = paths();
        let repo = repo_with(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
        let user = bridge_and_bindings(&paths, &repo);
        let cfg = config::resolve(
            &user,
            config::parse_repo(
                "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                 lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n",
            )
            .unwrap(),
            &Flags::default(),
        )
        .unwrap();
        // The test bridge's PATH is /bin, which has no cargo.
        let said = doctor_toolchain(&paths, &cfg, &repo.canonicalize().unwrap());
        assert!(
            said.iter()
                .any(|s| matches!(s, Said::Fix(t) if t.contains("reviewers can't run `cargo`"))),
            "{said:?}"
        );
        // A repository with no toolchain of its own has nothing to check.
        let plain = repo_with(&[("a.txt", "one\n")]);
        assert!(doctor_toolchain(&paths, &cfg, &plain.canonicalize().unwrap()).is_empty());
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&plain);
    }

    #[test]
    fn the_doctor_says_what_to_fix_and_how() {
        use super::change::tests::repo_with;
        let fixes = |said: &[Said]| -> Vec<String> {
            said.iter()
                .filter_map(|s| match s {
                    Said::Fix(t) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        };
        let repo = repo_with(&[("a.txt", "one\n")]);
        let repo = repo.canonicalize().unwrap();
        // Nothing set up: no bus, and no bindings, and it stops there.
        let paths = paths();
        let said = diagnose(&paths, &repo, false);
        let f = fixes(&said);
        assert!(f[0].contains("service install"), "{f:?}");
        assert!(f[1].contains("setup acr"), "{f:?}");
        assert_eq!(f.len(), 2, "{said:?}");
        // Bindings, and a bridge that may work here, but no review config.
        let user = bridge_and_bindings(&paths, &repo);
        let yaml = "version: 1\nallowed_bindings:\n\
            \x20 fits: {agent: agent-x@here, model: m, effort: high, tools: [Read, Bash], timeout: 600}\n\
            defaults:\n  bindings: {sizer: fits, reviewer: fits, orchestrator: fits, judge: fits}\n";
        fs::write(user_config_path(&paths), yaml).unwrap();
        assert!(user.allowed_bindings.contains_key("fits"));
        let said = diagnose(&paths, &repo, false);
        assert!(
            said.contains(&Said::Ok("the agent-x bridge may work in it".into())),
            "{said:?}"
        );
        let f = fixes(&said);
        assert!(
            f.iter().any(|t| t.contains("acr configure --repo")),
            "{f:?}"
        );
        // A repository it may not work in is told to add it.
        let elsewhere = repo_with(&[("b.txt", "two\n")]);
        let f = fixes(&diagnose(&paths, &elsewhere.canonicalize().unwrap(), false));
        assert!(f.iter().any(|t| t.contains("acr add-repo")), "{f:?}");
        // A config, not yet accepted, whose tests pass as you run them but
        // not with the reviewers' bridge's PATH, /bin, which has no git.
        let kept = outside_config(&paths, &repo).unwrap();
        fs::create_dir_all(kept.parent().unwrap()).unwrap();
        fs::write(
            &kept,
            "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
             lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n\
             stages: []\ntests: {command: 'command -v git', timeout: 60}\n",
        )
        .unwrap();
        let said = diagnose(&paths, &repo, true);
        let f = fixes(&said);
        assert!(f.iter().any(|t| t.contains("isn't calibrated")), "{f:?}");
        // What a person needs to judge a long call by, and a cap short
        // enough to cut one off, said, not acted on.
        assert!(
            said.contains(&Said::Ok(
                "the agent-x bridge lets a review's call run up to 10m 0s, and ends one \
                 that's quiet for 20m 0s"
                    .into()
            )),
            "{said:?}"
        );
        assert!(
            said.iter().any(|s| matches!(s, Said::Note(t)
                if t.starts_with("Binding fits ends its calls after 600 s"))),
            "{said:?}"
        );
        assert!(
            said.contains(&Said::Ok(
                "tests.command runs in a copy, as a review runs it".into()
            )),
            "{said:?}"
        );
        assert!(
            f.iter()
                .any(|t| t.contains("pass for you but not as the agent-x bridge's reviewers")),
            "{f:?}"
        );
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn a_binding_its_bridge_would_refuse_stops_a_review_before_it_starts() {
        use super::change::tests::repo_with;
        let paths = paths();
        let repo = repo_with(&[("a.txt", "one\n")]);
        let user = bridge_and_bindings(&paths, &repo);
        let small = "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                     lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n";
        let cfg = |extra: &str| {
            config::resolve(
                &user,
                config::parse_repo(&format!("{small}{extra}")).unwrap(),
                &Flags::default(),
            )
            .unwrap()
        };
        // What a review is checked by before it starts: this machine, and
        // what each binding asks its bridge for, where it will work.
        check_on(&paths, &cfg(""), "here", &repo, false).unwrap();
        let err = format!(
            "{:#}",
            check_on(
                &paths,
                &cfg("bindings: {judge: edits}\n"),
                "here",
                &repo,
                false
            )
            .unwrap_err()
        );
        assert!(
            err.contains("would refuse every call on binding edits"),
            "{err}"
        );
        assert!(
            err.contains("tool \"Edit\" isn't one this agent's config allows"),
            "{err}"
        );
        assert!(err.contains("change the binding in"), "{err}");
        // A review saved with it is told what can help.
        let err = format!(
            "{:#}",
            check_on(
                &paths,
                &cfg("bindings: {judge: edits}\n"),
                "here",
                &repo,
                true
            )
            .unwrap_err()
        );
        assert!(
            err.contains("saved with that binding") && err.contains("--stop"),
            "{err}"
        );
        // Where it works is checked too: a repository the bridge doesn't allow.
        let other = repo_with(&[("b.txt", "two\n")]);
        let err = format!(
            "{:#}",
            check_on(&paths, &cfg(""), "here", &other, false).unwrap_err()
        );
        assert!(err.contains("working directory"), "{err}");
        // A binding on another machine is refused for that, not checked
        // against this machine's bridge of the same name.
        let err = format!(
            "{:#}",
            check_on(
                &paths,
                &cfg("bindings: {judge: away}\n"),
                "here",
                &repo,
                false
            )
            .unwrap_err()
        );
        assert!(err.contains("ACR runs on one machine"), "{err}");
        check_bridges(
            &paths,
            &[("away", &user.allowed_bindings["away"])],
            "here",
            &repo,
            false,
        )
        .unwrap();
        // And every call says it's a review, which a bridge takes only from
        // its own machine.
        assert_eq!(
            options_for(&user.allowed_bindings["fits"]).review,
            Some(true)
        );
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&other);
    }

    #[test]
    fn a_waiting_review_can_be_stopped_whatever_its_agents_and_checkout_are() {
        use super::change::tests::{commit, repo_with};
        let paths = paths();
        let repo = repo_with(&[("a.txt", "one\n")]);
        // The head the review was of, and a commit after it, checked out.
        let head = change::git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        commit(&repo, &[("a.txt", "two\n")], "moved on");
        let user = config::parse_user(include_str!("../../examples/acr/user.yaml")).unwrap();
        let cfg = config::resolve(
            &user,
            config::parse_repo(
                "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                 lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n",
            )
            .unwrap(),
            &Flags::default(),
        )
        .unwrap();
        // Waiting on a head that's no longer checked out, on a machine with
        // no bus to say which machine it is.
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": "r-stop", "repo": repo, "base": "main", "head": head,
            "tier": "a", "tier_why": "", "fan_out": 1, "round": 1, "stage": "review",
            "status": "waiting", "waiting": "escalation", "lenses": {},
            "escalations": [{"reason": "a person should see this", "seen": false}],
            "disagreements": [], "usage": []
        }))
        .unwrap();
        save(&paths, &run, &cfg).unwrap();
        let resume = |stop: bool| AcrCmd::Resume {
            run_id: "r-stop".into(),
            answers: None,
            fixed: false,
            brief: None,
            fold: Vec::new(),
            waive: Vec::new(),
            defer: Vec::new(),
            approve: !stop,
            stop,
            post_pr: None,
        };
        // Going on needs the head checked out, before anything else.
        let err = format!("{:#}", super::run(&paths, resume(false), None).unwrap_err());
        assert!(err.contains("has to be what's checked out"), "{err}");
        super::run(&paths, resume(true), None).unwrap();
        // Ended, and kept to be looked at, packed, but it can't go on.
        let kept = keep::kept(&paths, "r-stop").unwrap();
        let name = kept.file_name().unwrap().to_string_lossy().into_owned();
        let run = crate::util::packed_file(
            &kept,
            &format!("{}/run.json", name.trim_end_matches(".tar.gz")),
        );
        assert!(run.is_ok(), "{name}");
        let err = format!("{:#}", super::run(&paths, resume(true), None).unwrap_err());
        assert!(err.contains("has finished"), "{err}");
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
    }

    /// A process standing in for a review's, reaped as it ends so its ID
    /// isn't held by a zombie. Its pid, and a way to know it's gone.
    fn stand_in(script: &str) -> (i32, std::sync::mpsc::Receiver<()>) {
        stand_in_named(script, "sh")
    }

    /// [`stand_in`], with `name` among the words `ps` shows for it.
    fn stand_in_named(script: &str, name: &str) -> (i32, std::sync::mpsc::Receiver<()>) {
        let mut child = std::process::Command::new("sh")
            .args(["-c", script, name])
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let (done, gone) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = done.send(());
        });
        // Long enough for ps to see it as it is.
        std::thread::sleep(std::time::Duration::from_millis(200));
        (pid, gone)
    }

    #[test]
    fn a_reviews_process_is_known_by_its_mark_and_never_by_another_with_its_id() {
        let paths = paths();
        let (pid, _gone) = stand_in("exec sleep 30");
        // Its own mark, with when it started: it's the review's.
        keep::mark_for(&paths, "r-own", pid, None);
        assert_eq!(keep::live(&paths, "r-own"), Some(pid));
        assert!(keep::active(&paths).contains(&"r-own".to_string()));
        // A mark whose process started at another time names another
        // process now holding the ID: not the review's.
        keep::mark_for(&paths, "r-reused", pid, Some("Mon Jan  1 00:00:00 2001"));
        assert_eq!(keep::live(&paths, "r-reused"), None);
        // An old mark, with no start time, is trusted only for a process
        // running an acr command, which `sleep` isn't.
        keep::mark_for(&paths, "r-old", pid, Some(""));
        assert_eq!(keep::live(&paths, "r-old"), None);
        // One that is, as a review started before marks said when, is.
        let (acr, _acr_gone) = stand_in_named("while :; do sleep 0.2; done", "acr");
        keep::mark_for(&paths, "r-old-acr", acr, Some(""));
        assert_eq!(keep::live(&paths, "r-old-acr"), Some(acr));
        assert_eq!(
            stop_live(
                &paths,
                "r-old-acr",
                &[(libc::SIGTERM, std::time::Duration::from_secs(5))]
            )
            .unwrap(),
            Some(acr)
        );
        // Stopping goes only to the process its mark names.
        assert_eq!(
            stop_live(
                &paths,
                "r-reused",
                &[(libc::SIGTERM, std::time::Duration::from_secs(1))]
            )
            .unwrap(),
            None
        );
        // SAFETY: signal 0 only asks whether the stand-in still runs.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "left alone");
        assert_eq!(
            stop_live(
                &paths,
                "r-own",
                &[(libc::SIGTERM, std::time::Duration::from_secs(5))]
            )
            .unwrap(),
            Some(pid)
        );
        assert_eq!(keep::live(&paths, "r-own"), None);
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn a_process_that_ignores_an_interrupt_is_stopped_by_the_next_signal() {
        let paths = paths();
        let (pid, gone) = stand_in("trap '' INT; while :; do sleep 0.2; done");
        keep::mark_for(&paths, "r-stubborn", pid, None);
        let started = std::time::Instant::now();
        let stopped = stop_live(
            &paths,
            "r-stubborn",
            &[
                (libc::SIGINT, std::time::Duration::from_secs(1)),
                (libc::SIGTERM, std::time::Duration::from_secs(5)),
            ],
        )
        .unwrap();
        assert_eq!(stopped, Some(pid));
        gone.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the stand-in ended");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(1),
            "it waited first"
        );
        // Its mark, which stopping skips removing, is gone too.
        assert!(!run_dir(&paths, "r-stubborn").join("pid").exists());
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn under_answers_person_resume_refuses_answers_and_decisions_from_anywhere_but_a_person() {
        use super::change::tests::repo_with;
        let paths = paths();
        let repo = repo_with(&[("a.txt", "one\n")]);
        let user = config::parse_user(include_str!("../../examples/acr/user.yaml")).unwrap();
        let cfg = config::resolve(
            &user,
            config::parse_repo(
                "version: 1\nanswers: person\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                 lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n",
            )
            .unwrap(),
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(cfg.answers, config::Answers::Person);
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": "r-person", "repo": repo, "base": "main", "head": null,
            "tier": "a", "tier_why": "", "fan_out": 1, "round": 1, "stage": "scope",
            "status": "waiting", "waiting": "scope", "lenses": {},
            "escalations": [], "disagreements": [], "usage": []
        }))
        .unwrap();
        save(&paths, &run, &cfg).unwrap();
        let resume = |answers: Option<String>, approve: bool| AcrCmd::Resume {
            run_id: "r-person".into(),
            answers,
            fixed: false,
            brief: None,
            fold: Vec::new(),
            waive: Vec::new(),
            defer: Vec::new(),
            approve,
            stop: false,
            post_pr: None,
        };
        let e = super::run(&paths, resume(Some("{}".into()), false), None).unwrap_err();
        assert!(format!("{e:#}").contains("--answers is refused"), "{e:#}");
        let e = super::run(&paths, resume(None, true), None).unwrap_err();
        assert!(format!("{e:#}").contains("--approve are refused"), "{e:#}");
        // Nothing was taken: it still waits at its scope gate.
        let kept: Run = serde_json::from_slice(
            &fs::read(run_dir(&paths, "r-person").join("run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(kept.waiting, Some(flow::Gate::Scope));
        assert!(kept.answered_by.is_empty());
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn a_running_review_stops_in_one_step() {
        use super::change::tests::repo_with;
        let paths = paths();
        let repo = repo_with(&[("a.txt", "one\n")]);
        let user = config::parse_user(include_str!("../../examples/acr/user.yaml")).unwrap();
        let cfg = config::resolve(
            &user,
            config::parse_repo(
                "version: 1\ntiers: [{id: a, match: {paths: ['**']}, fan_out: 1}]\n\
                 lenses: [{id: l, criteria: c, triggers: {paths: ['**']}}]\n",
            )
            .unwrap(),
            &Flags::default(),
        )
        .unwrap();
        // Mid-round in another process: running, and waiting for nobody.
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": "r-live", "repo": repo, "base": "main", "head": null,
            "tier": "a", "tier_why": "", "fan_out": 1, "round": 1, "stage": "review",
            "status": "running", "waiting": null, "lenses": {},
            "escalations": [], "disagreements": [], "usage": []
        }))
        .unwrap();
        save(&paths, &run, &cfg).unwrap();
        let (pid, gone) = stand_in("exec sleep 30");
        keep::mark_for(&paths, "r-live", pid, None);
        // Resuming it to go on is refused, and says how to stop it.
        let on = AcrCmd::Resume {
            run_id: "r-live".into(),
            answers: None,
            fixed: false,
            brief: None,
            fold: Vec::new(),
            waive: Vec::new(),
            defer: Vec::new(),
            approve: true,
            stop: false,
            post_pr: None,
        };
        let err = format!("{:#}", super::run(&paths, on, None).unwrap_err());
        assert!(err.contains("acr stop r-live"), "{err}");
        // One command: its process stops, and the review ends, stopped.
        let code = super::run(
            &paths,
            AcrCmd::Stop {
                run_id: "r-live".into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        gone.recv_timeout(std::time::Duration::from_secs(5))
            .expect("its process ended");
        let kept = keep::kept(&paths, "r-live").expect("ended, and kept");
        let name = kept.file_name().unwrap().to_string_lossy().into_owned();
        let saved = crate::util::packed_file(
            &kept,
            &format!("{}/run.json", name.trim_end_matches(".tar.gz")),
        )
        .unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(saved["status"], "stopped");
        // Stopping what's already over says so.
        let err = format!(
            "{:#}",
            super::run(
                &paths,
                AcrCmd::Stop {
                    run_id: "r-live".into()
                },
                None
            )
            .unwrap_err()
        );
        assert!(err.contains("has finished"), "{err}");
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_stopped_review_says_what_it_left_and_touches_none_of_it() {
        use super::change::tests::repo_with;
        let paths = paths();
        let repo = repo_with(&[("a.txt", "one\n")]);
        let mut run: Run = serde_json::from_value(serde_json::json!({
            "id": "01M7900F4B7PYCJKHYY3AYBZNK", "repo": repo, "base": "main", "head": null,
            "tier": "a", "tier_why": "", "fan_out": 1, "round": 1, "stage": "done",
            "status": "stopped", "waiting": null, "lenses": {},
            "escalations": [], "disagreements": [], "usage": []
        }))
        .unwrap();
        let clean = left_behind(&paths, &run);
        assert!(clean.contains("nothing uncommitted"), "{clean}");
        assert!(clean.contains("no copies"), "{clean}");
        // An Orchestrator's fix, uncommitted, and a copy it worked in.
        fs::write(repo.join("a.txt"), "fixed\n").unwrap();
        fs::write(repo.join("new.txt"), "added\n").unwrap();
        run.steelmans.push(
            serde_json::from_value(serde_json::json!({
                "round": 1, "blockers": [], "done": "fixed it", "declined": [], "pins": [],
                "tests_done": "", "briefs": {}
            }))
            .unwrap(),
        );
        let copy = copies_root(&paths).join("20261112T230300Z-01M7900F4B7PYCJKHYY3AYBZNK-r1-x");
        fs::create_dir_all(&copy).unwrap();
        let said = left_behind(&paths, &run);
        assert!(said.contains("2 uncommitted change(s)"), "{said}");
        assert!(
            said.contains("the Orchestrator's fixes are among them"),
            "{said}"
        );
        assert!(
            said.contains(" M a.txt") && said.contains("?? new.txt"),
            "{said}"
        );
        assert!(
            said.contains(&format!("git -C {} diff", repo.display())),
            "{said}"
        );
        assert!(said.contains("1 copy or copies"), "{said}");
        // Untouched.
        assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "fixed\n");
        assert!(repo.join("new.txt").exists() && copy.exists());
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn a_blocker_deferred_to_a_ticket_gets_a_draft_in_the_reviews_folder() {
        let paths = paths();
        let run: Run = serde_json::from_value(serde_json::json!({
            "id": "r-ticket", "repo": "/nowhere", "base": "main", "head": null,
            "tier": "a", "tier_why": "", "fan_out": 1, "round": 1, "stage": "done",
            "status": "confirmed", "waiting": null,
            "lenses": {"alpha": {"id": "alpha", "criteria": "c", "done": [], "outcome": "waived", "rounds": [{"round": 1,
                "reviewers": [], "failures": [], "findings": [{"id": "alpha-r1-1", "title": "a defect",
                "detail": "why", "file": "a.src", "line": 3, "severity": "High",
                "action": "blocks", "reason": "It's real.", "state": "deferred",
                "note": "next sprint"}]}]}},
            "escalations": [], "disagreements": [], "usage": []
        }))
        .unwrap();
        let written = write_tickets(&paths, &run);
        assert_eq!(written.len(), 1);
        let text = fs::read_to_string(&written[0]).unwrap();
        assert!(text.starts_with("# a defect\n\n"), "{text}");
        assert!(
            text.contains("`a.src:3`") && text.contains("next sprint"),
            "{text}"
        );
        assert!(text.contains("It's real."), "{text}");
        assert_eq!(
            written[0],
            run_dir(&paths, "r-ticket")
                .join("tickets")
                .join("alpha-r1-1.md")
        );
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn an_unconfigured_repository_is_told_how_to_get_a_config() {
        use super::change::tests::repo_with;
        let repo = repo_with(&[("a.txt", "one\n")]);
        let paths = paths();
        let err = format!(
            "{:#}",
            repo_layer(&paths, &repo, Some("main"), None).unwrap_err()
        );
        assert!(err.contains("has no review config yet"), "{err}");
        assert!(err.contains("acr configure --repo"), "{err}");
        // One that only a change brings along doesn't count.
        super::change::tests::branch(&repo, "work");
        super::change::tests::commit(&repo, &[(REPO_CONFIG, "version: 1\n")], "bring my own");
        let err = format!(
            "{:#}",
            repo_layer(&paths, &repo, Some("main"), None).unwrap_err()
        );
        assert!(err.contains("has no review config yet"), "{err}");
        assert!(err.contains(&outside_config(&paths, &repo).unwrap().display().to_string()));
        let _ = fs::remove_dir_all(&repo);
    }

    fn target(repo: &Path, config: Option<PathBuf>) -> Target {
        Target {
            repo: repo.to_path_buf(),
            base: "main".into(),
            head: Some("work".into()),
            config,
            human_gate: None,
            max_rounds: None,
            reattack: None,
            spend_cap_usd: None,
            review_scope: None,
            bindings: Vec::new(),
            description: None,
            pr: None,
            brief: None,
        }
    }

    #[test]
    fn a_description_comes_from_its_file_and_nothing_is_none() {
        let dir = std::env::temp_dir().join(format!("tb-desc-{}", new_id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pr.md");
        fs::write(&file, "# Fix B-1\n\nAn edit keeps the date.").unwrap();
        let mut t = target(&dir, None);
        assert_eq!(description_of(&t, None).unwrap(), None);
        t.description = Some(file);
        assert_eq!(
            description_of(&t, Some(7)).unwrap().as_deref(),
            Some("# Fix B-1\n\nAn edit keeps the date.")
        );
        t.description = Some(dir.join("missing.md"));
        assert!(description_of(&t, None).is_err());
    }

    #[test]
    fn a_config_kept_outside_the_repository_wins_and_brings_its_own_list() {
        let repo = repo_with(&[
            (REPO_CONFIG, &lens("inside")),
            ("decided.md", "inside list"),
        ]);
        branch(&repo, "work");
        commit(&repo, &[("a.txt", "x")], "a change");
        let paths = paths();
        // With nothing outside, it's the repository's own.
        let (cfg, source) = load_config(&paths, &target(&repo, None), &Flags::default()).unwrap();
        assert_eq!(cfg.lenses[0].id, "inside");
        assert!(!cfg.outside);
        assert_eq!(
            not_findings(&cfg, &source, &repo, "main").unwrap(),
            "inside list"
        );

        // Kept in acr/, under the repository's name.
        let kept = outside_config(&paths, &repo).unwrap();
        fs::create_dir_all(kept.parent().unwrap()).unwrap();
        fs::write(&kept, lens("outside")).unwrap();
        fs::write(kept.with_file_name("decided.md"), "outside list").unwrap();
        let (cfg, source) = load_config(&paths, &target(&repo, None), &Flags::default()).unwrap();
        assert_eq!(cfg.lenses[0].id, "outside");
        assert!(cfg.outside);
        assert_eq!(
            not_findings(&cfg, &source, &repo, "main").unwrap(),
            "outside list"
        );

        // Named on the command line, which beats both.
        let named = paths.state.with_file_name("named.yaml");
        fs::write(&named, lens("named")).unwrap();
        fs::write(named.with_file_name("decided.md"), "named list").unwrap();
        let t = target(&repo, Some(named));
        let (cfg, source) = load_config(&paths, &t, &Flags::default()).unwrap();
        assert_eq!(cfg.lenses[0].id, "named");
        assert_eq!(
            not_findings(&cfg, &source, &repo, "main").unwrap(),
            "named list"
        );
    }

    #[test]
    fn with_no_config_anywhere_it_says_where_it_looked() {
        let repo = repo_with(&[("a.txt", "x")]);
        branch(&repo, "work");
        let err = load_config(&paths(), &target(&repo, None), &Flags::default()).unwrap_err();
        assert!(format!("{err:#}").contains(REPO_CONFIG), "{err:#}");
    }

    #[test]
    fn a_caller_removes_only_the_attachments_it_made() {
        let paths = paths();
        let theirs = {
            let caller = BridgeCaller::new(&paths).unwrap();
            // Someone else's, already in the store.
            let theirs = caller
                .store
                .add("theirs.txt", &mut &b"shared"[..], 6)
                .unwrap();
            let again = caller.attach("same.txt", b"shared").unwrap();
            assert_eq!(again.sha256, theirs.sha256);
            let mine = caller.attach("mine.txt", b"only mine").unwrap();
            assert!(caller.store.path(&mine.sha256).exists());
            let mine_path = caller.store.path(&mine.sha256);
            drop(caller);
            assert!(!mine_path.exists(), "it leaves nothing behind");
            theirs
        };
        let store = Attachments::new(paths.attachments(), 1 << 20, 1 << 30);
        assert!(
            store.path(&theirs.sha256).exists(),
            "and never takes another's"
        );
        fs::remove_dir_all(paths.state.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_doctor_says_how_to_give_a_bridges_agent_the_system_prompt() {
        let dir = std::env::temp_dir().join(format!("tb-acr-prompt-{}", new_id()));
        let paths = Paths {
            state: dir.join("state"),
            config: dir.join("config.toml"),
        };
        let bridge = |command: &str, args: &str| -> crate::bridge::Settings {
            toml::from_str(&format!(
                "owner = \"o\"\ncommand = \"{command}\"\nworking_directory = \"/tmp\"\n\
                 home = \"/tmp/h\"\npath = \"/usr/bin\"\nargs = {args}\n"
            ))
            .unwrap()
        };
        // One acrbus knows gets it from the bridge, whatever its args say.
        let omp = bridge("/usr/local/bin/omp", "[\"-p\"]");
        assert!(matches!(doctor_prompt(&paths, "omp", &omp), Said::Ok(_)));
        // Another agent needs the placeholder.
        let file = crate::setup::system_prompt_path(&paths);
        let arg = crate::setup::system_prompt_arg(&file);
        let given = bridge("/opt/agent", &format!("[\"--sys\", \"{arg}\"]"));
        assert!(matches!(doctor_prompt(&paths, "x", &given), Said::Ok(_)));
        let Said::Fix(fix) = doctor_prompt(&paths, "x", &bridge("/opt/agent", "[]")) else {
            panic!("a bridge without it needs a fix");
        };
        assert!(fix.contains(&arg) && fix.contains("set `kind`"), "{fix}");
    }

    #[test]
    fn a_check_says_an_unconfined_agent_reads_outside_and_fails_a_confined_one() {
        let read = vec![
            trial::READ_OUTSIDE.to_string(),
            "its Bash didn't run".to_string(),
        ];
        let (rest, outside) = outside_is_a_note(read.clone(), false);
        assert_eq!(rest, ["its Bash didn't run"]);
        assert!(outside);
        let (rest, outside) = outside_is_a_note(read.clone(), true);
        assert_eq!(rest, read, "a confined agent that read outside is wrong");
        assert!(!outside);
    }

    #[test]
    fn a_check_needs_no_trial_builder_where_claude_code_isnt_set_up() {
        let dir = std::env::temp_dir().join(format!("tb-acr-builder-{}", new_id()));
        let paths = Paths {
            state: dir.join("state"),
            config: dir.join("config.toml"),
        };
        let why = trial::builder_missing(&paths).unwrap();
        assert!(why.contains("setup acr"), "{why}");
        fs::create_dir_all(dir.join("acr")).unwrap();
        fs::write(dir.join("acr").join(crate::setup::CONFINE_FILE), "{}").unwrap();
        assert_eq!(trial::builder_missing(&paths), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_a_person_types_at_the_terminal_is_their_answer() {
        let q = |multi: bool| carousel::Question {
            question: "q".into(),
            header: "h".into(),
            options: ["Fix it", "Not a defect", "None of them"]
                .iter()
                .map(|l| carousel::Choice {
                    label: l.to_string(),
                    description: String::new(),
                })
                .collect(),
            multi_select: multi,
        };
        assert_eq!(
            typed(&q(false), "2"),
            Some(serde_json::json!("Not a defect"))
        );
        assert_eq!(
            typed(&q(false), "use a lock"),
            Some(serde_json::json!("use a lock"))
        );
        assert_eq!(
            typed(&q(false), "9"),
            Some(serde_json::json!("9")),
            "out of range is words"
        );
        assert_eq!(
            typed(&q(true), "1, 3"),
            Some(serde_json::json!(["Fix it", "None of them"]))
        );
        assert_eq!(typed(&q(true), "  "), None);
    }

    #[test]
    fn a_review_keeps_its_calls_in_flight_so_stopping_it_stops_them() {
        let paths = paths();
        let caller = BridgeCaller::new(&paths).unwrap();
        let dir = run_dir(&paths, "r-calls");
        fs::create_dir_all(&dir).unwrap();
        *caller.calls_file.lock().unwrap() = Some(dir.join("calls"));
        caller.in_flight("one", true);
        caller.in_flight("two", true);
        caller.in_flight("one", false);
        assert_eq!(fs::read_to_string(dir.join("calls")).unwrap(), "two");
        drop(caller);
        // A resume clears what a review that died left at work, even with
        // no bus to tell, and leaves nothing to clear twice.
        cancel_left(&paths, "r-calls");
        assert!(!dir.join("calls").exists());
        cancel_left(&paths, "r-calls");
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn a_review_sends_as_itself_never_a_session() {
        let paths = paths();
        assert_eq!(BridgeCaller::new(&paths).unwrap().agent, "acr");
    }

    #[test]
    fn a_stopped_runs_finished_calls_are_counted_from_their_answers() {
        // A run stopped mid-round recorded only what finished in earlier
        // rounds: the round's own finished calls were still in hand. Each
        // answer is kept as it comes, so they count when it's stopped.
        let paths = paths();
        let caller = BridgeCaller::new(&paths).unwrap();
        let dir = run_dir(&paths, "r-answered");
        fs::create_dir_all(&dir).unwrap();
        *caller.calls_file.lock().unwrap() = Some(dir.join("calls"));
        let binding = config::Binding {
            agent: "pi@here".into(),
            model: Some("glm".into()),
            ..config::Binding::default()
        };
        let call = |label: &str| Call {
            role: Role::Reviewer,
            binding_name: "small",
            binding: &binding,
            prompt: "Review",
            artifact: None,
            writes: false,
            no_tools: false,
            relay_tokens: None,
            repo: Path::new("."),
            label: label.into(),
        };
        let answer = |id: &str, usd: f64| flow::Answer {
            text: "{}".into(),
            run_id: id.into(),
            usage: Some(crate::proto::RunUsage {
                calls: 7,
                input_tokens: 1_000,
                cache_read_tokens: 40_000,
                output_tokens: 200,
                usd: Some(usd),
                ..crate::proto::RunUsage::default()
            }),
            secs: Some(30),
        };
        caller.answered(&call("api: reviewer"), &answer("req-1", 0.10));
        caller.answered(&call("probes: reviewer"), &answer("req-2", 0.25));
        drop(caller);
        // The run had recorded req-1 at an earlier round's end, and was in
        // round 2 when it stopped.
        let mut run: Run = serde_json::from_value(serde_json::json!({
            "id": "r-answered", "repo": "/r", "base": "main", "head": null,
            "tier": "core", "tier_why": "", "fan_out": 1, "round": 2, "stage": "review",
            "status": "running", "waiting": null,
            "lenses": {}, "escalations": [], "disagreements": [], "usage": []
        }))
        .unwrap();
        let mut kept = flow::Answered::read(&dir)[0].usage(1);
        kept.unfinished = false;
        run.usage.push(kept);
        let whole = with_unrecorded(&paths, &run);
        assert_eq!(whole.usage.len(), 2, "req-2 added once, req-1 not twice");
        let added = whole.usage.iter().find(|u| u.run_id == "req-2").unwrap();
        assert!(added.unfinished);
        assert_eq!(added.round, 2);
        assert_eq!(added.label.as_deref(), Some("probes: reviewer"));
        assert_eq!(added.usd, Some(0.25));
        assert_eq!(
            (added.tokens_in, added.cache_read, added.tokens_out),
            (1_000, 40_000, 200)
        );
        assert!(
            report::unfinished_line(&whole.usage).is_some_and(|l| l
                .contains("round 2 didn't finish")
                && l.contains("its 1 finished call")),
            "{:?}",
            report::unfinished_line(&whole.usage)
        );
        assert!(report::unfinished_line(&run.usage).is_none());
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
    }

    #[test]
    fn a_fix_needs_somewhere_to_go() {
        let paths = paths();
        let caller = BridgeCaller::new(&paths).unwrap();
        let binding = config::Binding {
            agent: "a@b".into(),
            ..config::Binding::default()
        };
        let call = Call {
            role: Role::Orchestrator,
            binding_name: "large",
            binding: &binding,
            prompt: "Fix these",
            artifact: None,
            writes: true,
            no_tools: false,
            relay_tokens: None,
            repo: Path::new("."),
            label: "the Orchestrator, fixing".into(),
        };
        let err = caller.call(&call).err().unwrap();
        assert!(format!("{err:#}").contains("writes: true"), "{err:#}");
    }

    #[test]
    fn a_call_asks_its_bridge_for_as_long_as_it_allows_not_its_default() {
        // A review's call is never cut off while it works: with no cap of
        // its binding's, it asks for the bridge's longest, since a bridge's
        // default may be ten minutes.
        let paths = paths();
        let work = paths.config.parent().unwrap().join("work");
        fs::create_dir_all(&work).unwrap();
        let user = bridge_and_bindings(&paths, &work);
        let text = fs::read_to_string(&paths.config)
            .unwrap()
            .replace("max_timeout_secs = 600", "max_timeout_secs = 7200");
        fs::write(&paths.config, text).unwrap();
        let caller = BridgeCaller::new(&paths).unwrap();
        let uncapped = config::Binding {
            timeout: None,
            ..user.allowed_bindings["edits"].clone()
        };
        assert_eq!(caller.asks_for(&uncapped, "agent-x"), Some(7200));
        assert_eq!(
            caller.asks_for(&user.allowed_bindings["fits"], "agent-x"),
            Some(600)
        );
        assert_eq!(caller.asks_for(&uncapped, "no-such-bridge"), None);
        drop(caller);
        fs::remove_dir_all(paths.config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_runs_list_is_the_one_it_started_with() {
        let paths = paths();
        let cfg = config::resolve(
            &Default::default(),
            config::parse_repo(&lens("l")).unwrap(),
            &Flags::default(),
        )
        .unwrap();
        let s = setting(&paths, &cfg, "r1", Some("the list".into())).unwrap();
        assert_eq!(s.not_findings, "the list");
        let again = setting(&paths, &cfg, "r1", None).unwrap();
        assert_eq!(again.not_findings, "the list");
    }

    #[test]
    fn clean_clears_only_copies_left_over_and_doctor_counts_them() {
        let p = paths();
        let root = copies_root(&p);
        let stamp = |ago: u64| crate::util::stamp(crate::util::now_ms() - (ago as i64) * 1000);
        let copy = |name: String| {
            let dir = root.join(&name).join("repo");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("a.src"), "x".repeat(2000)).unwrap();
            name
        };
        let (done, waiting, running, gone) = (new_id(), new_id(), new_id(), new_id());
        // A finished review, packed; one waiting for a person; one running.
        fs::create_dir_all(keep::dir(&p)).unwrap();
        fs::write(
            keep::dir(&p).join(format!("{}-{done}.tar.gz", stamp(60))),
            "x",
        )
        .unwrap();
        fs::create_dir_all(run_dir(&p, &waiting)).unwrap();
        fs::write(run_dir(&p, &waiting).join("run.json"), "{}").unwrap();
        let _running = keep::active_guard(&p, &running).unwrap();
        let names = [
            copy(format!("{}-{done}-r1-aaaaaa", stamp(60))),
            copy(format!("{}-{waiting}-r1-bbbbbb", stamp(60))),
            copy(format!("{}-{running}-r1-cccccc", stamp(60))),
            copy(format!("{}-{gone}-r2-dddddd", stamp(60))),
            copy(format!("{}-check-{}", stamp(3600), new_id())),
            copy(format!("{}-check-{}", stamp(5), new_id())),
        ];
        let leftover: std::collections::BTreeSet<String> = kept_copies(&p)
            .into_iter()
            .filter(|k| k.leftover.is_some())
            .map(|k| k.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let expected: std::collections::BTreeSet<String> = [&names[0], &names[3], &names[4]]
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(leftover, expected);
        let note = copies_note(&p).unwrap();
        assert!(
            note.starts_with("6 copies kept in")
                && note.contains("12 KB apparent")
                && note.contains("3 of them are left over"),
            "{note}"
        );
        // A dry run removes nothing; then only the leftovers go.
        clean(&p, true).unwrap();
        assert!(names.iter().all(|n| root.join(n).exists()));
        clean(&p, false).unwrap();
        for (n, name) in names.iter().enumerate() {
            assert_eq!(root.join(name).exists(), ![0, 3, 4].contains(&n), "{name}");
        }
        drop(_running);
        let _ = fs::remove_dir_all(&p.state);
    }

    #[test]
    fn ask_shows_a_running_reviews_lenses_where_they_stand() {
        let p = paths();
        let id = new_id();
        let _running = keep::active_guard(&p, &id).unwrap();
        let said = lens_states(&p, &id, true).unwrap();
        assert!(said.contains("between rounds"), "{said}");
        let mut states = flow::LensStates {
            round: 2,
            ..flow::LensStates::default()
        };
        states.lenses.insert(
            "probes".into(),
            flow::LensNow {
                state: "no findings".into(),
                reviewers: 1,
                answered: 1,
                at: crate::util::now_ms(),
            },
        );
        fs::write(
            run_dir(&p, &id).join(flow::LENS_STATES),
            serde_json::to_vec(&states).unwrap(),
        )
        .unwrap();
        let said = lens_states(&p, &id, true).unwrap();
        assert!(
            said.contains("round 2")
                && said.contains("probes: no findings, 1 of 1 reviewer(s) answered"),
            "{said}"
        );
        let json: serde_json::Value =
            serde_json::from_str(&lens_states(&p, &id, false).unwrap()).unwrap();
        assert_eq!(json["lenses"]["probes"]["state"], "no findings");
        drop(_running);
        let _ = fs::remove_dir_all(&p.state);
    }

    #[test]
    fn a_call_asks_for_its_bindings_tools_unchanged_unless_it_asks_for_none() {
        let binding = config::Binding {
            agent: "pi@here".into(),
            model: Some("m".into()),
            tools: Some(vec![
                "read".into(),
                "grep".into(),
                "find".into(),
                "ls".into(),
            ]),
            ..config::Binding::default()
        };
        let repo = PathBuf::from("/repo");
        let call = |role, no_tools| flow::Call {
            role,
            binding_name: "b",
            binding: &binding,
            prompt: "p",
            artifact: None,
            writes: false,
            no_tools,
            relay_tokens: None,
            repo: &repo,
            label: "x".into(),
        };
        // A call that relays asks its bridge to hand off past its size.
        let relaying = flow::Call {
            relay_tokens: Some(90_000),
            ..call(Role::Reviewer, false)
        };
        assert_eq!(
            call_options(&relaying, false, Some(60)).relay_tokens,
            Some(90_000)
        );
        assert_eq!(
            call_options(&call(Role::Reviewer, false), false, Some(60)).relay_tokens,
            None
        );
        // A fixer's relay counts its growth and waits for its first edit;
        // a reader's is its size alone.
        assert_eq!(call_options(&relaying, false, None).relay_after_edit, None);
        let fixing = flow::Call {
            role: Role::Orchestrator,
            writes: true,
            relay_tokens: Some(64_000),
            ..call(Role::Orchestrator, false)
        };
        assert_eq!(
            call_options(&fixing, false, None).relay_after_edit,
            Some(true)
        );
        let whole = flow::Call {
            relay_tokens: None,
            ..fixing
        };
        assert_eq!(call_options(&whole, false, None).relay_after_edit, None);
        for role in [Role::Judge, Role::Reviewer, Role::Orchestrator, Role::Sizer] {
            let options = call_options(&call(role, false), false, Some(60));
            assert_eq!(options.tools, binding.tools, "{role:?}");
            assert_eq!(options.workdir.as_deref(), Some("/repo"));
            assert_eq!(options.model.as_deref(), Some("m"));
        }
        let bare = call_options(&call(Role::Sizer, true), false, Some(60));
        assert_eq!(bare.tools, Some(Vec::new()));
        // A binding with no list leaves the bridge's own tools.
        let open = config::Binding {
            tools: None,
            ..binding.clone()
        };
        let any = flow::Call {
            binding: &open,
            ..call(Role::Judge, false)
        };
        assert_eq!(call_options(&any, false, None).tools, None);
    }

    #[test]
    fn a_reviewers_and_judges_calls_get_the_rounds_facts_key_and_matched_tools() {
        let reviewer = config::Binding {
            agent: "pi@here".into(),
            model: Some("m".into()),
            tools: Some(vec!["grep".into(), "read".into()]),
            ..config::Binding::default()
        };
        let judge = config::Binding {
            tools: Some(vec!["bash".into(), "grep".into(), "read".into()]),
            ..reviewer.clone()
        };
        let shared = flow::Shared {
            facts: PathBuf::from("/state/acr/r/round-1.facts.md"),
            key: "01RUN".into(),
            tools: std::collections::BTreeMap::from([(
                ("pi@here".to_string(), Some("m".to_string())),
                vec!["bash".to_string(), "grep".to_string(), "read".to_string()],
            )]),
            agents: vec!["pi@here".into()],
        };
        let repo = PathBuf::from("/repo");
        let call = |role, binding, writes, no_tools| flow::Call {
            role,
            binding_name: "b",
            binding,
            prompt: "p",
            artifact: None,
            writes,
            no_tools,
            relay_tokens: None,
            repo: &repo,
            label: "x".into(),
        };
        let given = |c: &flow::Call| {
            let mut o = call_options(c, false, None);
            with_shared(&mut o, c, &shared, true);
            o
        };
        let r = given(&call(Role::Reviewer, &reviewer, false, false));
        assert_eq!(r.facts.as_deref(), Some("/state/acr/r/round-1.facts.md"));
        assert_eq!(
            r.tools,
            Some(vec!["bash".into(), "grep".into(), "read".into()])
        );
        assert_eq!(r.allowed_tools, reviewer.tools, "held to its own");
        let j = given(&call(Role::Judge, &judge, false, false));
        assert_eq!(j.tools, judge.tools);
        assert_eq!(
            j.allowed_tools, None,
            "its own are every tool: nothing to hold"
        );
        assert_eq!(r.cache_key, j.cache_key, "one key for one bridge and model");
        let key = r.cache_key.unwrap();
        assert!(key.starts_with("acr-01RUN-") && key.len() <= 128, "{key}");
        let other = cache_key("01RUN", "pi@here", Some("other"));
        assert_ne!(key, other, "another model, another key");
        // No tools stays none; the Orchestrator, a Sizer and a fix get
        // nothing of it.
        let bare = given(&call(Role::Judge, &judge, false, true));
        assert_eq!((bare.tools, bare.allowed_tools), (Some(Vec::new()), None));
        assert!(bare.facts.is_some());
        for (role, writes) in [
            (Role::Orchestrator, false),
            (Role::Sizer, false),
            (Role::Reviewer, true),
        ] {
            let o = given(&call(role, &reviewer, writes, false));
            assert_eq!(
                (o.facts, o.cache_key, o.allowed_tools),
                (None, None, None),
                "{role:?}"
            );
        }
        // A fix gets the key alone, so its groups' sessions share their
        // opening: not the facts, which a fix outdates, nor other tools.
        let fixing = given(&call(Role::Orchestrator, &reviewer, true, false));
        assert_eq!(fixing.cache_key.as_deref(), Some(key.as_str()));
        assert_eq!(
            (fixing.facts, fixing.allowed_tools, fixing.tools),
            (None, None, reviewer.tools.clone())
        );
        assert!(tools_note(&["read".into()]).contains("The tools you may use: read."));
        assert!(tools_note(&[]).contains("The tools you may use: none."));
        // A bridge that can't hold a call to some of its tools, such as
        // Claude Code's, would refuse one: it keeps its own, with the facts
        // and the key all the same.
        let c = call(Role::Reviewer, &reviewer, false, false);
        let mut kept = call_options(&c, false, None);
        with_shared(&mut kept, &c, &shared, false);
        assert_eq!(
            (kept.tools, kept.allowed_tools),
            (reviewer.tools.clone(), None)
        );
        assert!(kept.facts.is_some() && kept.cache_key.is_some());
    }

    #[test]
    fn every_judge_and_the_judge_keep_alive_ask_for_the_same_prefix_and_cache_key() {
        // Everything a call's bridge turns into what comes before its own
        // text: model, effort, tools, the facts and the cache key. Two
        // Judges and the keep-alive that refreshes their prefix must agree
        // on all of it, or the keep-alive warms a prefix no Judge reads.
        let judge = config::Binding {
            agent: "pi@here".into(),
            model: Some("glm".into()),
            effort: Some("low".into()),
            tools: Some(vec!["grep".into(), "read".into()]),
            ..config::Binding::default()
        };
        let shared = |round: u32| flow::Shared {
            facts: PathBuf::from(format!("/state/acr/r/round-{round}.facts.md")),
            key: "01RUN".into(),
            tools: std::collections::BTreeMap::new(),
            agents: vec!["pi@here".into()],
        };
        let repo = PathBuf::from("/repo");
        let diff = PathBuf::from("/state/acr/r/a.diff");
        // Two Judges, each with its own brief and the change attached, and
        // the keep-alive, with neither.
        let calls: [(&str, Option<&Path>, &str); 3] = [
            ("judge A's brief", Some(&diff), "a: the Judge"),
            ("judge B's brief", Some(&diff), "b: the Judge"),
            (roles::KEEP_ALIVE, None, "cache keep-alive: Judges"),
        ];
        let prefix = |round: u32, (prompt, artifact, label): &(&str, Option<&Path>, &str)| {
            let c = flow::Call {
                role: Role::Judge,
                binding_name: "judge",
                binding: &judge,
                prompt,
                artifact: *artifact,
                writes: false,
                no_tools: false,
                relay_tokens: None,
                repo: &repo,
                label: label.to_string(),
            };
            let mut o = call_options(&c, false, None);
            with_shared(&mut o, &c, &shared(round), true);
            (
                o.model,
                o.effort,
                o.tools,
                o.allowed_tools,
                o.facts,
                o.cache_key,
                o.review,
            )
        };
        let first = prefix(1, &calls[0]);
        for c in &calls[1..] {
            assert_eq!(prefix(1, c), first, "{}", c.2);
        }
        assert_eq!(first.1.as_deref(), Some("low"), "the binding's effort");
        assert!(first.5.is_some(), "a cache key");
        // A later round has its own facts, but keeps the key, so it reaches
        // the replica the last round warmed.
        let later = prefix(2, &calls[2]);
        assert_ne!(later.4, first.4);
        assert_eq!(later.5, first.5);
    }

    #[test]
    fn matched_tools_are_the_union_for_a_bridge_and_model_only_where_lists_differ() {
        let user = config::parse_user(include_str!("../../examples/acr/user.yaml")).unwrap();
        let repo = config::parse_repo(
            "version: 1\ntiers: [{id: t, match: {description: x}, fan_out: 1}]\nlenses: [{id: a, criteria: c, triggers: {paths: [\"**\"]}}]\n",
        )
        .unwrap();
        let mut cfg = config::resolve(&user, repo, &config::Flags::default()).unwrap();
        let lens = cfg.lenses[0].clone();
        let r = cfg
            .binding_name(Role::Reviewer, Some(&lens))
            .unwrap()
            .to_string();
        let j = cfg.binding_name(Role::Judge, None).unwrap().to_string();
        assert_ne!(r, j, "the example binds them apart");
        let set = |cfg: &mut Config, name: &str, model: &str, tools: &[&str]| {
            let b = cfg.allowed_bindings.get_mut(name).unwrap();
            b.agent = "pi@here".into();
            b.model = Some(model.into());
            b.tools = Some(tools.iter().map(|t| t.to_string()).collect());
        };
        set(&mut cfg, &r, "big", &["read", "grep"]);
        set(&mut cfg, &j, "big", &["read", "bash"]);
        let matched = flow::matched_tools(&cfg);
        assert_eq!(
            matched.get(&("pi@here".to_string(), Some("big".to_string()))),
            Some(&vec![
                "bash".to_string(),
                "grep".to_string(),
                "read".to_string()
            ])
        );
        // Another model can't share their prefix, so nothing is matched.
        set(&mut cfg, &j, "small", &["read", "bash"]);
        assert!(flow::matched_tools(&cfg).is_empty());
        set(&mut cfg, &j, "big", &["read", "bash"]);
        cfg.facts.match_tools = false;
        assert!(flow::matched_tools(&cfg).is_empty());
    }
}
