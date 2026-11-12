//! What a review is doing while it runs, so that a person can see it isn't
//! hung. A review can take an hour, and its calls most of that, so it says
//! when each call starts and ends, what each stage is, and at least once a
//! minute, every call still at work: how long it has run, when its bridge
//! last saw it use CPU or change a file, the tokens it has used so far where
//! its bridge can tell, and one waiting for a free slot on a full bridge.
//! With the plan's estimate, it also says what the run has used against it,
//! and warns once when a call passes its share of the most the plan allows,
//! or the run passes that most.
//!
//! It decides nothing. A call is never cut off while it works: its bridge
//! ends one that stalls, and a person can stop a review. This only tells
//! them what they need to judge.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::bridge::{self, LiveTokens, Standing};
use crate::config::{APP_NAME, Paths};
use crate::util::now_ms;

/// The longest a person waits for a line while a review runs.
pub const EVERY: Duration = Duration::from_secs(60);

/// Where a call stands, by its bridge's name and its request's ID.
type Look = dyn Fn(&str, &str) -> Standing + Send + Sync;

pub struct Progress {
    shared: Arc<Shared>,
    ticker: Option<thread::JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
    every: Duration,
    look: Box<Look>,
    out: Mutex<Box<dyn Write + Send>>,
    log: Mutex<Option<PathBuf>>,
    began: Instant,
}

struct State {
    calls: BTreeMap<u64, Live>,
    next: u64,
    /// When the last line was written.
    said: Instant,
    /// The stage under way, as last noted, and since when.
    doing: Option<(String, Instant)>,
    /// What the calls that have ended used, and what came before this
    /// process, for a review resumed.
    spent: Spent,
    budget: Option<Budget>,
    /// The run's total passed the plan's most, and was told so.
    run_warned: bool,
}

/// What the plan estimated a review takes, in tokens not read from a
/// cache, as [`Spent::fresh`] counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub expected: Option<u64>,
    pub most: u64,
}

/// What calls used: tokens not read from a cache, which is what a plan
/// estimates; those read from one, shown apart, since an agent's can be many
/// times the rest; and dollars, where its agent or the config says.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Spent {
    pub fresh: u64,
    pub cached: u64,
    pub usd: f64,
}

impl Spent {
    fn add(&mut self, t: &LiveTokens) {
        self.fresh += t.fresh();
        self.cached += t.cache_read;
        self.usd += t.usd.unwrap_or(0.0);
    }
}

struct Live {
    label: String,
    bridge: String,
    id: Option<String>,
    started: Instant,
    /// Its share of the plan's most, in tokens.
    share: Option<u64>,
    /// It passed its share, and was told so.
    warned: bool,
}

/// A call at work, as the heartbeat reads it: its number, label, bridge,
/// request, how long it has run, and its share of the plan.
type AtWork = (u64, String, String, Option<String>, Duration, Option<u64>);

/// A call being followed, from `start` to `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token(u64);

impl Progress {
    /// Reports to stderr, looking calls up on this machine's bridges.
    pub fn new(paths: &Paths) -> Progress {
        let paths = Paths {
            state: paths.state.clone(),
            config: paths.config.clone(),
        };
        // Tests drive real callers, and their lines aren't anyone's to read.
        let out: Box<dyn Write + Send> = if cfg!(test) {
            Box::new(std::io::sink())
        } else {
            Box::new(std::io::stderr())
        };
        Progress::with(
            out,
            Box::new(move |agent, id| bridge::standing(&paths, agent, id)),
            EVERY,
        )
    }

