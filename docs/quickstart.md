# Set up ACR review for a repository

For you, or an agent working for you. Each step says how to tell it worked.
Whenever something's off, `acrbus acr doctor --repo <path>` checks the
machine and the repository and says what to fix, with the command for it.

You need macOS or Linux (with systemd), git, Rust's `cargo`, and an agent
CLI for the reviewers: Claude Code (`claude`) for the setup below, or
another, such as omp or pi ([Another agent](#another-agent)).

## 1. Install

```sh
cargo install --locked --git https://github.com/Skeptic-Dog-Software/acrbus --root ~/.local
acrbus --version
```

- **`cargo` not found:** rustup puts it in `~/.cargo/bin`. Homebrew's rustup
  puts it in `/opt/homebrew/opt/rustup/bin`, which isn't on your PATH until
  you add it.
- **`acrbus` not found:** add `~/.local/bin` to your PATH.

## 2. Say who the bus belongs to

In `~/.config/acrbus/config.toml`:

```toml
owner = "you@example.com"
```

## 3. Set up the machine

```sh
acrbus setup all
```

That starts the bus at login, installs the Claude Code plugin, and sets up
ACR's agent:
- a headless Claude Code bridge, `claude-acr`, with a home of its own;
- the bindings, in `~/.config/acrbus/acr.yaml`, which say which model each
  role uses.

**This step needs a person.** Setup runs `claude setup-token`, which opens a
browser to sign in, and then asks for the token to be pasted. An agent can't
do either. Ask your user to run `acrbus setup acr` in a terminal of their
own. Setup signs in with the token before it keeps it.

Check: `acrbus bridge status claude-acr` says it's running.

## 4. Allow the repository

```sh
acrbus acr add-repo <path>
```

## 5. Configure it

```sh
acrbus acr configure --repo <path>
```

The configurer, Opus at max effort unless your bindings say otherwise, reads
the whole repository and its history. It tries its test commands in a copy
made the way reviewers get one, then proposes the review config:

- `~/.config/acrbus/acr/<name>.proposed.yaml`, the config;
- `<name>.proposed.md`, why each part is there, what it ran, and what it's
  unsure of;
- `<name>.proposed.not-findings.md`, things that look wrong but are
  deliberate.

It takes 30 to 60 minutes.

Read `<name>.proposed.md`. Its "before the first review" steps are for
whoever runs the machine, such as filling the reviewers' crate cache or
installing a tool: do them.

## 6. Accept it

```sh
acrbus acr calibration --repo <path> --accept
```

Accepting says this config is how this repository should be reviewed. It's
your user's call: show them the proposal first. Until a config is
accepted, `acrbus acr run` refuses the repository.

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
- the config's test commands run in a copy, the way a review runs them;
- the tests run as the reviewers would run them.

Fix what it says until it says Ready. `--no-tests` skips running the tests.

## 8. Review a change

```sh
acrbus acr plan --repo <path> --base main                 # no model calls
acrbus acr run --repo <path> --base main --head <commit>
```

- **Before it runs:** `plan` shows the tier, the lenses and an estimate.
- **The head:** `run` needs its `--head` checked out, with nothing on top.
  Leave the checkout alone until the review ends.
- **How it ends:** `run` exits 0 when every lens is confirmed, 1 when it
  ended with lenses open, and 4 when it's waiting for a person. To go on:
  - `acrbus acr resume <id> --approve` goes past an escalation;
  - `--fold <ids>` has its blockers fixed, if the Orchestrator's binding may
    write;
  - `--waive <ids>` waives them;
  - `--stop` ends the review.

## 9. Teach it

```sh
acrbus acr feedback <id>                                    # lists the findings
acrbus acr feedback <id> --finding 3 --verdict not-a-blocker --note "why"
acrbus acr feedback <id> --missed "what it missed" --lens <lens>
acrbus acr calibrate --repo <path>
```

- **Verdicts:** `right`, `not-a-defect`, `not-a-blocker` and `should-block`.
- **Calibrate:** it has the configurer fold your feedback into a revised
  proposal. Accept it as in step 6.
- **Reminders:** `acrbus acr run` reminds you while there's feedback not yet
  folded in.

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
toolchain, a filled package cache. `acrbus acr doctor` runs the tests both
ways and says which one fails.

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
- **Give slow models a fallback.** A binding's `timeout` is its role's time
  budget. With `fallback: <another binding>`, a call that runs out of time
  is tried once more there.
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
| the doctor: tests pass for you but not as reviewers | Fill what they need in the bridge's home, or set it in the commands. |
| "waiting for you" | `acrbus acr resume <id>` with `--approve`, `--fold`, `--waive` or `--stop` (step 8). |
| a role's answer "didn't work either" | Its model answered in a form even a second call couldn't read. If its agent can't read the attached change, set `attachments = "inline"` in its bridge; otherwise try a stronger binding, or a `fallback`. |

[Setting up ACR review for a repository](acr-repo-setup.md) is the reference
behind all this: what each part of a config means, for reading a proposal,
changing one, or writing one by hand.
