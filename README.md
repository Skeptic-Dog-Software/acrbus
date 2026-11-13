# acrbus

acrbus runs adversarial convergence review (ACR) on code changes. Point it
at a change, and fresh reviewers attack it one dimension at a time. A
separate Judge checks what they find against the code, the blockers get
fixed, and fresh reviewers attack the new version. It stops when no lens it
attacks has a blocker, or at its round cap, where you decide.

acrbus doesn't review anything itself. Its reviewers and its Judge are
models running headless in harnesses you already use, such as Claude Code,
pi or omp. acrbus orchestrates them: each gets a clean session and one job,
the Judge states facts about each finding and fixed rules turn them into
rulings, and the review keeps attacking until no lens has a blocker or its
round cap puts the rest to you.

It runs on one machine. Its daemon serves a Unix socket on this machine,
and opens no TCP port, except a loopback one briefly during
`grayboard login`. Your code still leaves the machine: each harness sends
what it reviews to its model's provider, and `--pr`, `--post-pr` and
Grayboard reach GitHub and the team server.
[tailbus](https://github.com/Skeptic-Dog-Software/tailbus), built on
acrbus, carries the same bus between machines over Tailscale.

## How a review goes

1. **You configure the repository once.** A strong model reads the whole
   repository: what it's built from, how it's tested and shipped, and which
   defects its history keeps fixing. It proposes the lenses, the dimensions
   worth reviewing there, and what blocks in each. You read the proposal,
   change what you like, and accept it. acrbus won't review a repository
   until you have.
2. **The builder briefs the reviewers.** The session that built the change
   says what it's for, and where each lens should look. The brief directs;
   it doesn't judge, though where a reviewer is pointed shapes what it
   finds.
3. **The change is sized.** Each lens's triggers decide whether it runs on
   the change, and a tier, picked from what the change touches, sets how
   many reviewers each gets and can require lenses. A docs fix skips a
   security lens whose triggers don't match docs, though by default it
   still pays for the `elegance` and `probes` lenses.
4. **acrbus gathers evidence.** With its `gate` stage on, which is off by
   default, it runs the change's tests itself before any reviewer starts: coverage of the changed lines,
   mutants of the changed code, and the tests again in random order. Nobody
   has to take a model's word that the tests ran.
5. **Reviewers attack.** Each lens gets its own reviewers, in fresh sessions
   that see the change and that lens only. What the change says about
   itself is a claim to check, not a fact.
6. **The Judge rules.** A separate Judge checks each finding against the
   code and states facts about it: is it real, was it there before, does the
   change make it worse. Fixed rules turn those facts into a ruling;
   whether a finding meets its lens's blocker definition, and otherwise its
   severity, is still the Judge's call.
7. **You decide.** At the scope, at each round's blockers and at the
   result, the review asks you short multiple-choice questions: in Claude
   Code, in a terminal, or on your phone through Grayboard.
8. **Fix, and attack again.** The blockers you choose are fixed, by the
   review's Orchestrator or by the session that started the review, and
   fresh reviewers attack the new version: with `reattack: blocked`, the
   lenses that had blockers, any that didn't finish, and any the fix now
   brings in; with `reattack: all`, every lens. Each fix is held to the
   files its blockers point at. A lens that passed before isn't attacked
   again under `blocked` unless you ask for another round or report a
   defect the reviewers missed. A review to convergence, which `acrbus acr
   trial --arm converge` runs, attacks the lenses each fix touched, then
   every lens that hasn't attacked the final version, so it ends only when
   they all pass the same version.
9. **It learns, when you recalibrate.** You say how the review did,
   finding by finding, and what it missed. `acrbus acr calibrate` folds
   that into a config kept outside the repository, for you to accept.

[How it works](docs/how-it-works.md) covers each step in more depth,
including how acrbus avoids paying twice for what calls share and what it
does when an agent stalls.

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

That runs the daemon at login, installs the Claude Code plugin, and sets
up headless Claude Code as ACR's reviewer, with its token, its bindings and
its service. Each part is also its own command: `acrbus service install`,
`acrbus setup claude` and `acrbus setup acr`. To review with pi, omp or
another harness, see [Another agent](docs/quickstart.md#another-agent).

## Review a change

[The quickstart](docs/quickstart.md) goes through it step by step, for you
or your agent, with how to tell each step worked. In short:

1. Allow the repository: `acrbus acr add-repo <path>`.
2. Configure it: `acrbus acr configure --repo <path>`. The configurer
   proposes the repository's review config, with its reasons, at
   `~/.config/acrbus/acr/<repository>.proposed.yaml`.
3. Read it, change what you like, and accept it:
   `acrbus acr calibration --repo <path> --accept`.
4. Review: `acrbus acr run --repo <path> --base <ref> --brief <file>`.
   [The builder's brief](docs/acr-repo-setup.md#5a-the-builders-brief)
   says what goes in the file.
5. Say how it did, `acrbus acr feedback <review id>`, and when you're
   ready, fold that in: `acrbus acr calibrate --repo <path>`.

[Setting up ACR review for a repository](docs/acr-repo-setup.md) explains
the config, for reading a proposal or writing one yourself.
[examples/acr](examples/acr) has examples.

## Commands

| Command | Does |
| --- | --- |
| `acrbus acr doctor` | Checks this machine and a repository are ready for review, and says what to fix. |
| `acrbus acr validate`, `acrbus acr schema` | Checks a repository's config, or prints its schema. |
| `acrbus acr plan` | Shows the tier, lenses, bindings and estimated cost, with no model calls. |
| `acrbus acr configure` | Has the configurer review the whole repository and propose its config. |
| `acrbus acr calibration --accept` | Accepts the proposed config, or your own. acrbus calls that calibrating: you've read and accepted it; nothing is measured. |
| `acrbus acr run` | Reviews a change. |
| `acrbus acr ask <run id>` | Shows what a review waiting for you asks, as question carousels or `--text`, or, while it runs, where each lens of its round stands. |
| `acrbus acr resume <run id>` | Answers it (`--answers <json>`, or `--fold`, `--defer` and `--waive` by blocker), or says your fixes are in (`--fixed`). |
| `acrbus acr stop <run id>` | Stops a review in one step, running or waiting: its process and calls stop, it ends as stopped, and it says what it left. |
| `acrbus acr feedback <review id>` | Lists a finished review's findings, for you to say how each did and what it missed. |
| `acrbus acr calibrate` | Folds your feedback in: the configurer proposes a revised config. |
| `acrbus acr clean` | Clears the copies that finished reviews, checks, the doctor and the configurer left, now rather than at the daemon's cleanup. `--dry-run` lists them, with their size. |
| `acrbus acr carry` | Carries work that isn't code, such as a document, into git so it can be reviewed. |
| `acrbus acr check`, `acrbus acr trial` | Checks that a trial can run, then measures a review design on a change whose defects are known. |
| `acrbus acr judge-check <key>...` | Compares the Judge's rulings with a past review's, as a benchmark. `--first-pass <binding>` also tries a first-pass Judge. |
| `acrbus grayboard login`, `status`, `logout` | Signs in to a Grayboard team server, so reviews ask you there and claim their changes. See [Grayboard](docs/grayboard.md). |

The method itself is in [the ACR skill](plugin/skills/acr/SKILL.md), and
the config's schema in [schema/acr.schema.json](schema/acr.schema.json).

## More

- [How it works](docs/how-it-works.md): the design, step by step, and why
  each part is there.
- [FAQ](docs/faq.md): questions and answers on every part, each with
  where to check it in the code.
- [Bindings](docs/bindings.md): which model and harness each role uses, and
  what a review reports while it runs.
- [Bridges](docs/bridges.md): how acrbus runs a harness headless, and what
  it keeps of every run.
- [Grayboard](docs/grayboard.md): answering a review's questions from
  Slack, and holding a claim on a change while it's reviewed.
- [The bus](docs/bus.md): the message bus underneath, its commands, doors
  for agents that run in other OS accounts, and building your own program
  on acrbus.

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
