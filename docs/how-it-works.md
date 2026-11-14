# How acrbus works

A single agent with a good prompt and tools can review a change well,
including checking its own findings in a fresh pass. Its limit is that it
reads in one framing, and usually rates what it found itself. acrbus bets
that splitting review by dimension, judging separately and attacking again
after fixes catches more, and puts that in the structure of the review, not
in a prompt. That bet isn't measured yet; the
[FAQ](faq.md#is-there-evidence-it-finds-more-than-a-simpler-review) says
what would show it.

acrbus doesn't review anything itself. Every reviewer, Judge, Sizer and
Orchestrator is a model running headless in a harness, such as Claude Code,
pi or omp. A [binding](bindings.md) says
which harness and model each role uses, and each role can use a different
one. acrbus is what runs around them.

## Every role starts clean

Each lens, one dimension of review such as correctness or regression
safety, gets reviewers of its own, in fresh sessions that see the change
and that lens only. They don't inherit the reasoning that wrote the code,
or each other's conclusions. A reviewer spends its effort in its lens and
doesn't go looking outside it, but a real defect it comes on there, above
all one where another dimension interacts with its own, it still reports,
saying which lens it belongs to, and the Judge rates it like any other.

What the change says about itself, its description, commit messages and
comments, reaches them as claims to check, not to take on trust.

## The builder directs, and doesn't judge

The session that built the change knows what it's for and where the risky
parts are, though not its own blind spots, so it writes the reviewers a
brief: what the change is for, and, for each lens, where and how to attack.
The brief gives direction only, and the builder never reviews or rules on
its own change. Where a reviewer is pointed still shapes what it finds: the
design bets that a brief prevents more tangents than it causes anchoring,
and `facts.reviewers: facts-only` gives reviewers nothing a model wrote. [The builder's brief](acr-repo-setup.md#5a-the-builders-brief)
shows the format.

## A separate Judge states facts, and rules map them

Reviewers find; a separate Judge checks each finding against the code and
states facts about it. Is it a real defect? Was it there before the change?
Does the change claim to fix it, or make it worse? Does a new test pin
nothing?

Fixed rules turn those facts into a ruling: they drop some findings and
raise others to High, and otherwise the Judge's own severity decides. So
whether most findings block is still a model's call; the rules make the
mapping the same everywhere, and auditable. A fact that rests on the
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
say how it did, finding by finding, and what it missed. Recalibrating, for
a config kept outside the repository, folds that back into the config, aiming the next review at what this one got
wrong. A repository
with little history to go on gets a config from its code, and leans more on
your feedback.

## It gathers its own evidence

With its `gate` stage on, which is off by default, acrbus runs the
change's tests before any reviewer starts: coverage of the changed lines, mutants of the changed code that
every test should catch, and the tests again in random order, one at a time
and in another time zone. Reviewers that run things get a copy of the
repository of their own, and probes write and run scenarios of what the
change says it does. Nobody has to take a model's word that it ran the
tests.

## It converges

After a round with blockers, the blockers you choose are fixed. Either the
session that started the review fixes them, since it holds the change's
context, or the Orchestrator does, if its binding may write. Either way,
each fix is held to the files its blockers point at and the tests beside
them: what it changes elsewhere, and any binary no blocker points at, is
put back before the next round reads it, and what it needs changed
elsewhere, or a doc it left stale, comes to you as a note. A cheaper
fixer binding can take the groups of blockers rules call easy: few files,
not Critical, their cause stated, no one's call, and not recurring. Then new
reviewers attack again, until every lens's latest round has no blocker, or
`max_rounds` puts what's left to you.

Which lenses attack after a fix is an intensity you set with `reattack`:
`blocked`, the lenses that had blockers, or `all`, every lens, since a fix
for one lens can break another. Left unset, the review asks at its scope
gate, and takes `blocked` if nobody is asked there; the report says which,
and whose choice it was. `acrbus acr plan` shows what `all` would be
expected to cost here.

A review to convergence goes further: after each fix, a steelman folds in
the blockers and writes tests, and, below its 20-round cap, the review ends
only when every lens passes the same version.

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

The review records where each gate's answers came from: a terminal, a
Grayboard reply and its sender, or `acr resume --answers`. A run's flags
that make it weaker than its repository's config, such as a gate that asks
you less or fewer rounds, are said at its start, at every gate and in the
report. With `answers: person` in the config, `--answers` and the decision
flags are refused, and weakening flags need confirming at a terminal: an
agent that runs the review can't answer for you through them. It isn't a
wall against an agent with a shell running as you, which could type at a
terminal or write the files acrbus reads; it closes the channels that
don't need one.

A spend cap, `spend_cap`, pauses a review once what its calls spent reaches
it, each call priced as the report prices it: no new call starts, calls at
work finish, and you're asked to raise the cap, go on without it, or stop.
It never cuts off a working call. With nobody in the loop, as in a trial,
the review ends there.

## It's sized to the change

Each lens's triggers decide whether it runs on a change, and a tier,
chosen from what the change touches, sets how many reviewers each lens gets
and can require lenses. A docs fix skips a security lens whose triggers
don't match docs, though by default it still pays for `elegance`, `probes`
where reviewers can run things, a Judge for each lens with findings, and
the human gates. `acrbus acr plan` shows the lenses and an estimate before
anything runs, and every model call is counted.

## It pays once for what every call shares

Each round, tools map the change: each changed file's definitions, where
they're referenced, the tests that name them, its hunks, and the test
gate's results. Every reviewer and Judge gets that map in its system
prompt, after ACR's own instructions and ahead of anything that differs
between calls.

That order lets the provider's prompt cache serve the shared part to every
call after the first, and each round after the first
keeps the previous round's file sections where they were, with what a fix
changed after them, so it shares the previous round's map through every
file. A role's first call warms the cache before the rest start, which then
start a few seconds apart, so a burst of new sessions isn't spread over
replicas that don't hold the prefix, and a keep-alive refreshes a prefix still in use before it could be evicted, never
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

## A step that did no work fails

A step is judged by what it must do, never by what it found: a reviewer that
read the change and found nothing did its work. One observed doing none
fails, as any failed step does, to be tried again or escalated, and never
passes; progress says "did no work" and why.

- A fix, or one session of it, that leaves the checkout as it found it,
  unless it declines each of its blockers with a reason.
- A handoff that adds nothing: no change, nothing new found or checked, and
  the same left.
- A reviewer, scout or prober whose call made no tool call, when its bridge
  counts them; and a prober that wrote no probe in its copy.
- A Judge that rated none of the findings it was given. A first pass that
  rated none settles nothing: the Judge rules.
- A test gate run, coverage, mutants' baseline or another way, that passed
  but says it ran no tests.

The Sizer's first look, which has no tools, keep-alives and merging
duplicates aren't steps of this kind.

## It survives its agents, and says what they're doing

Each headless run gets an environment of its own, and a watch for going
quiet. A run whose processes stop using CPU and writing files is woken, and
if that doesn't work, it's ended and tried again in a fresh session. The
idle watch never cuts off a call that's working; only a call's time cap,
4 hours for the shipped kinds, can.

A review says what each call is doing at least once a minute, with the
tokens it has used so far and the run's total against its plan, and warns of
a call that has logged no new turn in a while, again for as long as it
lasts. A pi call's model request that gets no response at all is aborted
and retried, and a call whose requests keep getting none fails, so it's
tried again. Stopping a review stops its calls, and its usage still counts
every call that finished, in a round marked as unfinished. A failed reviewer or Judge is never taken for a
clean pass, and a lens with nothing for its Judge says so.

A reviewer or Judge that answers in another form than asked has its
answer put in form by a quick call, told why it couldn't be read, adding
and dropping nothing. Every unit of work that still comes back unusable,
whose run failed or lost its requests, or that did no work, is restarted
once, in a fresh session with the same inputs: a reviewer, a prober in a
fresh copy, a scout or one of its checks, a Judge, a first pass, a fix. A
restart that fails too is asked about at the gate, with both reasons.
Whatever a failed session used is counted with its round, and restarts
apart from the rest.

When the calls in flight can't reach their model together, an outage, the
round pauses instead of restarting each: what its units finished is kept
as they finish, the review is saved, and `acrbus acr resume` goes on with
only what didn't.

## Reviewers stay in their lane

Reviewers are told to read the code under review, not your other files,
and nothing they do is copied back to the repository. That's guidance and
structure, not a sandbox: an agent with a shell runs as you. A pi call is
held to the tools its binding names: any other, one a gateway added to the
request or one the model made up, is refused, and any call's model reaching
for a tool its binding doesn't name is said, and listed in the report. The
host's MCP servers reach a pi call only if its binding names them.

A fix keeps to its own files: the ones its blockers point at or name, test
files beside them for tests that pin it, and any a person allowed. A pi
fixer is refused a write anywhere else. Whatever agent fixes, what it
changed outside those files, a rebuilt binary or a rewritten doc, is put
back as each of its sessions ends, before any reviewer reads it, and no fix
may change that file again unless a blocker points at it. Paying to write,
review and then revert an edit nobody asked for is the waste this stops.

What a fix truly needs elsewhere, such as a caller of a signature it
changed, it notes instead, and a doc its fix left wrong it notes as stale.
Under a gate that asks at rounds, those notes, and any file the session
that ran the review changed outside its blockers, are put to you right
after the fix and before the next round's reviewers: fix it now, in a
follow-up held to that file, leave it for a ticket, or leave it. A fixer's
brief carries its files as they are, so its sessions don't spend turns
reading them again.

## It's measurable

`acrbus acr trial` reruns a review design blind on a change whose
findings an earlier review already settled, and scores what it catches. A
change to acrbus, a lens or a model choice can be measured that way, over
several trials per design, since model output varies. A key's findings
aren't every defect the change has, and trials have no single-reviewer
design and can't run your own skill: the
[FAQ](faq.md#how-do-i-measure-a-review-design) says what that leaves
open.
