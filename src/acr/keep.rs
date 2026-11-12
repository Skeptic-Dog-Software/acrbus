//! What ACR keeps, and for how long. A finished review leaves a line of
//! history, which estimates come from, and its files, packed into
//! `<time>-<id>.tar.gz` as it finishes, to be looked at for
//! `cleanup::KEEP`. One waiting for a person keeps them as they are, to go
//! on from, until `UNFINISHED`. Nothing removes them as a review ends: the
//! daemon's hourly cleanup clears them once they're old, which is cheap: a
//! look at each name, and nothing read.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::flow::Run;
use super::plan::Record;
use crate::config::{APP_NAME, Paths};

const HISTORY: &str = "history.jsonl";
/// The finished reviews of each repository that history keeps.
pub const HISTORY_RUNS: usize = 20;
/// How long a review waiting for a person stays before it's taken as left.
const UNFINISHED: Duration = crate::cleanup::KEEP;
/// In a review's directory while a process is driving it: that process's
/// ID, and on the next line when it started, as `ps` says it, so a later
/// process given the same ID is never taken for it.
const ACTIVE_MARK: &str = "pid";

/// Marks a review as under way in this process until it's dropped, so an
/// update to acrbus waits for it rather than cutting it short.
pub struct Active(PathBuf);

impl Drop for Active {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn active_guard(paths: &Paths, id: &str) -> Result<Active> {
    let review = dir(paths).join(id);
    fs::create_dir_all(&review)?;
    let mark = review.join(ACTIVE_MARK);
    let pid = std::process::id();
    let started = crate::bridge::process_started(pid as i32).unwrap_or_default();
    fs::write(&mark, format!("{pid}\n{started}\n"))?;
    Ok(Active(mark))
}

/// Whether process `pid` is running.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    let ok = unsafe { libc::kill(pid, 0) } == 0;
    ok || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The command line of process `pid`, as `ps` gives it.
fn command_of(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The process driving review `id` now, if one is: the ID its mark names,
/// only while that's still the process that wrote it. A mark from before
/// marks said when their process started is trusted only for a process
/// that's running an `acr` command.
pub fn live(paths: &Paths, id: &str) -> Option<i32> {
    let mark = fs::read_to_string(dir(paths).join(id).join(ACTIVE_MARK)).ok()?;
    let mut lines = mark.lines();
    let pid = lines.next()?.trim().parse::<i32>().ok()?;
    if pid <= 0 || !alive(pid) {
        return None;
    }
    let recorded = lines.next().unwrap_or_default().trim();
    let same = if recorded.is_empty() {
        command_of(pid).is_some_and(|c| c.split_whitespace().any(|w| w == "acr"))
    } else {
        crate::bridge::process_started(pid).as_deref() == Some(recorded)
    };
    same.then_some(pid)
}

/// Removes review `id`'s mark once no live process holds it: one a stopped
/// process left behind, since stopping it skips what removes it.
pub fn clear_stale(paths: &Paths, id: &str) {
    if live(paths, id).is_none() {
        let _ = fs::remove_file(dir(paths).join(id).join(ACTIVE_MARK));
    }
}

/// Marks review `id` as driven by process `pid`, as its own process would
/// mark it, with `started` in place of when it started, if given.
#[cfg(test)]
pub fn mark_for(paths: &Paths, id: &str, pid: i32, started: Option<&str>) {
    let review = dir(paths).join(id);
    fs::create_dir_all(&review).unwrap();
    let started = match started {
        Some(s) => s.to_string(),
        None => crate::bridge::process_started(pid).unwrap_or_default(),
    };
    fs::write(review.join(ACTIVE_MARK), format!("{pid}\n{started}\n")).unwrap();
}

/// The reviews under way on this machine now.
pub fn active(paths: &Paths) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().join(ACTIVE_MARK).exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| live(paths, id).is_some())
        .collect()
}

/// In a review's directory once it's finished.
const FINISHED_MARK: &str = "finished";

/// Whether review `id` has finished. Its files are kept a while, but it
/// can't go on.
pub fn is_finished(paths: &Paths, id: &str) -> bool {
    kept(paths, id).is_some()
}

/// Where a finished review's files are kept: its archive, or its folder,
/// marked, should packing it have failed.
pub fn kept(paths: &Paths, id: &str) -> Option<PathBuf> {
    fs::read_dir(dir(paths))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|path| {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            match name.strip_suffix(".tar.gz") {
                Some(base) => crate::util::unstamped(base) == id,
                None => crate::util::unstamped(&name) == id && path.join(FINISHED_MARK).exists(),
            }
        })
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.state.join("acr")
}