    pub fn with(out: Box<dyn Write + Send>, look: Box<Look>, every: Duration) -> Progress {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                calls: BTreeMap::new(),
                next: 0,
                said: Instant::now(),
                doing: None,
                spent: Spent::default(),
                budget: None,
                run_warned: false,
            }),
            stop: AtomicBool::new(false),
            every,
            look,
            out: Mutex::new(out),
            log: Mutex::new(None),
            began: Instant::now(),
        });
        let ticking = Arc::clone(&shared);
        let ticker = thread::Builder::new()
            .name("progress".into())
            .spawn(move || ticking.tick())
            .ok();
        Progress { shared, ticker }
    }

    /// Writes every line to `path` too, for whoever started the review in
    /// the background and reads it later.
    pub fn log_to(&self, path: PathBuf) {
        *lock(&self.shared.log) = Some(path);
    }

    /// A stage starting, such as a round: said now, and repeated while
    /// nothing else is.
    pub fn note(&self, what: &str) {
        lock(&self.shared.state).doing = Some((what.to_string(), Instant::now()));
        self.shared.say(&[what.to_string()]);
    }

    /// Something that happened, said now, without changing the stage: a
    /// lens with nothing for its Judge, say.
    pub fn line(&self, what: &str) {
        self.shared.say(&[what.to_string()]);
    }

    /// The plan's estimate, and what the review used before this process,
    /// to say the run's total against it as it goes.
    pub fn budget(&self, budget: Budget, spent: Spent) {
        let mut state = lock(&self.shared.state);
        state.budget = Some(budget);
        state.spent = spent;
        state.run_warned = spent.fresh > budget.most;
    }

    /// `share`: its part of the most the plan allows, to warn once it
    /// passes it.
    pub fn start(&self, label: &str, binding: &str, bridge: &str, share: Option<u64>) -> Token {
        let token = {
            let mut state = lock(&self.shared.state);
            state.next += 1;
            let n = state.next;
            state.calls.insert(
                n,
                Live {
                    label: label.to_string(),
                    bridge: bridge.to_string(),
                    id: None,
                    started: Instant::now(),
                    share,
                    warned: false,
                },
            );
            Token(n)
        };
        self.shared
            .say(&[format!("{label}: started, on {binding} through {bridge}")]);
        token
    }

    /// The request's ID, once the bus has it, to find it on its bridge.
    pub fn sent(&self, token: Token, id: &str) {
        if let Some(call) = lock(&self.shared.state).calls.get_mut(&token.0) {
            call.id = Some(id.to_string());
        }
    }

    /// `used`: the tokens its agent says it used, added to the run's total.
    pub fn end(&self, token: Token, failed: Option<&str>, used: Option<LiveTokens>) {
        let call = {
            let mut state = lock(&self.shared.state);
            if let Some(u) = &used {
                state.spent.add(u);
            }
            state.calls.remove(&token.0)
        };
        let Some(call) = call else {
            return;
        };
        let took = took(call.started.elapsed());
        let spent = used.map(|u| format!(", {}", spent(&u))).unwrap_or_default();
        let line = match failed {
            None => format!("{}: answered after {took}{spent}", call.label),
            Some(why) => format!("{}: failed after {took}{spent}: {why}", call.label),
        };
        self.shared.say(&[line]);
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
    }
}

impl Shared {
    fn tick(&self) {
        let step = (self.every / 10).clamp(Duration::from_millis(10), Duration::from_secs(1));
        while !self.stop.load(Ordering::SeqCst) {
            thread::sleep(step);
            let due = lock(&self.state).said.elapsed() >= self.every;
            if due {
                let lines = self.status();
                self.say(&lines);
            }
        }
    }

    /// Everything at work now, a line for each call, and the run's total
    /// against the plan, with any warning that's due.
    fn status(&self) -> Vec<String> {
        let (at_work, doing, spent_before, budget) = {
            let state = lock(&self.state);
            let at_work: Vec<AtWork> = state
                .calls
                .iter()
                .map(|(n, c)| {
                    (
                        *n,
                        c.label.clone(),
                        c.bridge.clone(),
                        c.id.clone(),
                        c.started.elapsed(),
                        c.share,
                    )
                })
                .collect();
            let doing = state
                .doing
                .as_ref()
                .map(|(what, since)| (what.clone(), since.elapsed()));
            (at_work, doing, state.spent, state.budget)
        };
        let now = now_ms();
        let mut lines = Vec::new();
        let mut total = spent_before;
        let mut warnings = Vec::new();
        let mut calls = Vec::new();
        for (n, label, bridge, id, ran, share) in at_work {
            let standing = id.as_deref().map(|id| (self.look)(&bridge, id));
            let tokens = match &standing {
                Some(Standing::Running { tokens, .. }) => *tokens,
                _ => None,
            };
            if let Some(t) = &tokens {
                total.add(t);
            }
            if let (Some(t), Some(share)) = (tokens, share)
                && t.fresh() > share
                && self.warn_call(n)
            {
                warnings.push(format!(
                    "warning: {label} has used {} tokens not read from a cache, past its share \
                     of the most the plan allows ({})",
                    short(t.fresh()),
                    short(share)
                ));
            }
            calls.push(format!(
                "  {label}: {}",
                described(standing.as_ref(), &bridge, ran, now)
            ));
        }
        let against = budget.map(|b| {
            let expected = b
                .expected
                .map(|e| format!("{} expected, ", short(e)))
                .unwrap_or_default();
            format!(
                "; the run has used {} tokens so far, of {expected}{} at most{}",
                short(total.fresh),
                short(b.most),
                besides(&total)
            )
        });
        if let Some(b) = budget
            && total.fresh > b.most
            && self.warn_run()
        {
            warnings.push(format!(
                "warning: the run has used {} tokens not read from a cache, past the most the \
                 plan allows ({})",
                short(total.fresh),
                short(b.most)
            ));
        }
        if calls.is_empty() {
            lines.push(match doing {
                Some((what, since)) => format!(
                    "still at work, {} into: {what}{}",
                    took(since),
                    against.unwrap_or_default()
                ),
                None => format!("still at work{}", against.unwrap_or_default()),
            });
        } else {
            lines.push(format!(
                "{} call(s) at work{}:",
                calls.len(),
                against.unwrap_or_default()
            ));
            lines.extend(calls);
        }
        lines.extend(warnings);
        lines
    }

