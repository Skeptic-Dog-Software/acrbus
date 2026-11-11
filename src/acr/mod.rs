//! `acrbus acr`: adversarial convergence review for any repository. The
//! framework holds no knowledge of a repository. Roles, tiers, lenses and
//! bindings all come from config, in `config`.

mod calibration;
mod carry;
mod change;
mod config;
mod configure;
mod flow;
mod generated;
mod keep;
mod plan;
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
    /// The change's head. Left out, the working tree.
    #[arg(long)]
    head: Option<String>,
    #[arg(long, value_parser = parse_gate)]
    human_gate: Option<HumanGate>,
    #[arg(long)]
    max_rounds: Option<u32>,
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
        /// changed since: its rulings haven't been checked against a full
        /// review.
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
    /// Carry on with a review that's waiting for a person.
    Resume {
        run_id: String,
        /// Blockers to fix, by ID, with commas.
        #[arg(long, value_delimiter = ',')]
        fold: Vec<String>,
        /// Blockers to waive, by ID, with commas.
        #[arg(long, value_delimiter = ',')]
        waive: Vec<String>,
        /// Go on past an escalation, or approve the scope or the result.
        #[arg(long)]
        approve: bool,
        /// End the review here.
        #[arg(long)]
        stop: bool,
        #[arg(long, value_name = "NUMBER")]
        post_pr: Option<u64>,
    },
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

