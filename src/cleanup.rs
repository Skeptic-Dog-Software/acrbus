//! The daemon's hourly cleanup. Bridges and reviews delete nothing as they
//! finish: every run's log and folder, a review's files, reviewers' copies
//! and a trial's copy stay, to be looked at, until they're older than their
//! keeping. Then this clears them, and nothing else does. Cheap: a look at
//! each folder, and nothing read.

use std::fs;
use std::time::Duration;

use crate::config::Paths;

/// Days what a run or a review left is kept, unless a bridge's
/// `log_retention_days` says otherwise for its runs.
pub const KEEP_DAYS: u32 = 14;
pub const KEEP: Duration = Duration::from_secs(KEEP_DAYS as u64 * 86_400);
/// How often the daemon cleans up.
pub const EVERY: Duration = Duration::from_secs(3600);

/// Clears what's past keeping in this machine's state. Returns what it did,
/// a line for each place, for the daemon's log.
pub fn sweep(paths: &Paths) -> Vec<String> {
    let mut said = Vec::new();
    let bridges = fs::read_dir(paths.state.join("bridge"));
    let mut agents: Vec<String> = bridges
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    agents.sort();
    for agent in agents {
        let swept = crate::bridge::sweep(paths, &agent);
        if swept.runs > 0 {
            said.push(format!(
                "cleared {} run(s) the {agent} bridge kept past their keeping",
                swept.runs
            ));
        }
        if swept.log_cut {
            said.push(format!(
                "cut the {agent} bridge's log back to its last lines"
            ));
        }
    }
    let acr = crate::acr::sweep(paths);
    if acr > 0 {
        said.push(format!(
            "cleared {acr} review folder(s), copies and trials past their keeping"
        ));
    }
    said
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;

    /// Makes `path` look last changed `days` ago.
    fn age(path: &Path, days: u64) {
        let then = SystemTime::now() - Duration::from_secs(days * 86_400);
        fs::File::open(path).unwrap().set_modified(then).unwrap();
    }

    #[test]
    fn the_cleanup_clears_only_whats_past_keeping_and_never_an_open_run() {
        let home = std::env::temp_dir().join(format!("tb-cleanup-{}", crate::util::new_id()));
        let paths = Paths {
            config: home.join("config.toml"),
            state: home.join("state"),
        };
        let bridge = paths.state.join("bridge/claude");
        let mut old = Vec::new();
        let mut new = Vec::new();
        for (place, days) in [
            ("bridge/claude/runs/OLD", KEEP_DAYS + 1),
            ("bridge/claude/log/OLD", KEEP_DAYS + 1),
            ("bridge/claude/runs/NEW", 1),
            ("bridge/claude/runs/OPEN", KEEP_DAYS + 1),
            ("acr-copies/r1-r1-x", KEEP_DAYS + 1),
            ("acr-copies/r2-r1-x", 1),
            ("acr-trials/t1", KEEP_DAYS + 1),
        ] {
            let path = paths.state.join(place);
            fs::create_dir_all(path.join("1")).unwrap();
            age(&path, u64::from(days));
            if days > KEEP_DAYS && !place.ends_with("OPEN") {
                old.push(path);
            } else {
                new.push(path);
            }
        }
        fs::create_dir_all(bridge.join("inflight")).unwrap();
        fs::write(bridge.join("inflight/OPEN.json"), "{}").unwrap();
        let said = sweep(&paths);
        assert!(
            said.iter()
                .any(|l| l.contains("cleared 1 run(s) the claude bridge")),
            "{said:?}"
        );
        for path in &old {
            assert!(!path.exists(), "{} wasn't cleared", path.display());
        }
        for path in &new {
            assert!(path.exists(), "{} was cleared", path.display());
        }
        assert!(sweep(&paths).is_empty(), "nothing more to clear");
        fs::remove_dir_all(&home).unwrap();
    }
}
