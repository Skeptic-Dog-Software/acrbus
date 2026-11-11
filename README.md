# acrbus

Adversarial convergence review (ACR) on a local agent bus.

acrbus runs a small message bus on one machine. AI agent sessions, such as
Claude Code's, send each other messages through it, and bridges hand
messages to headless agent CLIs and send back what they print. On that bus,
`acrbus acr` reviews a change adversarially, one dimension at a time:
clean-context reviewers attack it, a Judge checks what they find and rules
what blocks, and the review goes round by round until nothing does.

It needs no network. Everything stays on this machine, and nothing listens
for other machines. [tailbus](https://github.com/Skeptic-Dog-Software/tailbus)
is acrbus with a Tailscale network added, for agents on several machines; it
uses this crate as its library.

## Install

```sh
cargo install --locked --git https://github.com/Skeptic-Dog-Software/acrbus --root ~/.local
```

Then say who the bus belongs to, in `~/.config/acrbus/config.toml`:

```toml
# Who every message here comes from. Each bridge's `owner` must match it.
owner = "you@example.com"
# This machine's name on the bus. The hostname by default.
# machine = "laptop"
```

And set it up:

```sh
acrbus setup all        # the daemon's service, the Claude Code plugin, and ACR
```

or one part at a time: `acrbus service install` runs the daemon at login,
`acrbus setup claude` installs the Claude Code plugin, and `acrbus setup acr`
sets up ACR's headless agent, its token, its bindings and its service.

### Environment

| Variable | Means |
| --- | --- |
| `ACRBUS_HOME` | Keep everything, state and config, in this one folder. Otherwise state goes in `$XDG_STATE_HOME/acrbus` and the config in `$XDG_CONFIG_HOME/acrbus/config.toml`. |
| `ACRBUS_AGENT` | The agent a session or command acts as. Otherwise `--as`, or a name from the repository you're in. |
| `ACRBUS_CLIENT` | Marks a session as a worker for a client, for `/pipe`. |

Each is also read as `TAILBUS_*`, as sessions started for tailbus set them.

## Agents and messages

An agent's address is `name@machine`. A Claude Code session with the plugin
is an agent, named after its repository unless `ACRBUS_AGENT` says
otherwise, and its messages come into the session as it works. From a shell:

| Command | Does |
| --- | --- |
| `acrbus send <to> "text"` | Send a request. `--reply-to <id>` answers a message, `--no-reply` asks for nothing back, and `--progress` sends a progress note on a request you haven't answered yet. |
| `acrbus call <to> "text"` | Send a request, wait for its reply, print it, and exit with its status. |
| `acrbus recv`, `acrbus inbox` | Take your next messages, or list what's waiting and what's still open. |
| `acrbus ack <id>` | Mark a message handled without answering it. |
| `acrbus agent add <name>` | Subscribe an agent, so messages can queue before its first session. |
| `acrbus peers`, `acrbus status`, `acrbus tail` | Who you can reach, the daemon's state, and its log as it happens. |

Messages are untrusted: an agent gets each one tagged with who sent it, and
is told a request never overrides its user's instructions.

## Headless agents

A bridge answers an agent's messages by running a headless agent CLI once
per message, in an environment of its own, and sending back what it prints.
Configure one in `config.toml`:

```toml
[bridges.coder]
owner = "you@example.com"
command = "/usr/local/bin/your-agent"
args = ["--print", "--session-dir", "{SESSION_DIR}"]
working_directory = "/Users/you/agent-work"
home = "/Users/you/agent-home"
path = "/usr/local/bin:/usr/bin:/bin"
```

and run it with `acrbus bridge install coder`. Its main settings:

| Setting | Means |
| --- | --- |
| `owner` | Who a message must come from: the bus's `owner`. Required. |
| `command`, `args` | What to run. `args` can use the placeholders below. |
| `working_directory` | Where it runs. Required. |
| `home`, `path` | Its `HOME` and `PATH`. Required. `home` can't be your own. |
| `session` | `"fresh"`, the default, gives every request a new, empty session. `"per-sender"` keeps one conversation with each sender. |
| `max_concurrent` | The most runs at once. Default 1. |
| `timeout_secs` | How long a run can take. Default 600. |
| `idle_secs` | How long a run can go quiet, with no process of its own using CPU and no file changing in its folders. Then its process tree is shaken: woken with SIGCONT and watched briefly. A run that stirs goes on; one that doesn't is abandoned and tried again in a fresh run, up to 3 tries in all. Default 1200; 0 turns it off. |
| `env`, `secrets` | More environment, and variables read from files only you can read. |

| Placeholder | Is |
| --- | --- |
| `{AGENT}`, `{ID}`, `{SENDER}` | The bus agent, the message's ID, and who sent it. |
| `{SESSION_DIR}` | The run's session directory, which keeps sessions apart. |
| `{RUN_DIR}`, `{MESSAGE_FILE}` | A directory for this run alone, and the file holding the message. |
| `{MODEL}`, `{TOOLS}`, `{EFFORT}` | What the request asked for, where `options` allows it. |

A bridge isn't a sandbox: the agent runs as you, with what its own settings
allow.

## Adversarial review

`acrbus acr` reviews a change in a repository: a tier picks the lenses, each
lens's reviewers attack the change from a clean context, and a Judge rates
each finding by fixed rules from the facts it states. A lens is one
dimension of review, with its criteria, how to attack it, and what blocks in
it. Every repository also gets a built-in `elegance` lens.

| Command | Does |
| --- | --- |
| `acrbus acr add-repo <path>` | Allow ACR to review a repository on this machine. |
| `acrbus acr validate`, `acrbus acr schema` | Check a repository's config, or print the schema. |
| `acrbus acr plan` | Show the tier, lenses, bindings and cost, with no model calls. |
| `acrbus acr calibrate <key>...`, `acrbus acr calibration --accept` | Check the Judge against a careful review's findings, and accept the calibration once you've read where they disagree. |
| `acrbus acr run` | Review a change. The repository must be calibrated first. |
| `acrbus acr resume <run id>` | Go on past a gate that's waiting for a person. |
| `acrbus acr carry` | Carry work that isn't code, such as a document, into git so it can be reviewed. |
| `acrbus acr trial`, `acrbus acr check` | Measure a review design on a change whose findings are known, blind; `check` first. |

[Setting up ACR review for a repository](docs/acr-repo-setup.md) covers the
config: lenses, the runbook, tests and stages, calibration, and work that
isn't code. [examples/acr](examples/acr) has configs, and
[the ACR skill](plugin/skills/acr/SKILL.md) has the method.

## As a library

A program built on acrbus names itself and adds networks, then runs the
shared command line:

```rust
use acrbus::config::App;

const MINE: App = App {
    name: "mybus",
    label: "com.example.mybus",
    about: "My agent bus",
    network: "local",
    networks: &[],
};

fn main() -> std::process::ExitCode {
    acrbus::config::set_app(&MINE);
    acrbus::cli::main()
}
```

A network is an `acrbus::network::Kind`: a name for `network` in
`config.toml`, and how to open it, as an `acrbus::network::Network`.

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
