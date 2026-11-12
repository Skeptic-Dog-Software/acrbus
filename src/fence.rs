//! A fence around an agent's shell.
//!
//! A reviewer that can run commands works in a copy of its own, but a shell
//! reaches whatever the agent's user can: other repositories, the agent's
//! own home, the tokens and keys its model calls use. A bridge with
//! `[bridges.<agent>.fence]` fences that shell. Before its agent starts, the
//! bridge writes the run's fence, and names it in the agent's environment.
//! The agent's tool layer runs each shell command through
//! `<app> fence -c <command>`, which runs it in macOS's sandbox, with an
//! environment of its own, so that the command:
//!
//! - reads only the copy, the copy's history, its toolchains and the system;
//! - writes only the copy, and a home and temp folder of its own;
//! - reaches no network, or only this machine's loopback when allowed;
//! - gets no variable the fence doesn't set, so no token and no key.
//!
//! Nothing the agent does decides its fence: the bridge writes the file,
//! and the variables naming it are on the agent's own process.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::config::{APP_NAME, app, env_var};

/// The variable naming a run's fence file, `<PREFIX>_FENCE`.
pub const POLICY_VAR: &str = "FENCE";
/// The variable naming the program that runs the fence, `<PREFIX>_FENCE_BIN`.
pub const BIN_VAR: &str = "FENCE_BIN";
/// The fence file, in the bridge's folder for the run.
const POLICY_FILE: &str = "fence.json";
/// macOS's sandbox.
const SANDBOX: &str = "/usr/bin/sandbox-exec";

/// Set for the shell by the fence itself, so a bridge's `fence.env` can't.
const SET: [&str; 3] = ["HOME", "PATH", "TMPDIR"];
/// Kept from the agent's environment: who it runs as, and its language.
const KEPT: [&str; 5] = ["USER", "LOGNAME", "LANG", "LC_ALL", "TERM"];

/// The system folders a fenced command may read: programs, libraries, the
/// developer tools, time zones and settings that hold nothing of yours.
const SYSTEM_READS: [&str; 13] = [
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/opt",
    "/dev",
    "/private/etc",
    "/private/var/db/timezone",
    "/private/var/db/dyld",
    "/private/var/select",
    "/Library/Developer",
    "/Library/Frameworks",
    "/Library/Java",
];

/// Inside those, what holds data rather than programs: a local database, say.
const SYSTEM_DENIED: [&str; 2] = ["/opt/homebrew/var", "/usr/local/var"];

/// The system services a command may reach: who a user is, notifications,
/// and logging. Not the keychain, not the launcher of apps or other
/// programs, and not other apps' events.
const SERVICES: [&str; 6] = [
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.system.notification_center",
    "com.apple.system.logger",
    "com.apple.logd",
    "com.apple.diagnosticd",
];

/// What the network a fenced command may reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    /// None at all, the default.
    #[default]
    None,
    /// This machine's loopback alone, for tests that start a local server.
    Loopback,
}

/// What a bridge's `[bridges.<agent>.fence]` adds to each run's fence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// More folders it may read, such as a toolchain or a package cache
    /// under your home.
    #[serde(default)]
    pub read: Vec<PathBuf>,
    /// More folders it may write.
    #[serde(default)]
    pub write: Vec<PathBuf>,
    #[serde(default)]
    pub network: Network,
    /// Variables it gets, besides those the fence sets. Never a secret.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// One run's fence, which its bridge writes before the agent starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The run's working directory: the copy, the one place of the
    /// checkout's it may write.
    pub copy: PathBuf,
    /// The agent's PATH, which the command gets too.
    pub path: String,
    /// Its own HOME and TMPDIR, kept with the run, under here.
    pub state: PathBuf,
    #[serde(default)]
    pub fence: Settings,
}

