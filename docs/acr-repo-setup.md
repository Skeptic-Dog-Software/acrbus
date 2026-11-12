# Setting up ACR review for a repository

To set one up, start with the [quickstart](quickstart.md): the configurer
writes the config for you, and `acrbus acr doctor` checks what it needs.
This guide is the reference behind it, for reading a proposal, changing
one, or writing one by hand.

How to set up a repository you don't know yet, on a machine that may be new,
so that its review is configured correctly. The pipeline reviews well only
when its reviewers know two things the diff can't tell them. Each lens needs
to know how to attack, and every reviewer needs to know how to work in this
repository on this machine. The brief stage then adds where to attack in
each change. This guide covers what's written once, per repository and
machine: by `acrbus acr configure` (§7), which follows it, or by you.

The review config lives outside the repository, at
`~/.config/acrbus/acr/<repository name>.yaml`, so nothing in a change can
switch off its own review. Its schema is `schema/acr.schema.json`, and
`examples/acr/` has two small configs to start from.

## 1. The machine

```sh
acrbus service install
acrbus setup acr                 # token, bridge, bindings, confinement
acrbus acr add-repo <path>       # let ACR review the repository here
```

Every role's binding comes from `~/.config/acrbus/acr.yaml`. Reviewers do
best when they can run things: give the reviewer binding a shell and the
writing tools, as `acrbus setup acr` does. A reviewer with them works in a
copy of its own, so nothing it does reaches the checkout. What guides it
beyond that is ACR's system prompt, which every role gets through its
bridge: it's in [src/acr/system-prompt.md](../src/acr/system-prompt.md). The probes stage,
on by default, needs them: without them it's left out, and `acr validate`
and `acr plan` say so.

```yaml
allowed_bindings:
  reviewer-runs:
    agent: <the ACR agent>@<this machine>
    model: <a mid-size model>
    effort: high
    tools: [Read, Grep, Glob, Bash, Write, Edit]
defaults:
  bindings:
    reviewer: reviewer-runs
```

Calls have no time budget. A call runs until it answers, and one that
stalls, using no CPU and writing no file for its bridge's `idle_secs` (20
minutes unless set), is ended and tried again by its bridge. Left out, a
binding's `timeout` is as long as its bridge allows; set, it's a cap that no
call should reach, not a budget, since a call cut off loses its work. While
a review runs, it says what's at work at least once a minute.

If a repository's reviews may use only some models, say so in its config,
with patterns:

```yaml
allowed_models: ["vendor-b/*"]
```

Every binding its reviews would call, the configurer too, must name a
model one matches, or the config is refused before anything
runs. Set in your acr.yaml's `defaults`, it holds every repository on the
machine that doesn't set its own.

## 2. Learn the repository first

Before writing the config, find out the following, from the repository's
files, its README and scripts, and its CI:

- **How it builds and tests offline.** List the commands, the caches they
  need, and any environment they assume, such as a time zone or a locale.
  Note anything that reaches for the network on a first run, such as a
  package resolve or a build hook that downloads a native library, and how
  to avoid it.
- **What can hurt users or production.** That means data and migrations,
  keys and secrets, a live backend, purchases, and release builds. These
  become lenses.
- **Where claims live.** Look for a bug ledger, docs that cite code by line,
  and comments that promise behaviour. False claims are findings.
- **Where reviewers will need to read beyond the repository.** That means
  dependency sources, the SDK, and sibling repositories it talks to.
- **What must never run in a review.** For example, anything that touches a
  real backend, a store, or the network.

## 3. Lenses: what's wrong, and how to attack it

Each lens is one dimension of review:

```yaml
lenses:
  - id: regression-safety
    criteria: "No guard or test is weakened: no deleted or loosened assertion, ..."
    attack: "Run the new tests in another time zone, in random order, and one at
      a time; mutation-test in your own copy: each mutant must compile and fail
      a test on an assertion; was an old test replaced by a looser one?"
    triggers:
      paths: ["lib/**", "test/**"]
```

- **`criteria`** is the bar: what counts as wrong in this lens. The Judge
  rates by it.
