# Set up ACR review for a repository

For you, or an agent working for you. Each step ends with how to tell it
worked. Whenever something's off, `acrbus acr doctor --repo <path>` checks the
machine and the repository and says what to fix, with the command for it.

You need macOS or Linux (with systemd), git, and an agent CLI for the
reviewers: Claude Code (`claude`) for the setup below, or another, such as
omp or pi ([Another agent](#another-agent)). Installing needs Rust's `cargo`.

## Where things live

| What | Where, by default | With `ACRBUS_HOME` set |
| --- | --- | --- |
| The bus's config | `~/.config/acrbus/config.toml` | `$ACRBUS_HOME/config.toml` |
| The bindings | `~/.config/acrbus/acr.yaml` | `$ACRBUS_HOME/acr.yaml` |
| Review configs and proposals | `~/.config/acrbus/acr/<name>.yaml` | `$ACRBUS_HOME/acr/<name>.yaml` |
| ACR's token | `~/.config/acrbus/claude-token` | `$ACRBUS_HOME/claude-token` |
| State: the bus, bridges, reviews | `~/.local/state/acrbus/` | `$ACRBUS_HOME/` |

`<name>` is the repository's folder name. A repository can also carry its
own config, at `.acr/config.yaml`, read from the base of each change, but a
config kept outside wins, and nothing a change does can alter that.

## 1. Install

```sh
cargo install --locked --git https://github.com/Skeptic-Dog-Software/acrbus --root ~/.local
```

- **`cargo` not found:** rustup puts it in `~/.cargo/bin`. Homebrew's rustup
  puts it in `/opt/homebrew/opt/rustup/bin`, which isn't on your PATH until
  you add it.
- **`acrbus` not found:** add `~/.local/bin` to your PATH.

Check: `acrbus --version` prints its version.

## 2. Say who the bus belongs to

Make the folder if it isn't there, and put this in
`~/.config/acrbus/config.toml`:

```toml
owner = "you@example.com"
```

Check: step 3's `service install` says the bus is "owned by" you.

## 3. Set up the machine

`acrbus setup all` does three things, which you can also do one at a time:

| Command | Does |
| --- | --- |
| `acrbus service install` | Starts the bus at login. |
| `acrbus setup claude` | Installs the Claude Code plugin, so your own sessions are on the bus. |
| `acrbus setup acr` | Sets up ACR's agent, described below. |

`setup acr`:
- adds a headless Claude Code bridge, `claude-acr`, to config.toml, with a
  home of its own and a PATH that includes the folders where it finds your
  toolchains, such as cargo, go, node and python3;
- writes `acr/confine.json`, which keeps reviewers' file tools to the code
  under review;
- writes the bindings, `acr.yaml`, which say which model each role uses;
- starts the bridge.

**The token is the one thing that may need a person.** `setup acr` uses
the token at `claude-token` in the config folder (see the table above) if
there is one, and signs in with it before relying on it. If there's none,
it runs `claude setup-token`, which needs a terminal and a browser to sign
in, then asks for the token to be pasted. An agent can't do that: ask your
user to run `acrbus setup acr` in a terminal of their own. Without a
terminal, setup stops and says how to make one.

Check: `acrbus bridge status claude-acr` says it's running.

## 4. Allow the repository

```sh
acrbus acr add-repo <path>
```

This adds the repository to the bridge's allowed working directories, and
restarts the bridge.

Check: it prints "ACR may now review <path>".

## 5. Configure it

```sh
acrbus acr configure --repo <path>
```

The configurer, Opus at max effort unless your bindings say otherwise, reads
the whole repository and its history. It tries its test commands in a copy
made the way reviewers get one, then proposes the review config:

- `acr/<name>.proposed.yaml`, the config;
- `acr/<name>.proposed.md`, why each part is there, what it ran, what it's
  unsure of, and, last, **Before the first review**: what the machine's user
  must do before a review can run as the config expects, with the commands;
- `acr/<name>.proposed.not-findings.md`, things that look wrong but are
  deliberate.

It takes 20 to 60 minutes and prints nothing until it's done. It installs
and downloads nothing itself. It also allows the folder reviewers' copies
go in, and restarts the bridge.

Check: the three files are there, and `acrbus acr calibration --repo <path>`
says a proposal waits.

## 6. Accept it

Do what "Before the first review" says. Then:

```sh
acrbus acr calibration --repo <path> --accept
```

Accepting says this config is how this repository should be reviewed. It's
your user's call: show them the proposal, and its reasons, first. Until a
config is accepted, `acrbus acr run` refuses the repository.

Check: `acrbus acr calibration --repo <path>` says "calibrated".

## 7. Check

```sh
acrbus acr doctor --repo <path>
```

It checks, in order:
- the bus answers;
- the bridge may work in the repository;
- the config resolves;
- every binding is one its bridge takes;
- the config is calibrated;
- the token signs in;
- reviewers who run things find the repository's toolchain;
- the config's test commands run in a copy, the way a review runs them;
- the tests run as the reviewers would run them.

Fix what it says until it says Ready. `--no-tests` skips running the tests.

## 8. Review a change

```sh
acrbus acr plan --repo <path> --base <base>                 # no model calls
acrbus acr run --repo <path> --base <base> --head <head>
```

- **`--base`** is where the change starts: the branch it goes into, or the
  commit before it.
- **`--head`** is its last commit, and it must be checked out, with nothing
  on top. Leave the checkout alone until the review ends. Leave `--head` out
  to review the working tree.
- **`plan`** shows the tier, the lenses and an estimate. The estimate counts
  only the text ACR sends and reads back. What agents' tools read adds more,
  often several times as much, so treat it as a floor.
- **While it runs,** it says what it's doing on stderr, at least once a
  minute: each round and stage as it starts, each call as it starts and
  ends, and every call still at work, with how long it has run and when its
  bridge last saw it use CPU or change a file. The same lines go to
  `progress.log` in the review's folder, for a review started in the
  background. A call that has gone quiet says so, and when its bridge will
  end it. Nothing cuts a call off while it works.

`run` prints the review as JSON, and on stderr what to do next. Its exit
code says how it ended:

| Exit | Means | What to do |
| --- | --- | --- |
| 0 | Every lens is confirmed. | Nothing. |
| 1 | It ended with lenses open. | Its last line says why. Fix the blockers and review again. |
| 3 | It's waiting for a person at a scope, fold or final gate. | Answer the gate, below. |
| 4 | It's waiting for a person on an escalation. | Read it, then go on or stop. |

The gates, and how to answer each with `acrbus acr resume <id>`:

| Waits for | When | Answer |
| --- | --- | --- |
| scope | before any reviewer runs, with `human_gate: ends` | `--approve` to go on, `--stop` to end it |
| fold | after a round with blockers, with `human_gate: per-round` | `--fold <ids>` to have them fixed, `--waive <ids>` to waive them, `--stop` |
| final | at the end, with `human_gate: ends` | `--approve` or `--stop` |
| escalation | whenever something must go to a person, whatever `human_gate` says | `--approve` to go on, `--stop` |

`human_gate` in the review config chooses the gates:
- **`exception`**, the default, stops only for escalations.
- **`ends`** also asks you to approve the scope before it starts and the
  result at the end.
- **`per-round`** also asks, after each round with blockers, which ones get
  fixed.

**Fixes.** Except with `per-round`, a round's blockers go straight to the
Orchestrator to fix. It fixes them only if its binding may write: `writes:
true` in `acr.yaml`, with Write or Edit in its tools. The binding `setup
acr` writes can't, so the review escalates that to you instead: approve,
and it ends with the blockers listed. Fix them, then review again.

**Findings.** Each finding has a severity, an action and a state:
- **Severity:** Critical, High, Medium, Low, or Rejected by the Judge.
- **Action:** `blocks`, `blocks-unless-waived`, `never-blocks` (logged), or
  `drop`.
- **State:**
  - `open`, a blocker with nothing done yet;
  - `folded`, to be fixed, or fixed;
  - `waived`;
  - `logged`, recorded without blocking;
  - `dropped`, rejected, with its reason kept.

## 9. Teach it

```sh
acrbus acr feedback <id>                                    # lists the findings
acrbus acr feedback <id> --finding 3 --verdict not-a-blocker --note "why"
acrbus acr feedback <id> --missed "what it missed" --lens <lens>
acrbus acr calibrate --repo <path>
```

Each verdict says how the review ruled the finding:

| Verdict | For a finding that |
| --- | --- |
| `right` | was ruled right: a blocker that should block, or a logged one that shouldn't |
| `not-a-blocker` | blocked, but is a real defect that shouldn't have |
| `should-block` | didn't block, but should have |
| `not-a-defect` | isn't a defect at all |

`calibrate` has the configurer fold your feedback into a revised proposal:
accept it as in step 6. `acrbus acr run` reminds you while there's feedback
not yet folded in.

## What runs where

- **The harness** runs the config's test commands, `tests.setup`,
  `tests.command` and `tests.coverage`, in copies of the repository. They
  run in the environment of whoever ran `acrbus acr run`: your PATH, your
  HOME, your caches.
- **Reviewers** run through their bridge, with its own HOME and PATH: its
  `home` and `path` in `config.toml`. A reviewer whose binding has a shell
  (Bash, Write or Edit) works in a copy of its own. Its file tools can't
  read outside that copy, though its shell isn't confined.

So whatever the tests need has to be there for the reviewers too: a
toolchain, a filled package cache. `acrbus acr doctor` checks both ways and
says which one fails. Nothing a review runs downloads on its own: the
bridge turns off rustup's and Homebrew's downloads, so a missing tool is for
you to install.

The lenses a review runs: the config's own, `elegance`, which every
repository gets, and one for each stage that runs as a lens, `claims`,
`probes`, and `tests` for the gate. When the gate runs, the mutation stage
is part of it.

## Another agent

A bridge runs any agent CLI headless, once per request. For an agent like
omp, in `config.toml`:

```toml
[bridges.omp]
owner = "you@example.com"
command = "/path/to/omp"
args = ["-p", "--model", "{MODEL}", "--session-dir", "{SESSION_DIR}"]
model = "<the model it uses when a request names none>"
working_directory = "<a folder of its own>"
home = "<a home of its own, not yours>"
path = "/usr/local/bin:/usr/bin:/bin"
timeout_secs = 14400
attachments = "inline"                          # see below

[bridges.omp.options]
tools = ["read", "grep", "glob", "bash", "write", "edit"]  # its own names
tools_args = ["--tools", "{TOOLS}"]
models = ["<other models a binding may name>"]
efforts = ["low", "medium", "high"]
effort_args = ["--thinking", "{EFFORT}"]
workdirs = []                                   # add-repo fills this in
```

Then, with a binding in `acr.yaml` for each role, as `agent: omp@<this
machine>`:

```sh
acrbus bridge install omp
acrbus acr add-repo <path> --bridge omp
```

Things that matter for an agent other than Claude Code:

- **Name each binding's `model`.** With `allowed_models` set, which holds a
  repository's reviews to the models it lists, a binding with no model is
  refused.
- **Probes need a shell.** They're on by default, and need reviewers with a
  shell and the writing tools, in the agent's own names. With read-only
  reviewers they're left out, and the doctor says so.
- **The agent must be able to read attachments.** Each role gets the change
  as an attached file, kept outside the agent's working directory. Claude
  Code reads it there through `--add-dir {ATTACHMENTS_DIR}`. An agent that
  can read only its working directory, as omp can, needs `attachments =
  "inline"` in its bridge, which puts each attachment's text in its
  message. Without it, every role on that agent fails to read the change.
- **Let calls run.** A review never cuts a call off while it works: the call
  asks for as long as its bridge allows, so set `timeout_secs` in hours, as
  above, and the bridge ends a call that stalls (`idle_secs`, 20 minutes
  unless set). Leave `timeout` out of the binding, or make it a cap no call
  should reach.
- **Run the doctor.** It checks every binding against the bridge, with the
  repository as the working directory.

## When it goes wrong

| What you see | What to do |
| --- | --- |
| `cargo: command not found` | Put cargo's folder on PATH (step 1). |
| setup: the token doesn't sign in | `claude setup-token` for a new one, then `acrbus setup acr`. |
| `run`: "has no review config yet" | Steps 5 and 6. |
| `run`: "isn't calibrated" or "config has changed since" | Read the config, then step 6. |
| "the … bridge would refuse every call on binding …" | Allow what it names in that bridge's `options`, or change the binding. |
| the doctor: reviewers can't run `cargo` (or `go`, `node`…) | Add the folder it names to the bridge's `path`, and restart the bridge. |
| the doctor: the bridge's crate cache lacks the crates | Run the `cargo fetch` it gives, once, with the network. |
| exit 3 or 4 | The review is waiting for you: see the gates in step 8. |
| exit 1 | Its last line says what's left: fix it and review again. |
| a role's answer "didn't work either" | Its model answered in a form even a second call couldn't read. If its agent can't read the attached change, set `attachments = "inline"` in its bridge; otherwise try a stronger binding. |
| a review seems stuck | Read its progress, on stderr or in `progress.log` in its folder: every call at work, how long it has run, and when it last did anything. A call that goes quiet for its bridge's `idle_secs` is ended and tried again by the bridge. `acrbus bridge status` shows the same. |
| a call "timed out" | It ran as long as its bridge allows. Raise the bridge's `timeout_secs` and `max_timeout_secs`, and restart it. |

[Setting up ACR review for a repository](acr-repo-setup.md) is the reference
behind all this: what each part of a config means, for reading a proposal,
changing one, or writing one by hand.