/// What's wrong with a bridge's fence settings, given the names of its
/// secrets, which a fenced command never gets.
pub fn check_settings<'a>(
    fence: &Settings,
    mut secrets: impl Iterator<Item = &'a String>,
    plain_name: impl Fn(&str) -> bool,
) -> Result<()> {
    for p in fence.read.iter().chain(&fence.write) {
        if !p.is_absolute() {
            bail!(
                "fence.read and fence.write take absolute paths, not {}",
                p.display()
            );
        }
        if p.components().all(|c| matches!(c, Component::RootDir)) {
            bail!("fence.read and fence.write can't be /, which would undo the fence");
        }
    }
    for name in fence.env.keys() {
        if SET.contains(&name.as_str()) {
            bail!("fence.env can't set {name}: the fence sets it");
        }
        if !plain_name(name) {
            bail!("fence.env: {name:?} isn't an environment variable's name");
        }
    }
    if let Some(name) = secrets.find(|s| fence.env.contains_key(*s)) {
        bail!("fence.env can't pass the secret {name}: a fenced command never gets a secret");
    }
    Ok(())
}

/// Writes a run's fence into its folder, where only the bridge writes, and
/// returns the file to name in the agent's environment.
pub fn write_policy(run_dir: &Path, copy: &Path, path: &str, fence: &Settings) -> Result<PathBuf> {
    let policy = Policy {
        copy: copy.to_path_buf(),
        path: path.to_owned(),
        state: run_dir.join("tmp").join("fence"),
        fence: fence.clone(),
    };
    let file = run_dir.join(POLICY_FILE);
    let mut out = crate::bridge::create_private(&file)
        .with_context(|| format!("writing {}", file.display()))?;
    out.write_all(serde_json::to_string_pretty(&policy)?.as_bytes())?;
    Ok(file)
}

/// The run's fence, from the file its bridge named. Without one, nothing
/// runs: a command meant to be fenced never runs unfenced.
fn load(named: Option<String>) -> Result<Policy> {
    let Some(file) = named else {
        bail!(
            "no fence: `{APP_NAME} fence` runs a command only for an agent whose bridge has \
             [bridges.<agent>.fence], and runs nothing without one"
        );
    };
    let text = fs::read_to_string(&file).with_context(|| format!("reading the fence {file}"))?;
    serde_json::from_str(&text).with_context(|| format!("reading the fence {file}"))
}

/// Runs a command inside the fence its bridge wrote: `-c <command>` through
/// /bin/sh, or a program and its arguments. Returns only if it can't start.
pub fn run(command: Option<String>, program: Vec<OsString>) -> Result<std::process::ExitCode> {
    let (prog, args) = match (command, program.split_first()) {
        (Some(c), None) => (OsString::from("/bin/sh"), vec!["-c".into(), c.into()]),
        (None, Some((p, rest))) => (p.clone(), rest.to_vec()),
        _ => bail!("give one of `-c <command>` and `-- <program> [arguments]`"),
    };
    let policy = load(env_var(POLICY_VAR))?;
    let mut fenced = command_in(&policy, &prog, &args)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt as _;
        let err = fenced.exec();
        Err(err).context("starting macOS's sandbox")
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &mut fenced;
        bail!(
            "the fence needs macOS's sandbox, and there's none on {}, so it runs nothing",
            std::env::consts::OS
        )
    }
}

