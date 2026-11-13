# Bindings

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
minute:
- how long each call has run, and when its bridge last saw it work;
- the tokens it has used so far, where its bridge reads its agent's usage:
  input not read from a cache and output, then cache reads and dollars
  apart, since an agent's cache reads can be many times the rest;
- a call waiting for a free slot on a full bridge, how long it has waited,
  and how many of the bridge's calls are running;
- the run's total against its plan's estimate, counted the same way, with
  a warning once a call passes its share of the most the plan allows, or
  the run passes that most;
- each round's setup, what each lens's Judge has to rate, and a lens that
  has none, such as one whose reviewers found nothing;
- a call whose agent has logged no new turn for `sessions.stall_warn_secs`
  (3 minutes by default), once, apart from the bridge's own idle watch;
- after a fix, the files it changed, and those flagged for you.

Where each lens of the round stands is kept in `lenses.json` in the
review's folder as it moves, and `acr ask` shows it while the review
runs. The Sizer chooses a tier from the file list, the lines each file
changes and what the change says it's for, with no tools; it reads the
change, with its tools, whenever that isn't enough to be sure. Each
Judge's brief starts with the diff's hunks its findings point at; it
keeps the whole change and every tool its binding gives.

A repository's review config can set `allowed_models`, patterns
such as `vendor/*`: every binding its reviews would call must name a model
one matches, or the config is refused. Set in `defaults`, it holds every
repository that doesn't set its own.