fn flags(t: &Target) -> Flags {
    Flags {
        human_gate: t.human_gate,
        max_rounds: t.max_rounds,
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
    // Calibration.
    let decided = not_findings(&cfg, &source, repo, "HEAD").unwrap_or_default();
    match (change::name(repo), fingerprint(&cfg, &text, &decided)) {
        (Ok(name), Ok(now)) => {
            let record = calibration::load(&paths.state, &name).unwrap_or_default();
            match calibration::standing(&record, &now) {
                calibration::Standing::Calibrated => out.push(Said::Ok(format!("{name} is calibrated"))),
                calibration::Standing::JudgeChanged => out.push(Said::Note(format!(
                    "{name} is calibrated, but on an earlier Judge: `{APP_NAME} acr calibration \
                     --repo {shown} --accept` when you've read its config again"
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
    let root = copies_root(paths).join(format!("doctor-{}", crate::util::new_id()));
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
    let _ = fs::remove_dir_all(&root);
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
        })
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

    fn call(&self, call: &Call) -> Result<Answer> {
        let b = call.binding;
        // Fixes only from a binding the user lets write.
        if call.writes && !b.writes {
            bail!(
                "the {} binding can't apply fixes: give it `writes: true` in your acr.yaml",
                call.binding_name
            );
        }
        let options = RunOptions {
            // Every role works in the repository, so reviewers read the
            // code itself, read-only, and an Orchestrator fixes it there.
            workdir: Some(call.repo.display().to_string()),
            // A review keeps its own record, and leaves nothing behind,
            // unless it's a trial's, kept to see what each agent did.
            log: Some(self.keep_logs),
            ..options_for(b)
        };
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
        let mut client = Client::connect(&self.socket)?;
        client.send(&Request::Call {
            agent: self.agent.clone(),
            to: b.agent.clone(),
            body,
            reply_to: None,
            expects_reply: Some(true),
            ext: Some(Box::new(ext)),
            wait_secs: Some(wait),
        })?;
        let sent: Sent = client.reply()?;
        let awaited = match client.reply::<Awaited>() {
            Ok(awaited) => awaited,
            // The daemon went away mid-wait, as it does when it's updated.
            // The reply is kept for this agent, so wait for it again.
            Err(_) => {
                let left = wait.saturating_sub(asked.elapsed().as_secs()).max(1);
                self.await_again(&sent.id, left)?
            }
        };
        match awaited {
            Awaited::Reply { item } => {
                let msg = &item.msg;
                let result = msg.result();
                let status = result.map_or(RunStatus::Ok, |r| r.status);
                if status == RunStatus::Timeout {
                    return Err(flow::OverTime(format!("{} timeout: {}", b.agent, msg.body)).into());
                }
                if status != RunStatus::Ok {
                    bail!("{} {}: {}", b.agent, status.as_str(), msg.body);
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
                Ok(Answer {
                    text,
                    run_id: sent.id,
                    usage: result.and_then(|r| r.usage.clone()),
                    via: None,
                })
            }
            Awaited::Undelivered { why } => bail!("{}: {why}", b.agent),
            Awaited::Unanswered => bail!("{} marked it handled without answering", b.agent),
            Awaited::TimedOut => {
                Err(flow::OverTime(format!("{} didn't answer in {wait} seconds", b.agent)).into())
            }
        }
    }
}

fn save(paths: &Paths, run: &Run, cfg: &Config) -> Result<()> {
    let dir = run_dir(paths, &run.id);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("run.json"), serde_json::to_vec_pretty(run)?)?;
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

/// Prints a run's report, and says what to do next.
fn finish(paths: &Paths, run: &Run, cfg: &Config, pr: Option<u64>) -> Result<ExitCode> {
    save(paths, run, cfg)?;
    keep::finished(paths, run, true)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report::report(run, cfg))?
    );
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
        (Status::Waiting, Some(flow::Gate::Scope)) => format!(
            "waiting for you to approve its scope, tier {} ({}), before any reviewer runs: \
             `{APP_NAME} acr resume {id} --approve`, or `--stop`.",
            run.tier, run.tier_why
        ),
        (Status::Waiting, Some(flow::Gate::Fold)) => format!(
            "waiting for you to choose which blockers get fixed: {}. `{APP_NAME} acr resume \
             {id} --fold <ids>`{}, `--waive <ids>`, or `--stop`. `{APP_NAME} acr feedback {id}` \
             lists them with their IDs.",
            blockers.join(", "),
            if writes {
                ""
            } else {
                " (the Orchestrator's binding can't write, so give it `writes: true` in acr.yaml for that)"
            }
        ),
        (Status::Waiting, Some(flow::Gate::Final)) => format!(
            "waiting for you to approve the result: `{APP_NAME} acr resume {id} --approve`, or `--stop`."
        ),
        (Status::Waiting, _) => {
            let unseen: Vec<&str> = run
                .escalations
                .iter()
                .filter(|e| !e.seen)
                .map(|e| e.reason.as_str())
                .collect();
            format!(
                "waiting for you, escalated: {}. Read it, then `{APP_NAME} acr resume {id} \
                 --approve` to go on, or `--stop`.",
                unseen.join("; ")
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

/// Where reviewers that can change files get their copies of the checkout.
fn copies_root(paths: &Paths) -> PathBuf {
    paths.state.join("acr-copies")
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
        copies: Some(copies_root(paths)),
        builder_brief: None,
    })
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

pub fn run(paths: &Paths, command: AcrCmd) -> Result<ExitCode> {
    keep::prune(paths);
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
            if let Some(note) = calibrated(paths, &name, &now, uncalibrated)? {
                eprintln!("{note}");
            }
            check_machine(paths, &cfg, &target.repo, false)?;
            change::checked_out(&target.repo, target.head.as_deref())?;
            allow_copies(paths, &cfg)?;
            let description = description_of(&target, post_pr)?;
            let id = new_id();
            let mut s = setting(paths, &cfg, &id, Some(list))?;
            s.description = description;
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
            let mut run = flow::start(&id, repo, base, head, &s, &change, &caller)?;
            // After a fix, the change is the working tree.
            let reloading = |fixed: bool| reload(&cfg, repo, base, head.filter(|_| !fixed));
            save(paths, &run, &cfg)?;
            flow::drive(&mut run, &s, &caller, &reloading)?;
            return finish(paths, &run, &cfg, post_pr);
        }
        AcrCmd::Resume {
            run_id,
            fold,
            waive,
            approve,
            stop,
            post_pr,
        } => {
            let dir = run_dir(paths, &run_id);
            let read =
                |name: &str| fs::read(dir.join(name)).with_context(|| format!("no run {run_id}"));
            let mut run: Run = serde_json::from_slice(&read("run.json")?)?;
            let _active = keep::active_guard(paths, &run_id)?;
            let cfg: Config = stored_config(&read("config.json")?)?;
            // Ending a review calls nothing and reads no code, so neither its
            // agents nor its checkout has to be as it was.
            if stop {
                eprintln!(
                    "{APP_NAME}: ending review {run_id}, as asked. `{APP_NAME} acr feedback \
                     {run_id}` lists its findings."
                );
            } else {
                // Until a fix, the review is of its head, which must be
                // checked out.
                if run.lenses.values().all(|l| l.done.is_empty()) {
                    change::checked_out(&run.repo, run.head.as_deref())?;
                }
                check_machine(paths, &cfg, &run.repo, true)?;
            }
            let decision = Decision {
                fold,
                waive,
                approve,
                stop,
            };
            flow::decide(&mut run, &decision)?;
            let mut s = setting(paths, &cfg, &run_id, None)?;
            s.description = run.description.clone();
            let (repo, base) = (run.repo.clone(), run.base.clone());
            let caller = BridgeCaller::new(paths)?;
            let head = run.head.clone();
            let reloading =
                |fixed: bool| reload(&cfg, &repo, &base, head.as_deref().filter(|_| !fixed));
            flow::drive(&mut run, &s, &caller, &reloading)?;
            return finish(paths, &run, &cfg, post_pr);
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
        AcrCmd::JudgeCheck { keys, out } => {
            for key in keys {
                let result = calibrate(paths, &key)?;
                println!(
                    "{}: blockers agreed {}, missed {}, overrated {}; non-blockers agreed {}; unrated {}",
                    result["key"].as_str().unwrap_or_default(),
                    result["agreed_blockers"],
                    result["missed_blockers"],
                    result["overrated"],
                    result["agreed_non_blockers"],
                    result["unrated"],
                );
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
    let started = std::time::Instant::now();
    let wrong = trial::check_session(paths, &s, &key, &canary);
    report("a builder's session", started, wrong);

    // The separated design's bindings: one call for each bridge and set of
    // tools, since that's what decides how a session starts.
    let target = Target {
        repo: s.tree.clone(),
        base: key.base.clone(),
        config: None,
        head: Some(key.head.clone()),
        human_gate: None,
        max_rounds: None,
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
    let copies = copies_root(paths).join(format!("check-{}", new_id()));
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
            repo: &tree,
        });
        let wrong = answer.map(|a| trial::check_answer(&a.text, &key.head, &canary.secret, bash));
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
    }
    let _ = fs::remove_dir_all(&copies);
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
            repo: &s.tree,
        });
        let wrong = answer.map(|a| {
            let mut wrong = trial::check_answer(&a.text, &key.head, &canary.secret, true);
            if fs::read_to_string(&result).is_err() {
                wrong.push(format!("it didn't write {} in the copy", result.display()));
            }
            wrong
        });
        let _ = fs::remove_file(&result);
        report(
            &format!("{STEELMAN_BINDING} through {}, writing", b.agent),
            started,
            wrong,
        );
    }
    drop(allowed);
    if !ok && let Some(kept) = s.keep(paths) {
        println!(
            "What the builder's session left is kept in {}.",
            kept.display()
        );
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
    change::checked_out(&target.repo, target.head.as_deref())?;
    allow_copies(paths, &cfg)?;
    let id = new_id();
    let list = not_findings(&cfg, &source, &target.repo, &target.base)?;
    let mut s = setting(paths, &cfg, &id, Some(list))?;
    s.description = description;
    s.converge = converge;
    s.stop_on_failure = stop_on_failure;
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

/// `acrbus acr calibrate`, for one key: its own review's findings, each
/// tagged, go to the pipeline's Judge one dimension at a time, in a copy of
/// the change, and its ratings are set against the review's.
/// A folder that goes when this does.
struct Gone(PathBuf);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
    // The configurer, and its fallback, are held to the models this
    // repository's review may use: its config's, or the user's for every
    // repository.
    let mut configuring = vec![(binding_name.as_str(), &binding)];
    if let Some((name, b)) = binding
        .fallback
        .as_ref()
        .and_then(|f| user.allowed_bindings.get_key_value(f))
    {
        configuring.push((name.as_str(), b));
    }
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
    let root = copies_root(paths).join(format!("configure-{}", crate::util::new_id()));
    let _gone = Gone(root.clone());
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
    let bridged = BridgeCaller::new(paths)?;
    let caller = flow::WithFallbacks {
        inner: &bridged,
        bindings: &user.allowed_bindings,
    };
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

fn calibrate(paths: &Paths, key_path: &Path) -> Result<serde_json::Value> {
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
    let mut dimensions: Vec<&str> = key.findings.iter().map(|f| f.dimension.as_str()).collect();
    dimensions.sort();
    dimensions.dedup();
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
        let raws: Vec<roles::Raw> = key
            .findings
            .iter()
            .enumerate()
            .filter(|(_, f)| f.dimension == d)
            .map(|(i, f)| roles::Raw {
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
            })
            .collect();
        let prompt = roles::judge_prompt(&lens, &[raws], &decided)?
            + "\n\nKeep each finding's title as it's given, with its [K..] tag, and judge each \
               on its own: don't merge them.";
        let answer = caller.call(&Call {
            role: Role::Judge,
            binding_name: cfg.binding_name(Role::Judge, None)?,
            binding: judge,
            prompt: &prompt,
            artifact: Some(&artifact),
            writes: false,
            repo: &s.tree,
        })?;
        for r in roles::read_ratings(&answer.text)? {
            let Some(i) = tag
                .captures(&r.title)
                .and_then(|c| c[1].parse::<usize>().ok())
                .filter(|i| *i < judged.len())
            else {
                continue;
            };
            let flow::Ruled {
                severity, action, ..
            } = flow::ruling(
                &cfg,
                &lens,
                &flow::grounded(r, &lines, lens.blocker.as_deref()),
            );
            let blocks = matches!(
                action,
                config::Action::Blocks | config::Action::BlocksUnlessWaived
            );
            judged[i] = Some((blocks, format!("{severity:?}")));
        }
    }
    drop(allowed);
    let mut rows = Vec::new();
    let (mut agreed_b, mut missed, mut over, mut agreed_n, mut unrated) = (0, 0, 0, 0, 0);
    for (f, j) in key.findings.iter().zip(&judged) {
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
    Ok(json!({
        "key": key_path.file_stem().map(|n| n.to_string_lossy().into_owned()),
        "agreed_blockers": agreed_b, "missed_blockers": missed, "overrated": over,
        "agreed_non_blockers": agreed_n, "unrated": unrated, "dropped": dropped,
        "findings": rows,
        "repo": change::name(&s.tree)?, "repo_path": key.repo, "fingerprint": fp,
    }))
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
    let prompt = roles::judge_prompt(&lens, &[Vec::new()], "")?;
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
             accepted. Check its config still fits, then accept it again with `{APP_NAME} acr \
             calibration --accept`.{feedback}"
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
            scope.contains("approve its scope, tier core") && scope.contains("resume r1 --approve"),
            "{scope}"
        );
        // The fold says how to let the Orchestrator fix, only where it can't.
        let fold = next_steps(&run("waiting", Some("fold"), none.clone()), &cfg);
        assert!(
            fold.contains("--fold <ids>") && !fold.contains("writes: true"),
            "{fold}"
        );
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
        assert_eq!(super::run(&paths, asked).unwrap(), ExitCode::FAILURE);
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
            fold: Vec::new(),
            waive: Vec::new(),
            approve: !stop,
            stop,
            post_pr: None,
        };
        // Going on needs the head checked out, before anything else.
        let err = format!("{:#}", super::run(&paths, resume(false)).unwrap_err());
        assert!(err.contains("has to be what's checked out"), "{err}");
        super::run(&paths, resume(true)).unwrap();
        // Ended, and tidied away as a finished review is.
        assert!(!run_dir(&paths, "r-stop").join("run.json").exists());
        let _ = fs::remove_dir_all(paths.config.parent().unwrap());
        let _ = fs::remove_dir_all(&repo);
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
    fn a_review_sends_as_itself_never_a_session() {
        let paths = paths();
        assert_eq!(BridgeCaller::new(&paths).unwrap().agent, "acr");
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
            repo: Path::new("."),
        };
        let err = caller.call(&call).err().unwrap();
        assert!(format!("{err:#}").contains("writes: true"), "{err:#}");
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
}