/// The command, fenced: the sandbox with the run's profile, and an
/// environment holding only what the fence sets.
fn command_in(policy: &Policy, prog: &OsString, args: &[OsString]) -> Result<Command> {
    let (home, tmp) = (policy.state.join("home"), policy.state.join("tmp"));
    for dir in [&home, &tmp] {
        fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    }
    let mut command = Command::new(SANDBOX);
    command
        .arg("-p")
        .arg(profile(policy))
        .arg(prog)
        .args(args)
        .env_clear();
    for name in KEPT {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .envs(&policy.fence.env)
        .env("PATH", &policy.path)
        .env("HOME", &home)
        .env("TMPDIR", &tmp);
    Ok(command)
}

/// The sandbox profile for a run: everything denied, then what it needs.
/// The last rule that matches wins, so each list's denials come after what
/// they narrow, and the run's own folders come last.
fn profile(policy: &Policy) -> String {
    let mut reads: Vec<PathBuf> = vec![policy.copy.clone(), policy.state.clone()];
    reads.extend(alternates(&policy.copy));
    reads.extend(toolchains(&policy.path));
    reads.extend(policy.fence.read.iter().cloned());
    let mut writes: Vec<PathBuf> = vec![policy.copy.clone(), policy.state.clone()];
    writes.extend(policy.fence.write.iter().cloned());
    let subpaths = |paths: &[PathBuf]| -> String {
        let mut seen: Vec<String> = Vec::new();
        for p in paths {
            let q = quoted(&real(p));
            if !seen.contains(&q) {
                seen.push(q);
            }
        }
        seen.iter()
            .map(|q| format!(" (subpath {q})"))
            .collect::<String>()
    };
    let system: Vec<PathBuf> = SYSTEM_READS.iter().map(PathBuf::from).collect();
    let denied: Vec<PathBuf> = SYSTEM_DENIED.iter().map(PathBuf::from).collect();
    let services: String = SERVICES
        .iter()
        .map(|s| format!(" (global-name \"{s}\")"))
        .collect();
    let network = match policy.fence.network {
        Network::None => String::new(),
        // Out to loopback only; a server bound to loopback, and what
        // reaches it. A rule on the local address alone would let every
        // connection out.
        Network::Loopback => "(allow network-outbound (remote ip \"localhost:*\"))\n\
                              (allow network-bind (local ip \"localhost:*\"))\n\
                              (allow network-inbound (local ip \"localhost:*\"))\n"
            .to_owned(),
    };
    format!(
        r#"(version 1)
(deny default)
(allow process-fork)
(allow process-exec*)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-map-executable)
(allow pseudo-tty)
(allow file-ioctl (regex #"^/dev/"))
(allow ipc-posix*)
(allow mach-lookup{services})
(allow file-read* (literal "/"){system})
(allow file-read* (regex #"^/Applications/Xcode[^/]*\.app/") (literal "/private/var/db/xcode_select_link") (literal "/Library/Preferences/com.apple.dt.Xcode.plist"))
(allow file-read* file-write* (regex #"^/private/var/folders/[^/]+/[^/]+/T/xcrun_db"))
(deny file-read*{denied})
(allow file-read*{reads})
(allow file-write*{writes} (literal "/dev/null") (literal "/dev/zero") (literal "/dev/tty") (literal "/dev/dtracehelper") (regex #"^/dev/fd/") (regex #"^/dev/ttys[0-9]+$"))
{network}"#,
        system = subpaths(&system),
        denied = subpaths(&denied),
        reads = subpaths(&reads),
        writes = subpaths(&writes),
    )
}

/// A path as the sandbox sees it, with its links followed, such as
/// /var/folders as /private/var/folders. One that doesn't exist yet stays
/// as it is.
fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn quoted(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The object stores a copy reads its history from: git's alternates, and
/// theirs in turn.
fn alternates(copy: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut objects = vec![copy.join(".git").join("objects")];
    while let Some(dir) = objects.pop() {
        let Ok(text) = fs::read_to_string(dir.join("info").join("alternates")) else {
            continue;
        };
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let alt = real(&dir.join(line));
            if !found.contains(&alt) && found.len() < 16 {
                found.push(alt.clone());
                objects.push(alt);
            }
        }
    }
    found
}

/// The toolchains on the PATH: each folder on it, and for a `bin` folder,
/// the toolchain around it, such as a Go or Rust install that reads its own
/// libraries. Never a home folder itself or a folder straight inside it,
/// which hold far more than a toolchain: one there goes in `fence.read`.
fn toolchains(path: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in path.split(':').filter(|e| e.starts_with('/')) {
        let dir = real(Path::new(entry));
        if SYSTEM_READS.iter().any(|s| dir.starts_with(s)) {
            continue;
        }
        if dir.file_name().is_some_and(|n| n == "bin")
            && let Some(around) = dir.parent()
            && deep_enough(around)
        {
            found.push(around.to_path_buf());
        } else if deep_enough(&dir) || dir.file_name().is_some_and(|n| n == "bin") {
            found.push(dir);
        }
    }
    found
}

/// Whether a folder is far enough below a home folder, or outside every
/// home, to be a toolchain's rather than the home's own.
fn deep_enough(dir: &Path) -> bool {
    let parts: Vec<_> = dir.components().collect();
    match parts.as_slice() {
        [
            Component::RootDir,
            Component::Normal(users),
            _name,
            rest @ ..,
        ] if users.to_str() == Some("Users") => rest.len() >= 2,
        [Component::RootDir, Component::Normal(users), ..] if users.to_str() == Some("Users") => {
            false
        }
        [Component::RootDir] => false,
        _ => true,
    }
}

/// Checks the fence holds on this machine, with a bridge's settings and
/// PATH, in `scratch`, a folder under your home that nothing else uses.
/// Returns what's wrong: empty if it holds.
pub fn check(fence: &Settings, path: &str, scratch: &Path) -> Result<Vec<String>> {
    let found = check_in(fence, path, scratch);
    let _ = fs::remove_dir_all(scratch);
    found
}

fn check_in(fence: &Settings, path: &str, scratch: &Path) -> Result<Vec<String>> {
    if !cfg!(target_os = "macos") {
        return Ok(vec![format!(
            "there's no fence on {}: it needs macOS's sandbox, and runs nothing without it",
            std::env::consts::OS
        )]);
    }
    fs::create_dir_all(scratch)?;
    let origin = scratch.join("origin");
    fs::create_dir_all(&origin)?;
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git")
            .args(["-c", "user.name=fence", "-c", "user.email=fence@localhost"])
            .args(args)
            .current_dir(&origin)
            .output()
            .context("running git")?;
        if !out.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    git(&["init", "-q"])?;
    fs::write(origin.join("code.txt"), "code\n")?;
    git(&["add", "code.txt"])?;
    git(&["commit", "-qm", "the change"])?;
    let head = git(&["rev-parse", "HEAD"])?;
    let copy = scratch.join("copy");
    crate::acr::change::copy_of(&origin, &copy)?;
    let secret = format!("fence-canary-{}", crate::util::new_id());
    let canary = scratch.join("canary.txt");
    fs::write(&canary, format!("{secret}\n"))?;
    let outside = scratch.join("outside.txt");
    let policy = Policy {
        copy: copy.clone(),
        path: path.to_owned(),
        state: scratch.join("state"),
        fence: fence.clone(),
    };
    let shell = |c: &str| -> Result<std::process::Output> {
        let mut command = command_in(&policy, &"/bin/sh".into(), &["-c".into(), c.into()])?;
        command
            .current_dir(&copy)
            .stdin(std::process::Stdio::null())
            .output()
            .context("running a fenced command")
    };
    let said = |out: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .trim()
        .to_owned()
    };
    let mut wrong = Vec::new();

    let out = shell("git rev-parse HEAD && git log --format=%s -1")?;
    if !String::from_utf8_lossy(&out.stdout).contains(&head) {
        wrong.push(format!(
            "a fenced command couldn't run git on its copy and the copy's history: {}",
            said(&out)
        ));
    }
    let out = shell("echo made > made.txt")?;
    if !copy.join("made.txt").exists() {
        wrong.push(format!(
            "a fenced command couldn't write in its copy: {}",
            said(&out)
        ));
    }
    let out = shell(&format!("cat '{}'", canary.display()))?;
    if said(&out).contains(&secret) {
        wrong.push("a fenced command read a file outside its copy".into());
    }
    shell(&format!("echo leaked > '{}'", outside.display()))?;
    if outside.exists() {
        wrong.push("a fenced command wrote a file outside its copy".into());
    }
    let out = shell("/usr/bin/env")?;
    let allowed: Vec<&str> = SET
        .iter()
        .chain(&KEPT)
        .copied()
        .chain(fence.env.keys().map(String::as_str))
        .chain(["PWD", "SHLVL", "OLDPWD", "_"])
        .collect();
    let extra: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('=').map(|(n, _)| n.to_owned()))
        .filter(|n| !allowed.contains(&n.as_str()))
        .collect();
    if !extra.is_empty() {
        wrong.push(format!(
            "a fenced command got variables the fence doesn't set: {}",
            extra.join(", ")
        ));
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    shell(&format!(
        "/usr/bin/curl -s -m 2 -o /dev/null http://127.0.0.1:{port}/"
    ))?;
    let reached = listener.accept().is_ok();
    match (fence.network, reached) {
        (Network::None, true) => {
            wrong.push("a fenced command reached this machine's loopback".into());
        }
        (Network::Loopback, false) => wrong.push(
            "a fenced command couldn't reach this machine's loopback, which fence.network allows"
                .into(),
        ),
        _ => {}
    }
    let out = shell("/usr/bin/curl -s -m 3 -o /dev/null -w '%{http_code}' http://1.1.1.1/")?;
    if out.status.success() {
        wrong.push("a fenced command reached the network".into());
    }
    Ok(wrong)
}

/// For `<app> fence --check <bridge>`: whether its fence holds here.
pub fn check_bridge(paths: &crate::config::Paths, agent: &str) -> Result<bool> {
    let settings = crate::bridge::load(paths, agent)?;
    let Some(fence) = &settings.fence else {
        println!(
            "The {agent} bridge has no fence: [bridges.{agent}.fence] in its config fences the \
             shell its agent runs."
        );
        return Ok(false);
    };
    let scratch = paths
        .state
        .join(format!("fence-check-{}", crate::util::new_id()));
    let wrong = check(fence, &settings.path, &scratch)?;
    if wrong.is_empty() {
        println!(
            "The {agent} bridge's fence holds here: a fenced command reads and writes its copy, \
             runs git on its history, and reads nothing outside it, writes nothing outside it, \
             gets no variable the fence doesn't set, and reaches {}.",
            match fence.network {
                Network::None => "no network",
                Network::Loopback => "this machine's loopback and nothing else",
            }
        );
        println!(
            "Its agent is fenced only if its tool layer runs each shell command through \
             `\"${}_{BIN_VAR}\" fence -c <command>`. `{APP_NAME} acr check` shows whether it does.",
            app().env[0]
        );
        return Ok(true);
    }
    println!("The {agent} bridge's fence doesn't hold here:");
    for w in wrong {
        println!("  {w}");
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own for a test, removed when it's done.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let dir = std::env::temp_dir().join(format!("tb-fence-{}", crate::util::new_id()));
            fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn plain(name: &str) -> bool {
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }

    fn policy(copy: &Path, fence: Settings) -> Policy {
        Policy {
            copy: copy.to_path_buf(),
            path: "/usr/bin:/bin".into(),
            state: copy.join("..").join("state"),
            fence,
        }
    }

    #[test]
    fn a_fence_takes_absolute_folders_and_never_a_secret() {
        let secrets = ["GATEWAY_KEY".to_string()];
        let ok = Settings {
            read: vec!["/opt/go".into()],
            env: [("GOFLAGS".to_string(), "-mod=mod".to_string())].into(),
            ..Settings::default()
        };
        check_settings(&ok, secrets.iter(), plain).unwrap();
        let relative = Settings {
            read: vec!["go".into()],
            ..Settings::default()
        };
        let e = check_settings(&relative, secrets.iter(), plain).unwrap_err();
        assert!(e.to_string().contains("absolute"), "{e}");
        let root = Settings {
            write: vec!["/".into()],
            ..Settings::default()
        };
        assert!(check_settings(&root, secrets.iter(), plain).is_err());
        let home = Settings {
            env: [("HOME".to_string(), "/x".to_string())].into(),
            ..Settings::default()
        };
        let e = check_settings(&home, secrets.iter(), plain).unwrap_err();
        assert!(e.to_string().contains("the fence sets it"), "{e}");
        let secret = Settings {
            env: [("GATEWAY_KEY".to_string(), "x".to_string())].into(),
            ..Settings::default()
        };
        let e = check_settings(&secret, secrets.iter(), plain).unwrap_err();
        assert!(e.to_string().contains("never gets a secret"), "{e}");
    }

    #[test]
    fn a_fence_reads_its_copy_and_history_and_writes_only_its_copy() {
        let dir = Scratch::new();
        let copy = dir.path().join("copy");
        let history = dir.path().join("checkout").join(".git").join("objects");
        fs::create_dir_all(copy.join(".git/objects/info")).unwrap();
        fs::create_dir_all(&history).unwrap();
        fs::write(
            copy.join(".git/objects/info/alternates"),
            format!("{}\n", history.display()),
        )
        .unwrap();
        let p = profile(&policy(&copy, Settings::default()));
        let (copy, history) = (real(&copy), real(&history));
        let reads = p
            .lines()
            .find(|l| l.starts_with("(allow file-read* (subpath"))
            .unwrap();
        assert!(reads.contains(&quoted(&copy)), "{reads}");
        assert!(reads.contains(&quoted(&history)), "{reads}");
        let writes = p
            .lines()
            .find(|l| l.starts_with("(allow file-write*"))
            .unwrap();
        assert!(writes.contains(&quoted(&copy)), "{writes}");
        assert!(!writes.contains(&quoted(&history)), "{writes}");
        assert!(p.contains("(deny default)"));
        assert!(!p.contains("network"), "no network by default:\n{p}");
    }

    #[test]
    fn loopback_is_let_through_only_when_the_bridge_says() {
        let dir = Scratch::new();
        let loopback = Settings {
            network: Network::Loopback,
            ..Settings::default()
        };
        let p = profile(&policy(dir.path(), loopback));
        assert!(p.contains("(remote ip \"localhost:*\")"), "{p}");
        assert!(!p.contains("(remote ip \"*:*\")"), "{p}");
    }

    #[test]
    fn a_toolchain_is_read_whole_but_never_a_home_or_whats_straight_inside_it() {
        let found = toolchains(
            "/usr/bin:/Users/u/.rustup/toolchains/stable/bin:/Users/u/.cargo/bin:/Users/u/.local/bin:/Users/u/bin:/Users/u/sdk/go1.23/bin:relative",
        );
        let found: Vec<String> = found.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            found,
            [
                "/Users/u/.rustup/toolchains/stable",
                "/Users/u/.cargo/bin",
                "/Users/u/.local/bin",
                "/Users/u/bin",
                "/Users/u/sdk/go1.23",
            ]
        );
    }

    #[test]
    fn without_a_fence_nothing_runs() {
        let e = load(None).unwrap_err();
        assert!(e.to_string().contains("runs nothing without one"), "{e}");
        let e = run(None, Vec::new()).unwrap_err();
        assert!(e.to_string().contains("give one of"), "{e}");
    }

    #[test]
    fn a_bridge_writes_a_runs_fence_only_it_can_change() {
        let dir = Scratch::new();
        let file = write_policy(
            dir.path(),
            Path::new("/copy"),
            "/usr/bin",
            &Settings::default(),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let p = load(Some(file.display().to_string())).unwrap();
        assert_eq!(p.copy, Path::new("/copy"));
        assert_eq!(p.state, dir.path().join("tmp").join("fence"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_fence_holds_on_this_machine() {
        let dir = Scratch::new();
        let wrong = check(
            &Settings::default(),
            "/usr/bin:/bin:/usr/sbin:/sbin",
            &dir.path().join("check"),
        )
        .unwrap();
        assert!(wrong.is_empty(), "{wrong:#?}");
        let loopback = Settings {
            network: Network::Loopback,
            ..Settings::default()
        };
        let wrong = check(&loopback, "/usr/bin:/bin", &dir.path().join("loop")).unwrap();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
