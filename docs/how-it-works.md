# How acrbus works

You can review a change by handing the diff to a coding agent and asking
what's wrong. You get one model's reading, in one context, judged by the
same model that found it, once. Those weaknesses are structural, so acrbus
fixes them in the structure of the review, not in a prompt.

acrbus doesn't review anything itself. Every reviewer, Judge, Sizer and
Orchestrator is a model running headless in a harness, such as Claude Code,
pi or omp, or called through a model's API. A [binding](bindings.md) says
which harness and model each role uses, and each role can use a different
one. acrbus is what runs around them.

## Every role starts clean

Each lens, one dimension of review such as correctness or regression
safety, gets reviewers of its own, in fresh sessions that see the change
and that lens only. They don't inherit the reasoning that wrote the code,
or each other's conclusions.

What the change says about itself, its description, commit messages and
comments, reaches them as claims to check, not to take on trust.

## The builder directs, and doesn't judge

The session that built the change knows best where it could be wrong, so it
writes the reviewers a brief: what the change is for, and, for each lens,
where and how to attack. Without one, reviewers go off on tangents. The
brief gives direction only. The builder never reviews or rules on its own
change. [The builder's brief](acr-repo-setup.md#5a-the-builders-brief)
shows the format.

## Rules decide what blocks

Reviewers find; a separate Judge checks each finding against the code and
states facts about it. Is it a real defect? Was it there before the change?
Does the change claim to fix it, or make it worse? Does a new test pin
nothing?

Fixed rules turn those facts into a ruling. A fact that rests on the
change's own words holds only when the Judge quotes them from the diff.
Each lens can say what blocks in its dimension, and a finding blocks when
the Judge shows it meets that.

## Each repository is configured, then calibrated by you

Before its first review, a strong model reviews the whole repository: what
it's built of, how it's tested and shipped, and which defects its history
keeps fixing. It proposes the lenses that matter there and what blocks in
each. You read it and accept it, or change it first, and `acrbus acr run`
refuses a repository until you have.

The proposal is a starting point, not a baseline. After each review you
say how it did, finding by finding, and what it missed. Recalibrating folds
that back into the config, so the next review gets it right. A repository
with little history to go on gets a config from its code, and leans more on
your feedback.

## It gathers its own evidence

With its stages on, acrbus runs the change's tests before any reviewer
starts: coverage of the changed lines, mutants of the changed code that
every test should catch, and the tests again in random order, one at a time
and in another time zone. Reviewers that run things get a copy of the
repository of their own, and probes write and run scenarios of what the
change says it does. Nobody has to take a model's word that it ran the
tests.

## It converges

After a round with blockers, the blockers you choose are fixed. Either the
session that started the review fixes them, since it holds the change's
context, or the Orchestrator does, if its binding may write. Then new
reviewers attack again, until every lens's latest round has no blocker.

A review to convergence goes further. A fix for one lens can break another,
so after each fix every lens attacks again, and the review ends only when
every lens agrees on the same version.

## You're in the loop

A review asks you at the scope, at every round's blockers and at the
result, and whenever something is your call: a values call, a split, a lens
that didn't finish. Each ask is a short multiple-choice question: a
question carousel in Claude Code, a numbered prompt in a terminal, or a
message on your phone through [Grayboard](grayboard.md). A defect found by
several lenses is asked about once, even when they found it at different
places: findings at different places that name the same symbol, field or
function are put to the dedupe call as possible pairs, and it decides
whether one fix would fix both. Merged, the question names every lens and
place, and the fixer is told to fix it at each place.

Your verdicts, and the defects you say were missed, become feedback for the
calibration. The rules guide; you decide.

## It's sized to the change

A tier, chosen from what the change touches, decides which lenses run and
how many reviewers each gets, so a docs fix doesn't pay for a security
review. `acrbus acr plan` shows the lenses and an estimate before anything
runs, and every model call is counted.

## It pays once for what every call shares

Each round, tools map the change: each changed file's definitions, where
they're referenced, the tests that name them, its hunks, and the test
gate's results. Every reviewer and Judge gets that map in its system
prompt, after ACR's own instructions and ahead of anything that differs
between calls.

That order is what makes it cheap. The provider's prompt cache serves the
shared part to every call after the first, and each round after the first
keeps the previous round's file sections where they were, with what a fix
changed after them, so it shares the previous round's map through every
file. A role's first call warms the cache before the rest start, and a
keep-alive refreshes a prefix still in use before it could be evicted, never
sending another while the last is still going. A fix goes in groups, each a
fresh session from the same opening with only its own blockers, rather than
one session that grows with every blocker.

The report gives each role's calls, fresh and cached tokens, dollars and
times, and how many bytes each set of parallel calls shared.

## It doesn't pay twice for the same ruling

A Judge's ruling that didn't block is kept. When a later round's reviewers
report the same claim at the same place, in a file no fix has changed, the
finding takes that ruling instead of a new Judge call. A blocker's ruling is
never reused: whatever a fix was meant to settle is judged again.

With `savings` in a repository's config, a review can also put a cheaper
Judge in front of the Judge, tell reviewers after a fix where to look first,
and skip lenses a fix can't touch. Each sends anything it can't settle the
way it would have gone, and a skipped lens is reported as skipped, never as
passing. [Savings](acr-repo-setup.md#5c-savings-judges-and-later-rounds)
says how, and how to check a cheaper Judge before you trust it.

## Long calls can run in shorter sessions

Every turn of a session rereads everything it has read, so a long call's cost
grows faster than its turns. Two settings, off by default, break one up:
past a size a session hands off what it found, checked and left to a fresh one
that starts from the cached opening, and a lens can scout first, listing what
it suspects, then check each suspicion in a short session of its own. A
handoff that can't be read, or a suspicion that can't be checked, fails the
call rather than passing it. [Long calls in shorter
sessions](acr-repo-setup.md#5d-long-calls-in-shorter-sessions) has the
settings and how to measure them.

## It survives its agents, and says what they're doing

Each headless run gets an environment of its own, and a watch for going
quiet. A run whose processes stop using CPU and writing files is woken, and
if that doesn't work, it's ended and tried again in a fresh session.
Nothing cuts off a call that's working.

A review says what each call is doing at least once a minute, with the
tokens it has used so far and the run's total against its plan, and warns of
a call that has logged no new turn in a while, again for as long as it
lasts. A pi call's model request that gets no response at all is aborted
and retried, and a call whose requests keep getting none fails, so it's
tried again. Stopping a review stops its calls, and its usage still counts
every call that finished, in a round marked as unfinished. A failed reviewer or Judge is never taken for a
clean pass, and a lens with nothing for its Judge says so.

## Reviewers stay in their lane

Reviewers read the code under review, not your other files, and nothing
they do is written back to the repository. A fix keeps to the source, and
every file it changes is listed: a binary, or a file none of its blockers
points at, is put to you, to keep or revert.

## It's measurable

`acrbus acr trial` reruns a review design blind on a change whose defects
are already known, and scores what it catches. A change to acrbus, a lens
or a model choice is a measurement, not a guess.