- **`attack`** gives the techniques that find what the criteria call wrong.
  Write how to find it, not what to check: run something, break something,
  try an edge. For example:
  - mutation testing, where each mutant must compile and fail on an
    assertion;
  - determinism across time zones, orders and repeat runs;
  - network denied, with a positive control;
  - claims checked against the code, line numbers included;
  - a generated-code regeneration compared with what's committed;
  - for concurrency, or work deferred past a lock or a return, an
    interleaving table first: for each changed method and each piece of
    shared or deferred state, every other method, thread or goroutine that
    can run at the same moment, and whose context, cancellation and error
    the deferred work uses. A finding names its interleaving.

  A reviewer who only reads traces each path on its own, and calls code
  safe that two paths break together: a shutdown that returns while
  another call is still writing, or cleanup that runs under another
  caller's context and is cancelled with it. Reviewers who write and run
  tests catch those that readers miss. Where a repository has
  concurrency, give its lens this attack, and turn on `probes`.
- **`blocker`** says what blocks in this lens: its dimension's definition
  of a blocker, set at calibration (§7) from the dimensions the
  repository's full review used. Reviewers get it, and a finding the Judge
  shows meets it, quoting the words it meets, blocks. Without one, the
  Judge's severity and the general rules decide.
- **`triggers`** choose the lens for a change: by paths, content patterns,
  or a plain-language description the Orchestrator judges. Each round
  checks them again, on the change as it is then.
- **`elegance`** is a lens every repository gets, built in, with its own
  criteria, attack and blocker. Give a lens with that id to word it for
  this repository; it can't be left out.
- **Keep everything about a particular change out of the config.** The brief
  stage writes that for each change.

## 4. The runbook: how to work here

The runbook is text that every role able to run things gets, and so does the
brief stage. It's operational. Write it for a reviewer starting cold in a
fresh copy of the repository, on this machine:

```yaml
runbook: |
  In your copy, first run `<offline dependency install>`, then copy
  <the cache a first run would otherwise download> into <where it goes>.
  Then pass <the flag that stops resolving again> to every test and build.
  Run tests with `<the test script> <files>`: it sets <the environment the
  tests need>.
  Wrap any command that could hang: `perl -e 'alarm 600; exec @ARGV' <command>`.
  Never touch <the backend, stores, the network>: no fetch, pull or push.
  Stop only processes you started, by PID: never kill by pattern.
  Read-only sources: dependencies under <path>, the SDK under <path>, and
  sibling repositories under <path>: tests.setup copies them into
  target/acr/ in each copy, since reviewers' file tools can't read outside
  their copy. Read them there.
  Before <a command that needs the network>, stop.
```

Then test it. Make a copy of the repository the way a reviewer gets one,
follow the runbook by hand, and run one test file. If anything reaches the
network or fails, fix the runbook.

## 5. Stages and the test command

```yaml
stages: [gate, brief, context, claims, probes, test-writing]
tests:
  setup: "<the offline package install>; <the cache copy>; <a first build or test run>"
  command: "<setup that is cheap to repeat>; <the test script> {tests}"
  coverage: "<setup>; <the test script with coverage>"
  report: coverage/lcov.info
  parallel: 1
  timeout: 900
  mutants: 12
```

- **`stages`** left out means `[probes]`: every change says it does
  something, and running scenarios of that finds what reading misses. List
  `stages` to choose for yourself, and keep `probes` in it unless the
  repository has nothing a scenario could run.
- **`gate`** runs first in every round. It finds changed lines no test runs
  (from the coverage report) and mutants of the changed code every test
  passes, and tells every reviewer what it found. Set `tests.coverage` to
  a command that writes an lcov report. Most test tools have a flag for
  that. `tests.gate: stop` makes a gap that blocks end the round before
  any reviewer runs. Leave it unset, which is `warn`: in one trial, `stop`
  saved 14M tokens, but the reviewers it stopped found 19 of the 28 defects
  a full review had found in its first round when they were let run.
  The change's own tests, which the gate reruns other ways and which make
  a surviving mutant block, are the test files it adds or changes: a path
  with "test" or "spec" in it. In Rust, a source file counts too when the
  change adds lines to its top-level `#[cfg(test)] mod … { … }`; that's the
  only language whose in-file tests are known. A pin the test writer puts
  in such a module is checked on the code from before the fixes with that
  module alone brought over.
- **`test-writing`** works between rounds of a review to convergence. After
  the steelman's fixes, a fresh writer pins each fixed blocker with a test.
  Each pin must fail on the code from before the fixes and pass after.
- **`tests.setup`** runs once a round, in one copy of the change, before
  any reviewer's copy is made from it. Every copy that round, reviewers'
  and mutants', is then cloned from it with its build caches, so none
  starts cold. Put the package install and a first build or test run in
  it.
- Size `tests.parallel` to the machine: test runs are CPU- and
  memory-bound, so on a small machine, run one at a time.

- **`brief`**: before each round, the Orchestrator writes each lens a brief
  for this change. It covers where to attack and how, as questions to
  settle, not conclusions.