    /// Whether call `n` is still to be warned about; true once.
    fn warn_call(&self, n: u64) -> bool {
        let mut state = lock(&self.state);
        match state.calls.get_mut(&n) {
            Some(c) if !c.warned => {
                c.warned = true;
                true
            }
            _ => false,
        }
    }

    /// Whether the run is still to be warned about; true once.
    fn warn_run(&self) -> bool {
        let mut state = lock(&self.state);
        !std::mem::replace(&mut state.run_warned, true)
    }

    fn say(&self, lines: &[String]) {
        let stamp = took(self.began.elapsed());
        let text: String = lines
            .iter()
            .enumerate()
            .map(|(n, line)| {
                if n == 0 {
                    format!("{APP_NAME} [{stamp}]: {line}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        {
            let mut out = lock(&self.out);
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
        if let Some(path) = lock(&self.log).as_ref()
            && let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = file.write_all(text.as_bytes());
        }
        lock(&self.state).said = Instant::now();
    }
}

/// How a call at work is doing, in words a person can judge it by.
fn described(standing: Option<&Standing>, bridge: &str, ran: Duration, now: i64) -> String {
    let ran = took(ran);
    match standing {
        None => format!("{ran}, being sent"),
        Some(Standing::Down) => {
            format!("{ran}, but its bridge, {bridge}, isn't running: it can't answer until it is")
        }
        Some(Standing::Waiting {
            max_concurrent,
            running,
        }) => format!(
            "waiting {ran} for a free call on {bridge} ({running} of {max_concurrent} running), or \
             just finished"
        ),
        Some(Standing::Running {
            active,
            idle_secs,
            tokens,
            ..
        }) => {
            let so_far = tokens
                .map(|t| format!(", {} so far", spent(&t)))
                .unwrap_or_default();
            let Some(at) = active else {
                return format!("{ran}, running{so_far}");
            };
            let quiet = Duration::from_millis((now - at).max(0) as u64);
            let idle = Duration::from_secs(*idle_secs);
            if *idle_secs > 0 && quiet * 2 >= idle {
                format!(
                    "{ran}, quiet for {}: {bridge} ends a run that's quiet for {}{so_far}",
                    took(quiet),
                    took(idle)
                )
            } else {
                format!("{ran}, active {} ago{so_far}", took(quiet))
            }
        }
    }
}

/// A call's tokens: `32k in, 8.2k out, 88k from cache, $0.12`, where in
/// is input not read from a cache.
fn spent(t: &LiveTokens) -> String {
    let cached = if t.cache_read > 0 {
        format!(", {} from cache", short(t.cache_read))
    } else {
        String::new()
    };
    let usd = t.usd.map(|u| format!(", ${u:.2}")).unwrap_or_default();
    format!(
        "{} in, {} out{cached}{usd}",
        short(t.input + t.cache_write),
        short(t.output)
    )
}

/// What a run used besides the tokens counted against its plan:
/// `, plus 31M read from cache, $4.12 where known`.
fn besides(s: &Spent) -> String {
    let mut out = String::new();
    if s.cached > 0 {
        out.push_str(&format!(", plus {} read from cache", short(s.cached)));
    }
    if s.usd > 0.0 {
        out.push_str(&format!(", ${:.2} where known", s.usd));
    }
    out
}

/// Tokens, short: `950`, `12k`, `1.7M`.
pub fn short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1e3),
        10_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// `45s`, `12m 3s`, `1h 2m`.
pub fn took(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What was written, for a test to read.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<u8>>>);

    impl Write for Seen {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seen {
        fn text(&self) -> String {
            String::from_utf8_lossy(&lock(&self.0)).into_owned()
        }
    }

    fn waits_for(seen: &Seen, what: &str) -> String {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let text = seen.text();
            if text.contains(what) || Instant::now() > until {
                return text;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn says_each_call_as_it_starts_and_ends() {
        let seen = Seen::default();
        let p = Progress::with(
            Box::new(seen.clone()),
            Box::new(|_, _| Standing::Down),
            Duration::from_secs(3600),
        );
        p.note("round 1: 2 lens(es) attack the change: a, b");
        let t = p.start("a: reviewer", "sonnet-high", "claude-acr", None);
        p.end(t, None, None);
        let t = p.start("b: reviewer", "sonnet-high", "claude-acr", None);
        p.end(t, Some("claude-acr timeout: it timed out"), None);
        let text = seen.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5, "{text}");
        assert!(lines[0].ends_with("round 1: 2 lens(es) attack the change: a, b"));
        assert!(lines[1].contains("a: reviewer: started, on sonnet-high through claude-acr"));
        assert!(lines[2].contains("a: reviewer: answered after 0s"));
        assert!(lines[4].contains("b: reviewer: failed after 0s: claude-acr timeout"));
        assert!(
            lines
                .iter()
                .all(|l| l.starts_with(&format!("{APP_NAME} [")))
        );
    }

    #[test]
    fn says_every_call_at_work_and_how_its_bridge_sees_it_at_a_steady_pace() {
        let seen = Seen::default();
        let now = now_ms();
        let p = Progress::with(
            Box::new(seen.clone()),
            Box::new(move |_, id| match id {
                "busy" => Standing::Running {
                    since: now - 600_000,
                    active: Some(now - 4_000),
                    idle_secs: 1200,
                    tokens: None,
                },
                "stalling" => Standing::Running {
                    since: now - 900_000,
                    active: Some(now - 660_000),
                    idle_secs: 1200,
                    tokens: None,
                },
                "queued" => Standing::Waiting {
                    max_concurrent: 5,
                    running: 5,
                },
                _ => Standing::Down,
            }),
            Duration::from_millis(100),
        );
        for (label, id) in [
            ("correctness: reviewer", "busy"),
            ("missed-context: reviewer", "stalling"),
            ("tests: the Judge", "queued"),
            ("claims: reviewer", "gone"),
        ] {
            let t = p.start(label, "glm-flash-high", "omp", None);
            p.sent(t, id);
        }
        let text = waits_for(&seen, "call(s) at work");
        drop(p);
        assert!(text.contains("4 call(s) at work:"), "{text}");
        assert!(
            text.contains("  correctness: reviewer: 0s, active 4s ago"),
            "{text}"
        );
        assert!(
            text.contains("missed-context: reviewer: 0s, quiet for 11m 0s: omp ends a run that's quiet for 20m 0s"),
            "{text}"
        );
        assert!(
            text.contains("tests: the Judge: waiting 0s for a free call on omp (5 of 5 running)"),
            "{text}"
        );
        assert!(
            text.contains("claims: reviewer: 0s, but its bridge, omp, isn't running"),
            "{text}"
        );
    }

    #[test]
    fn says_what_its_doing_even_when_no_call_is_at_work() {
        let seen = Seen::default();
        let p = Progress::with(
            Box::new(seen.clone()),
            Box::new(|_, _| Standing::Down),
            Duration::from_millis(100),
        );
        p.note("round 1: the tests gate runs the change's tests");
        let text = waits_for(&seen, "still at work");
        drop(p);
        assert!(
            text.contains("still at work, ")
                && text.contains(" into: round 1: the tests gate runs the change's tests"),
            "{text}"
        );
    }

    #[test]
    fn writes_the_same_lines_to_a_log_for_whoever_reads_it_later() {
        let dir = std::env::temp_dir().join(format!("acr-progress-{}", crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("progress.log");
        let seen = Seen::default();
        let p = Progress::with(
            Box::new(seen.clone()),
            Box::new(|_, _| Standing::Down),
            Duration::from_secs(3600),
        );
        p.log_to(log.clone());
        p.note("round 2: 1 lens(es) attack the change as fixed: a");
        drop(p);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), seen.text());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn times_read_the_way_a_person_says_them() {
        assert_eq!(took(Duration::from_secs(45)), "45s");
        assert_eq!(took(Duration::from_secs(723)), "12m 3s");
        assert_eq!(took(Duration::from_secs(3720)), "1h 2m");
    }

    #[test]
    fn says_each_calls_tokens_so_far_and_the_runs_against_the_plan_and_warns_once() {
        let seen = Seen::default();
        let tokens = |input, output| LiveTokens {
            input,
            cache_read: 5_000_000,
            cache_write: 0,
            output,
            usd: Some(0.5),
        };
        let p = Progress::with(
            Box::new(seen.clone()),
            Box::new(move |_, id| match id {
                "runaway" => Standing::Running {
                    since: now_ms(),
                    active: Some(now_ms()),
                    idle_secs: 0,
                    tokens: Some(tokens(900_000, 20_000)),
                },
                _ => Standing::Running {
                    since: now_ms(),
                    active: Some(now_ms()),
                    idle_secs: 0,
                    tokens: Some(tokens(40_000, 2_000)),
                },
            }),
            Duration::from_millis(100),
        );
        p.budget(
            Budget {
                expected: Some(600_000),
                most: 1_000_000,
            },
            Spent {
                fresh: 100_000,
                cached: 0,
                usd: 0.0,
            },
        );
        let t = p.start("a: reviewer", "flash", "pi", Some(200_000));
        p.sent(t, "runaway");
        let u = p.start("b: reviewer", "flash", "pi", Some(200_000));
        p.sent(u, "fine");
        let text = waits_for(&seen, "past the most the plan allows");
        thread::sleep(Duration::from_millis(350));
        let later = seen.text();
        drop(p);
        assert!(
            text.contains(
                "a: reviewer: 0s, active 0s ago, 900k in, 20k out, 5.0M from cache, $0.50 so far"
            ),
            "{text}"
        );
        // Cache reads are shown apart: the plan doesn't count them.
        assert!(
            text.contains("the run has used 1.1M tokens so far, of 600k expected, 1.0M at most, plus 10.0M read from cache, $1.00 where known"),
            "{text}"
        );
        assert!(
            text.contains("warning: a: reviewer has used 920k tokens not read from a cache, past its share of the most the plan allows (200k)"),
            "{text}"
        );
        assert!(!text.contains("warning: b: reviewer"), "{text}");
        assert_eq!(
            later.matches("warning: a: reviewer").count(),
            1,
            "once: {later}"
        );
        assert_eq!(
            later
                .matches("past the most the plan allows (1.0M)")
                .count(),
            1,
            "{later}"
        );
    }

    #[test]
    fn an_answer_adds_its_tokens_to_the_run_and_says_them() {
        let seen = Seen::default();
        let p = Progress::with(
            Box::new(seen.clone()),
            Box::new(|_, _| Standing::Down),
            Duration::from_secs(3600),
        );
        p.budget(
            Budget {
                expected: None,
                most: 50_000,
            },
            Spent::default(),
        );
        let t = p.start(
            "the Sizer, choosing a tier from the file list",
            "flash",
            "pi",
            None,
        );
        let used = LiveTokens {
            input: 3_000,
            cache_read: 1_000,
            cache_write: 0,
            output: 120,
            usd: Some(0.008),
        };
        p.end(t, None, Some(used));
        p.line("probes: no findings, nothing for the Judge");
        let spent = lock(&p.shared.state).spent;
        assert_eq!((spent.fresh, spent.cached), (3_120, 1_000));
        let text = seen.text();
        assert!(
            text.contains("the Sizer, choosing a tier from the file list: answered after 0s, 3.0k in, 120 out, 1.0k from cache, $0.01"),
            "{text}"
        );
        assert!(
            text.contains("probes: no findings, nothing for the Judge"),
            "{text}"
        );
    }
}
