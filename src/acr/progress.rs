//! What a review is doing while it runs, so that a person can see it isn't
//! hung. A review can take an hour, and its calls most of that, so it says
//! when each call starts and ends, what each stage is, and at least once a
//! minute, every call still at work: how long it has run, and when its
//! bridge last saw it use CPU or change a file.
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

use crate::bridge::{self, Standing};
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
}

struct Live {
    label: String,
    bridge: String,
    id: Option<String>,
    started: Instant,
}

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
        Progress::with(
            Box::new(std::io::stderr()),
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

    pub fn start(&self, label: &str, binding: &str, bridge: &str) -> Token {
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

    pub fn end(&self, token: Token, failed: Option<&str>) {
        let Some(call) = lock(&self.shared.state).calls.remove(&token.0) else {
            return;
        };
        let took = took(call.started.elapsed());
        let line = match failed {
            None => format!("{}: answered after {took}", call.label),
            Some(why) => format!("{}: failed after {took}: {why}", call.label),
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

    /// Everything at work now, a line for each call.
    fn status(&self) -> Vec<String> {
        let state = lock(&self.state);
        let at_work: Vec<(String, String, Option<String>, Duration)> = state
            .calls
            .values()
            .map(|c| {
                (
                    c.label.clone(),
                    c.bridge.clone(),
                    c.id.clone(),
                    c.started.elapsed(),
                )
            })
            .collect();
        let doing = state
            .doing
            .as_ref()
            .map(|(what, since)| (what.clone(), since.elapsed()));
        drop(state);
        if at_work.is_empty() {
            return vec![match doing {
                Some((what, since)) => format!("still at work, {} into: {what}", took(since)),
                None => "still at work".to_string(),
            }];
        }
        let mut lines = vec![format!("{} call(s) at work:", at_work.len())];
        let now = now_ms();
        for (label, bridge, id, ran) in at_work {
            let standing = id.as_deref().map(|id| (self.look)(&bridge, id));
            lines.push(format!(
                "  {label}: {}",
                described(standing.as_ref(), &bridge, ran, now)
            ));
        }
        lines
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
        Some(Standing::Waiting { max_concurrent }) => format!(
            "{ran}, waiting for one of {bridge}'s {max_concurrent} slot(s), or just finished"
        ),
        Some(Standing::Running {
            active, idle_secs, ..
        }) => {
            let Some(at) = active else {
                return format!("{ran}, running");
            };
            let quiet = Duration::from_millis((now - at).max(0) as u64);
            let idle = Duration::from_secs(*idle_secs);
            if *idle_secs > 0 && quiet * 2 >= idle {
                format!(
                    "{ran}, quiet for {}: {bridge} ends a run that's quiet for {}",
                    took(quiet),
                    took(idle)
                )
            } else {
                format!("{ran}, active {} ago", took(quiet))
            }
        }
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
        let t = p.start("a: reviewer", "sonnet-high", "claude-acr");
        p.end(t, None);
        let t = p.start("b: reviewer", "sonnet-high", "claude-acr");
        p.end(t, Some("claude-acr timeout: it timed out"));
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
                },
                "stalling" => Standing::Running {
                    since: now - 900_000,
                    active: Some(now - 660_000),
                    idle_secs: 1200,
                },
                "queued" => Standing::Waiting { max_concurrent: 5 },
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
            let t = p.start(label, "glm-flash-high", "omp");
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
            text.contains("tests: the Judge: 0s, waiting for one of omp's 5 slot(s)"),
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
}