- **`context`**: every role gets what git says about the changed files:
  what earlier commits and the docs say of them, and where their tests and
  callers are. It's built from git, with no model call, and saves every
  reviewer looking those up, so it's on by default.
- **`claims`**, **`mutation`** and **`probes`** are lenses the framework
  adds, and `gate` adds one called `tests`, for what it finds. With the
  gate on, the mutation stage runs inside it, so there's no `mutation`
  lens. Probers write scenarios as tests in copies of their own and run
  them, concurrent calls and hangs among them. What a prober writes is
  kept: after a fix, the next round's probers start with it in their
  copies and run it again, which tells whether the fix held. Mutation runs `tests.command` itself, in throwaway copies, once
  clean and then once per mutant, so the command must work in a fresh copy
  with no one at the keyboard. `{tests}` stands for the test files a mutant
  names. Put cheap setup in front of it, such as the offline dependency
  install and the cache copy from the runbook.

## 5a. The builder's brief

The session that built a change briefs its reviewers best: it knows what
the change does and where it could be wrong. Pass its brief with
`acrbus acr run --brief <file>`:

```yaml
scope: |
  What the change does, mechanism by mechanism: what it fixes, how,
  what it keeps, what its tests now pin, and what it doesn't touch.
lenses:
  correctness: |
    The parts to attack in this lens, and how: what to run, what to
    break, which edges and callers to check.
  regression-safety: |
    ...
```

Every role gets the scope statement, as the builder's claims to check.
Each reviewer gets its own lens's part. The `brief` stage covers any lens
the builder left out. After a fix, the steelman briefs the next round,
since by then it holds the change's context.

## 5b. What acrbus does, so the config needn't

- **Where commands run.** acrbus runs `tests.setup`, `tests.command`
  and `tests.coverage` itself, under `sh -c`, in copies of the repository,
  in the environment of whoever ran the review: their PATH, HOME and
  caches.
- **Where reviewers run.** Reviewers, the Judge and the Orchestrator run
  through their bridge, with its `home` and `path` from config.toml, not
  the user's. A reviewer whose binding has Bash, Write or Edit works in a
  copy of its own. Its file tools can't read outside that copy, though its
  shell can. So anything a reviewer runs must find its toolchain and
  caches there. `acrbus acr doctor` checks.
- **What's reviewed.** `review_scope` is `whole-pr` unless the config says
  otherwise: all of base..head as one diff, which is what merging puts on
  the base. Choose `bottom-of-stack` only where each pull request in a stack
  merges on its own; it reviews just the first commit after the base.
- **`{tests}`.** It's replaced with the test files to run, each quoted, with
  spaces between them. Left empty, the command should run the whole suite.
- **The gate without coverage.** With no `tests.coverage`, the gate still
  runs its mutants and its other-way runs; only the uncovered lines are
  missing.
- **Decided non-issues.** Accepting a proposal puts its non-findings in
  `<name>.not-findings.md`, beside the config, and points `not_findings` at
  it. A proposal needn't name the file.