/// Holds the history's lock, so two reviews finishing at once can't lose
/// one's line.
struct Locked(File);

impl Locked {
    fn take(paths: &Paths) -> Result<Locked> {
        fs::create_dir_all(dir(paths))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(dir(paths).join("history.lock"))?;
        // SAFETY: flock on a descriptor this owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("locking ACR's history");
        }
        Ok(Locked(file))
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_history(paths: &Paths) -> Vec<Record> {
    let Ok(file) = File::open(dir(paths).join(HISTORY)) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str(&line).ok())
        .collect()
}

/// The newest `HISTORY_RUNS` of each repository's, oldest first.
fn trimmed(mut records: Vec<Record>) -> Vec<Record> {
    records.sort_by(|a, b| a.id.cmp(&b.id));
    records.dedup_by(|a, b| a.id == b.id);
    let mut per_repo: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut kept: Vec<Record> = records
        .into_iter()
        .rev()
        .filter(|r| {
            let n = per_repo.entry(r.repo.clone()).or_default();
            *n += 1;
            *n <= HISTORY_RUNS
        })
        .collect();
    kept.reverse();
    kept
}

fn add(paths: &Paths, record: Record) -> Result<()> {
    let _lock = Locked::take(paths)?;
    let mut records = read_history(paths);
    records.push(record);
    let mut text = String::new();
    for r in trimmed(records) {
        text.push_str(&serde_json::to_string(&r)?);
        text.push('\n');
    }
    let path = dir(paths).join(HISTORY);
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// The finished reviews of `repo` that history has.
pub fn history(paths: &Paths, repo: &Path) -> Vec<Record> {
    let want = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    read_history(paths)
        .into_iter()
        .filter(|r| r.repo == want)
        .collect()
}

/// Once a review is finished: notes it in history, if `noted`, marks it
/// finished, and packs its files into `<time>-<id>.tar.gz`, to be looked at
/// until the daemon's cleanup clears them. Its report has been printed. A
/// review that isn't finished keeps them as they are, to go on from.
pub fn finished(paths: &Paths, run: &Run, noted: bool) -> Result<()> {
    if !matches!(
        run.status,
        super::flow::Status::Confirmed | super::flow::Status::Stopped
    ) || is_finished(paths, &run.id)
    {
        return Ok(());
    }
    if noted && let Some(record) = Record::of(run) {
        add(paths, record)?;
    }
    // Its findings stay a while, for its user to say how it did. A review
    // that can't keep them still finishes.
    if noted
        && let Err(e) = super::configure::keep_findings(&paths.state, run, crate::util::now_ms())
    {
        eprintln!(
            "{APP_NAME}: couldn't keep review {}'s findings for feedback: {e:#}",
            run.id
        );
    }
    let review = dir(paths).join(&run.id);
    fs::create_dir_all(&review)
        .and_then(|()| File::create(review.join(FINISHED_MARK)))
        .with_context(|| format!("marking {} finished", review.display()))?;
    // Packed under its time, so an agent that searches files later doesn't
    // come on its findings. Should that fail, the daemon's cleanup packs it.
    if let Err(e) = pack(&review) {
        eprintln!("{APP_NAME}: couldn't pack review {}'s files: {e:#}", run.id);
    }
    Ok(())
}

/// Packs a finished review's folder, named for the time now if it isn't
/// named with one.
fn pack(review: &Path) -> Result<PathBuf> {
    let name = review.file_name().unwrap_or_default().to_string_lossy();
    let review = if crate::util::unstamped(&name) == name {
        let stamped = review.with_file_name(crate::util::stamped(&name));
        fs::rename(review, &stamped)?;
        stamped
    } else {
        review.to_path_buf()
    };
    crate::util::pack(&review)
}

/// Removes reviews finished `cleanup::KEEP` ago, going by the time they're
/// named for, and ones left waiting after `UNFINISHED`, going by when each
/// was last saved. Packs a finished one that wasn't, once it's been
/// `cleanup::PACK_AFTER`. Returns how many it removed. The daemon's hourly
/// cleanup calls it.
pub fn prune(paths: &Paths) -> usize {
    prune_at(paths, SystemTime::now())
}

fn prune_at(paths: &Paths, now: SystemTime) -> usize {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return 0;
    };
    let running = active(paths);
    let mut gone = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let age = crate::util::age_at(&path, now).unwrap_or_default();
        if name.contains(".tar.gz") {
            if age > crate::cleanup::KEEP && fs::remove_file(&path).is_ok() {
                gone += 1;
            }
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_dir())
            || running.iter().any(|id| id == crate::util::unstamped(&name))
        {
            continue;
        }
        if path.join(FINISHED_MARK).exists() {
            if age > crate::cleanup::KEEP {
                gone += usize::from(fs::remove_dir_all(&path).is_ok());
            } else if age > crate::cleanup::PACK_AFTER {
                let _ = pack(&path);
            }
            continue;
        }
        let saved = fs::metadata(path.join("run.json"))
            .or_else(|_| entry.metadata())
            .and_then(|m| m.modified());
        if saved.is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > UNFINISHED))
            && fs::remove_dir_all(&path).is_ok()
        {
            gone += 1;
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::new_id;

    fn paths() -> Paths {
        let home = std::env::temp_dir().join(format!("tb-keep-{}", &new_id()[14..]));
        fs::create_dir_all(&home).unwrap();
        Paths {
            config: home.join("config.toml"),
            state: home,
        }
    }

    fn record(id: &str, repo: &str) -> Record {
        Record {
            id: id.to_string(),
            repo: PathBuf::from(repo),
            lenses: BTreeMap::from([("a".to_string(), 2)]),
            fixes: 1,
            tokens: 10,
            usd: Some(0.5),
        }
    }

    #[test]
    fn a_review_under_way_is_known_until_it_ends() {
        let p = paths();
        assert!(active(&p).is_empty());
        let guard = active_guard(&p, "r1").unwrap();
        assert_eq!(active(&p), ["r1"]);
        // A mark left by a process that's gone doesn't count.
        let gone = dir(&p).join("r2");
        fs::create_dir_all(&gone).unwrap();
        fs::write(gone.join(ACTIVE_MARK), "999999").unwrap();
        assert_eq!(active(&p), ["r1"]);
        drop(guard);
        assert!(active(&p).is_empty());
        fs::remove_dir_all(&p.state).unwrap();
    }

    #[test]
    fn history_keeps_the_newest_of_each_repository() {
        let p = paths();
        for n in 0..HISTORY_RUNS + 5 {
            add(&p, record(&format!("{n:04}"), "/r/one")).unwrap();
        }
        add(&p, record("0002", "/r/two")).unwrap();
        // The same review twice is one review.
        add(&p, record("0002", "/r/two")).unwrap();
        let one = history(&p, Path::new("/r/one"));
        assert_eq!(one.len(), HISTORY_RUNS);
        assert_eq!(one[0].id, "0005", "the oldest went");
        assert_eq!(history(&p, Path::new("/r/two")).len(), 1);
        fs::remove_dir_all(&p.state).unwrap();
    }

    #[test]
    fn a_finished_review_is_packed_and_kept_a_while_and_one_left_waiting_longer() {
        let p = paths();
        let review = |id: &str| {
            let review = dir(&p).join(id);
            fs::create_dir_all(&review).unwrap();
            fs::write(review.join("run.json"), "{\"found\": \"a finding\"}").unwrap();
            review
        };
        review("waiting");
        // Finished, but its packing failed: the cleanup packs it later.
        let left = review("left");
        File::create(left.join(FINISHED_MARK)).unwrap();
        assert!(is_finished(&p, "left") && !is_finished(&p, "waiting"));
        let archive = pack(&review("done")).unwrap();
        assert!(is_finished(&p, "done") && !dir(&p).join("done").exists());
        let name = archive.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(crate::util::unstamped(&name), "done.tar.gz", "{name}");
        let now = SystemTime::now();
        let minute = Duration::from_secs(60);
        assert_eq!(prune_at(&p, now + minute), 0, "all stay");
        assert!(left.exists(), "not an hour yet");
        assert_eq!(prune_at(&p, now + crate::cleanup::PACK_AFTER + minute), 0);
        assert!(!left.exists() && is_finished(&p, "left"), "packed now");
        assert_eq!(prune_at(&p, now + crate::cleanup::KEEP - minute), 0);
        // Finished or left waiting, each goes once it's past keeping.
        assert_eq!(prune_at(&p, now + crate::cleanup::KEEP + minute), 3);
        assert!(!is_finished(&p, "done") && !is_finished(&p, "left"));
        assert!(!dir(&p).join("waiting").exists());
        fs::remove_dir_all(&p.state).unwrap();
    }
}
