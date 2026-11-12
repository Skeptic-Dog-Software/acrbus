# acrbus

A harness for adversarial convergence review (ACR) of code changes.

Point a review at a change, and acrbus runs it: fresh reviewers attack it one
dimension at a time, a Judge rules on what they find, the blockers are
fixed, and fresh reviewers attack again, until a round finds nothing that
blocks. The reviewers are the agents you already use, such as Claude Code,
pi or omp, run headless through bridges. acrbus is everything around them
that makes their review adversarial, rule-bound and repeatable.

It runs on one machine. acrbus itself needs no network: the bus is a local
socket, and the models are reached only through the agents you bridge.
[tailbus](https://github.com/Skeptic-Dog-Software/tailbus), built on it,
carries the same bus between machines on a Tailscale network.

## Why a harness, not a model call

You can review a change by handing the diff to a coding agent and asking
what's wrong. That gets you one model's reading, in one context, judged by
the same model that found it, once. The weaknesses are structural, so
acrbus fixes them in structure, not in a prompt.

**Every role starts clean.** Each lens, one dimension of review such as
correctness or regression safety, gets reviewers of its own, in fresh
sessions that see the change and that lens only. They don't inherit the
reasoning that wrote the code, or each other's conclusions. What the change
says about itself, its description, commit messages and comments, reaches
them as claims to check, not to take on trust.

**What blocks is decided by rules, not by tone.** Reviewers find; a separate
Judge checks each finding against the code and states facts about it. Is it
a real defect? Was it there before the change? Does the change claim to fix
it, or make it worse? Does a new test pin nothing? Fixed rules turn those
facts into a ruling, and a fact that rests on the change's own words holds
only when the Judge quotes them from the diff. Each lens can say what blocks
in its dimension, and a finding blocks when the Judge shows it meets that.

**It's configured for your repository, and learns from you.** A strong
model reviews the whole repository, what it's built of, how it's tested and
shipped, and which defects its history keeps fixing, and proposes the
dimensions that matter there and what blocks in each. You read and accept
it, and `acrbus acr run` refuses a repository until you have. Then you say
how its reviews did, finding by finding, and what they missed, and
recalibrating folds that back into the config, so the next review gets it
right.

**It gathers its own evidence.** With its stages on, the harness runs the
change's tests itself before any reviewer starts: coverage of the changed
lines, mutants of the changed code that every test should catch, and the
tests again in random order, one at a time and in another time zone.
Reviewers that run things get a copy of the repository of their own, and
probes write and run scenarios of what the change says it does. Nobody has
to take a model's word that it ran the tests.

**It converges.** After a round with blockers, the Orchestrator fixes them,
if its binding may write, or hands them to you, and new reviewers attack
again, until every lens's latest round has no blocker. A review to
convergence goes further: after each fix every lens attacks again, since a
fix for one can break another, and it ends only when every lens agrees on
the same version. Gates hand a person the calls that are theirs.

**It's sized to the change.** A tier, chosen from what the change touches,
decides which lenses run and how many reviewers each gets, so a docs fix
doesn't pay for a security review. `acrbus acr plan` shows the lenses and an
estimate before anything runs, and every model call is counted.

**It survives its agents.** Each headless run gets an environment of its
own, a time limit, and a watch for going quiet: a run whose process tree
stops using CPU and writing is woken, and if it can't be, ended and tried
again in a fresh session. A failed reviewer or Judge is never taken for a
clean pass.

**It keeps reviewers in their lane.** Reviewers read the code under review,
not your other files, and nothing they do is written back to the
repository.

**It's measurable.** `acrbus acr trial` reruns a review design blind on a
change whose defects are already known and scores what it catches, so a
change to the harness, a lens or a model choice is a measurement, not a
guess.

pi, omp, Claude Code or a model's API remain what does the reviewing. Each
role, reviewer, Judge or Orchestrator, can use a different one, and a
binding says which.

## Install

```sh
cargo install --locked --git https://github.com/Skeptic-Dog-Software/acrbus --root ~/.local
```

Say who the bus belongs to, in `~/.config/acrbus/config.toml`:

```toml
# Who every message here comes from. Each bridge's `owner` must match it.
owner = "you@example.com"
```

Then set it up:

```sh
acrbus setup all
```

That runs the daemon at login, installs the Claude Code plugin, and sets up
ACR's agent: headless Claude Code, with its token, its bindings and its
service. Each part is also its own command: `acrbus service install`,
`acrbus setup claude` and `acrbus setup acr`.

## Review a change

[The quickstart](docs/quickstart.md) walks through it, for you or your
agent, step by step, with what to check at each. In short:

1. Allow the repository: `acrbus acr add-repo <path>`.
2. Configure it: `acrbus acr configure --repo <path>`. The configurer reviews
   the whole repository and proposes its review config, at
   `~/.config/acrbus/acr/<repository>.proposed.yaml`, with its reasons.
3. Read it, change what you like, and accept it:
   `acrbus acr calibration --repo <path> --accept`.
4. Review: `acrbus acr run --repo <path> --base <ref>`.
5. Say how it did, `acrbus acr feedback <review id>`, and, when you're
   ready, fold that in: `acrbus acr calibrate --repo <path>`.

[Setting up ACR review for a repository](docs/acr-repo-setup.md) describes
the config, for reading a proposal or writing one yourself, and
[examples/acr](examples/acr) has configs.

| Command | Does |
| --- | --- |
| `acrbus acr doctor` | Check this machine and a repository are ready for review, and say what to fix. |
| `acrbus acr validate`, `acrbus acr schema` | Check a repository's config, or print its schema. |
| `acrbus acr plan` | Show the tier, lenses, bindings and estimated cost, with no model calls. |
| `acrbus acr configure` | Have the configurer review the whole repository and propose its review config. |
| `acrbus acr calibration --accept` | Accept the proposed config, or your own: that calibrates the repository. |
| `acrbus acr feedback <review id>` | List a finished review's findings, and say how each did and what it missed. |
| `acrbus acr calibrate` | Fold your feedback into the config: the configurer proposes a revised one. |
| `acrbus acr run` | Review a change. |
| `acrbus acr resume <run id>` | Go on past a gate that's waiting for a person. |
| `acrbus acr carry` | Carry work that isn't code, such as a document, into git so it can be reviewed. |
| `acrbus acr check`, `acrbus acr trial` | Check that a trial can run, then measure a review design on a change whose defects are known. |
| `acrbus acr judge-check <key>...` | Compare the Judge's rulings with a past review's, as a benchmark. |

The method itself is in [the ACR skill](plugin/skills/acr/SKILL.md), and the
config's schema in [schema/acr.schema.json](schema/acr.schema.json).

### Bindings

`~/.config/acrbus/acr.yaml` names the models a review may use, as bindings,
and which role uses which. A binding is a bridge agent with a model, an
effort and a set of tools:

```yaml
version: 1
allowed_bindings:
  reviewer-runs:   # reviewers that run tests, in a copy of their own
    agent: claude-acr@laptop
    model: claude-sonnet-5-5
    effort: high
    tools: [Read, Grep, Glob, Bash, Write, Edit]
  judge:
    agent: claude-acr@laptop
    model: claude-opus-5-5
    effort: medium
    tools: [Read, Grep, Glob, Bash, Write]
  configurer:      # reviews a whole repository to configure its review
    agent: claude-acr@laptop
    model: claude-opus-5-5
    effort: max
    tools: [Read, Grep, Glob, Bash, Write]
defaults:
  bindings:
    reviewer: reviewer-runs
    judge: judge
    configurer: configurer
```

`acrbus setup acr` writes one for headless Claude Code. A bridge to pi, omp
or any other agent CLI works the same way. Without a `configurer`, the
Judge's binding configures.

Calls have no time budget: a call runs until it answers, and its bridge
ends one that stalls (`idle_secs`). A call asks its bridge for as long as
the bridge allows, so a binding's `timeout`, if set, is only a cap no call
should reach. While a review runs, it says on stderr, and in
`progress.log` in its folder, what each call is doing, at least once a
minute. A repository's review config can set `allowed_models`, patterns
such as `vendor/*`: every binding its reviews would call must name a model
one matches, or the config is refused. Set in `defaults`, it holds every
repository that doesn't set its own.

## Bridges

A bridge answers its agent's messages by running a headless agent CLI once
per message and sending back what it prints. Configure one in
`config.toml` and start it with `acrbus bridge install <agent>`:

```toml
[bridges.coder]
owner = "you@example.com"
command = "/usr/local/bin/your-agent"
args = ["--print", "--session-dir", "{SESSION_DIR}"]
working_directory = "/Users/you/agent-work"
home = "/Users/you/agent-home"
path = "/usr/local/bin:/usr/bin:/bin"
```

| Setting | Means |
| --- | --- |
| `owner` | Who a message must come from: the bus's `owner`. Required. |
| `command`, `args` | What to run. `args` can use the placeholders below. |
| `working_directory` | Where it runs. Required. |
| `home`, `path` | Its `HOME` and `PATH`. Required. `home` can't be your own. |
| `session` | `"fresh"`, the default, gives every request a new session. `"per-sender"` keeps one conversation with each sender. |
| `max_concurrent` | The most runs at once. Default 1. |
| `timeout_secs` | How long a run can take. Default 600. A review's call asks for as long as the bridge allows, `max_timeout_secs` or this, so give a review's bridge hours: `acrbus setup acr` writes 14400. |
| `idle_secs` | How long a run can go quiet before its process tree is woken, and, if nothing stirs, ended and tried again in a fresh run, up to 3 tries in all. Default 1200; 0 turns it off. Either way, `acrbus bridge status` shows when each running request last used CPU or changed a file, which tells a slow run from a stalled one. |
| `env`, `secrets` | More environment, and variables read from files only you can read. |
| `attachments` | `"path"`, the default, tells the agent where each attachment is. `"inline"` puts each one's text in its message, for an agent that can read only its working directory, since attachments are kept outside it. |

| Placeholder | Is |
| --- | --- |
| `{AGENT}`, `{ID}`, `{SENDER}` | The bridge's agent, the message's ID, and who sent it. |
| `{SESSION_DIR}` | The run's session directory, which keeps sessions apart. |
| `{RUN_DIR}`, `{MESSAGE_FILE}` | A directory for this run alone, and the file holding the message. |
| `{MODEL}`, `{TOOLS}`, `{EFFORT}` | What the request asked for, where `options` allows it. |

A bridge isn't a sandbox: the agent runs as you, with what its own settings
allow.

## The bus

Underneath, a small bus on this machine carries messages between agents:
the review's roles and their bridges, and your own sessions. An agent's
address is `name@machine`. A Claude Code session with the plugin is an
agent, named after its repository, and messages reach it as it works. From
a shell:

| Command | Does |
| --- | --- |
| `acrbus send <to> "text"` | Send a request. `--reply-to <id>` answers one, `--no-reply` asks for nothing back, and `--progress` sends a progress note on a request you're still working on. |
| `acrbus call <to> "text"` | Send a request, wait for the reply, print it, and exit with its status. |
| `acrbus recv`, `acrbus inbox`, `acrbus ack <id>` | Take your next messages, list what's waiting, or mark one handled. |
| `acrbus agent add <name>`, `acrbus peers` | Subscribe an agent before its first session, and list who you can reach. |
| `acrbus status`, `acrbus tail` | The daemon's state, and its log as it happens. |

Messages are untrusted: an agent gets each one tagged with who sent it, and
is told a request never overrides its user's instructions.

| Variable | Means |
| --- | --- |
| `ACRBUS_HOME` | Keep everything, state and config, in this one folder. |
| `ACRBUS_AGENT` | The agent a session or command acts as, instead of one named after the repository. |
| `ACRBUS_CLIENT` | Marks a session as a worker for a client, for `/pipe`. |

## As a library

A program can build on acrbus: it names itself, adds networks such as one
that reaches other machines, and runs the shared command line.
[tailbus](https://github.com/Skeptic-Dog-Software/tailbus) is one: acrbus
with a Tailscale network, in two files.

```rust
use acrbus::config::App;

const MINE: App = App {
    name: "mybus",
    label: "com.example.mybus",
    about: "My agent bus",
    network: "local",
    networks: &[],
    env: &["MYBUS"],
};

fn main() -> std::process::ExitCode {
    acrbus::config::set_app(&MINE);
    acrbus::cli::main()
}
```

A network is an `acrbus::network::Kind`: its name for `network` in
`config.toml`, the settings it reads there, and how to open it as an
`acrbus::network::Network`.

## History

acrbus has been in development since January 2026. Its git history was
overwritten before it was made open source, to remove sensitive
information, so this repository's history starts at that point.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

## Develop

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