- **Shared facts.** Each round, acrbus maps the change with tools,
  not models: for each changed file, its definitions the change touches,
  where they're referenced (`git grep`), the tests that name them, and its
  hunks; then the test gate's results. With `--pr`, it carries the pull
  request's words too, fetched with `gh` each round: its title,
  description and the issues it links, ahead of the files, and its review
  comments, which can grow, after the round's number. Every reviewer and
  Judge gets the map in its system prompt, after ACR's own and before its
  own text, so the provider's prompt cache serves it to all of a role's
  calls but the first. Files a fix left alone come first, as they were,
  and those it changed after them, so a round shares the last one's cached
  prefix up to the first file a fix touched; the round's number, the
  review comments and the gate's results come last. The map is kept in the review's folder, as
  `round-<n>.facts.md`. It goes to no call unless every agent the
  reviewers and the Judge are bound to can take it: pi reads it from a
  file; Claude Code and omp take it in the same argument as ACR's prompt,
  up to 100 KB. The `facts` section's settings, each with its default:

  ```yaml
  facts:
    shared: true          # build the map and give it to reviewers and Judges
    warm_secs: 60         # a role's first call goes alone, until its turn shows
                          # it read its prompt, or this many seconds; 0: no wait
    keep_alive_secs: 240  # refresh a prefix still in use this often; 0: off
    judges: per-lens      # per-finding: a Judge per finding, given its claim
                          # and the lines it cites, none of the reviewer's case
    reviewers: all        # facts-only: no briefs a model wrote, and no
                          # decided non-issues (the Judge still applies them)
    match_tools: true     # same tool definitions for roles on one model
    learned: true         # give the files reviewers keep opening, whole
    learned_bytes: 24576  # the most bytes of them a round's facts carry
  ```

  With `match_tools`, the reviewers and the Judge bound to one bridge and
  model are all shown the union of their tools, so their prompts share a
  prefix, and each call is held to its own: pi's calls load an extension
  acrbus ships that refuses any other tool and says which it may use. Other
  agents keep their own tool lists. A keep-alive is a minimal call through
  the role's binding, sent only when its bridge has a slot to spare, and
  counted apart from the roles' work. The provider's prompt cache is keyed
  to the review and the bridge and model, so every round's calls to one
  model reach the same cache. A provider that keeps a cache per replica,
  such as Fireworks, routes by that key, so pi's calls send it three ways,
  for a gateway in between that forwards only some of them: the
  `x-session-affinity` header; `x-bf-eh-x-session-affinity`, which Bifrost
  forwards as `x-session-affinity`, unless its allowlist leaves it out;
  and the request's `user`, in an OpenAI chat request that sets none.

  Every role's pi call loads that extension, and its stderr (`acrbus bridge
  log <agent> <id> --stderr`) says, for each turn, what the cache served,
  from pi's own usage, which a provider reports in the response body:
  `acrbus cache: turn 2: 12288 of 13644 prompt tokens read from the cache
  (1356 fresh, 0 written, 150 out)`. Hits come in whole blocks. A provider
  that also counts them in a response header gets a `response` line as
  well. Each call's first turn goes into its usage, so progress and `acrbus
  acr usage` show whether it read the warmed prefix. A cache-write count of
  0 on every call says nothing: some providers, Fireworks among them, don't
  report writes.

  pi rounds a thinking level its model doesn't have to one it does, and
  says so there as `acrbus effort:`: GLM 5.3 on Fireworks has `low`, `high`
  and `max`, so `medium` runs as `high`. Where the provider echoes what it
  applied, as Fireworks does in `fireworks-conversation-options`, a second
  `acrbus effort:` line says so, once a call, such as `the provider applied
  reasoning_effort high`. On GLM 5.3 Flash, `--thinking off` still reports
  `reasoning_effort low`: off isn't off there. `acrbus acr judge-check
  --per-finding` sets per-finding verdicts against per-lens ones on a key,
  before you switch.
- **Fewer turns.** A reviewer's cost is mostly what each turn reads again:
  everything before it. So the map carries what reviewers would otherwise
  spend turns to open. Each reviewer call's bridge says which files its
  agent read; the review counts them per lens in a record kept for the
  repository (`acr-learned/<name>.json` in the state folder), and each
  round's facts carry the files this repository's reviewers keep opening,
  whole, after the change's own files, most opened first, up to
  `learned_bytes`, each one opened by at least two reviewer calls. Every
  reviewer and Judge reads them again each turn, so a file pays only when
  reviewers would open it early anyway: keep the budget small, and watch
  turns and cache reads per reviewer when you change it. A file given once stays where it was for the rest of the
  review, so the facts only grow at their end, and the next review of the
  repository starts with what this one learned. Nothing is taken away: a
  reviewer still opens what it likes, and its prompt asks it to read from
  the facts first and to ask for several files in one turn. Trials don't
  learn or use the record, so their arms stay blind. `learned: false` turns
  it off. A pi bridge also leaves pi's pointer to its own documentation out
  of a review's prompt, since pi says it's for questions about pi itself:
  its kind sets `ACRBUS_DROP_PI_DOCS = "1"` in its `env`, and `"0"` keeps it.
- **Plan's estimate.** `acrbus acr plan` counts only the text the review
  sends and reads back. `costs.per_call` adds a measured average for each
  role, such as `reviewer: {input_tokens: 40000, output_tokens: 4000}`, to
  bring the estimate nearer the real use.

## 6. Check it

```sh
acrbus acr validate ~/.config/acrbus/acr/<name>.yaml
acrbus acr plan --base <ref>     # tier, lenses, bindings, cost; no model calls
```

`acrbus acr check <key>` checks that each kind of session starts. It also
checks that each one reads the copy and, for a reviewer with a shell, works
in a copy of its own. It says whether a session can read a file outside its
folders. That fails the check only for an agent its bridge confines, as
`acrbus setup acr` confines Claude Code's file tools; for any other agent
it's a note, and yours to judge. A trial's builder runs on Claude Code, so
its session is checked only where `setup acr` has set that up; a review
doesn't need it, since the session that runs the review is its builder.

## 7. Calibrate: configure it from the repository, then from your feedback

A review is only as good as its config: which dimensions it attacks, and
what blocks in each. So the config is drawn from the repository itself, and
then from what you say of its reviews. `acrbus acr run` refuses a
repository whose config was never accepted, or has changed since, and
notes when only the Judge has changed. `--uncalibrated` reviews it anyway.

1. **Configure it.** `acrbus acr configure --repo <path>` has the
   configurer, the strongest model your `acr.yaml` binds as `configurer`,
   review the whole repository as it is now, in a clone of its own: what
   it's built of, how it's tested and shipped, what it keeps and talks to,
   and which kinds of defect its history keeps fixing. It runs the test
   commands it proposes, to be sure they work. It proposes the config this
   guide describes: the dimensions that matter here, each with its
   criteria, attack, blocker and triggers, and the tiers, runbook, tests and
   stages. The proposal waits beside the config, at
   `<repository>.proposed.yaml`, with its reasons in `<repository>.proposed.md`.
2. **Read it and accept it.** Change what you disagree with, then run
   `acrbus acr calibration --repo <path> --accept`. That puts it in place,
   keeping the config it replaces as a backup, and calibrates the
   repository. A config you wrote yourself is accepted the same way.
3. **Review, and say how each review did.** After a review,
   `acrbus acr feedback <review id>` lists its findings. Mark each one
   (`--finding <number> --verdict right|not-a-defect|not-a-blocker|should-block`,
   with `--note` if it helps), and say what it missed
   (`--missed <text> --lens <id>`). A finished review's findings are kept
   90 days for this.
4. **Recalibrate when you're ready.** `acrbus acr calibrate --repo <path>`
   hands the configurer the config, the repository again, and the feedback
   given since the last recalibration. It proposes a revised config:
   criteria and blockers tightened where something wasn't a defect or
   shouldn't block, widened, or attacked better, where a defect was missed,
   and lenses added, merged or retired. Read it and accept it as in step 2.
   `acrbus acr run` reminds you while feedback is waiting to be folded in.

## 8. Measure what it finds

Calibration fits the config to the repository. Whether the reviewers find
what your most careful review finds is a separate question, for when you
change acrbus, a lens or a model:

1. Take a real change, new work nobody has reviewed. Freeze a key with its
   repository, base, head, a paragraph of what it's for, and the dimensions.
2. Run `acrbus acr trial <key> --arm separated` before any other review, so
   nothing can leak.
3. Review the change by your most careful method to convergence, and write
   its findings into the key, each marked as a blocker or not, and found in
   round 1 or later.
4. Run `acrbus acr rescore <key> --results <file>`. A key finding counts as
   caught if the pipeline reported it at any severity. Blocker agreement is
   reported apart from that.
5. Read what the pipeline missed, and why. A trial keeps every reviewer's
   transcript in `<state>/bridge/<agent>/log/<run>/transcripts`. Most misses
   trace to a missing technique in a lens's `attack`, or to a runbook gap
   that kept a reviewer from running something. Fix those, and run the next
   change.

## 9. Work that isn't code

ACR reviews a change, which needs a repository, a base and a head. A
design, a document or a plan has versions but no repository, so carry it
into one:

```sh
acrbus acr carry --name design --base before.md --head draft.md
```

This commits each version, a file or a folder of text, to a carrier
repository in acrbus's state, checks the head out there, and prints how to
review it. Leave out `--base` when there's no earlier version: the whole of
the work is then new. Carrying the same versions again gives the same
commits.

- **Export to stable text.** Two exports of the same version must match,
  or every diff is noise. Use a fixed order, such as frames and then what's
  in them, and leave out positions, colours and ids that change from one
  export to the next.
- **Configure the carrier like any repository.** Its config is
  `~/.config/acrbus/acr/<name>.yaml`, and one carrier serves every piece of
  the same kind of work. Its lenses are that work's own: for a design, say,
  requirements coverage, consistency, feasibility, user flow, accessibility
  and clarity. The test stages don't apply. `claims` and `context` do.
- **Calibrate it the same way.** A key for work that isn't code gives
  `head_file`, and `base_file` if there's a version before it, in place of
  `repo`, `base` and `head`, and `carrier` for the carrier's name. Paths are
  from the key's own file. `acr calibrate` and `acr trial` carry it first.

## What not to put in the config

Don't put severity tables or objectives written per repository or project.
A change's objective comes from its own description, comments and commit
messages. The config says what's wrong in each lens, how to find it, and how
to work here.
