---
name: review
description: Whole-change review. Two fresh reviewers from different model families, GLM on high and Opus on medium, each read the entire change at once against a standing list of cross-cutting checks. Each family then checks the other's findings, and your user decides each one. It reports; it doesn't fix. Lean on tokens. Use it to review a branch, a PR or a working tree when your user asks for a review and doesn't ask for a full ACR.
---

# Whole-change review

Two reviewers from different model families each read the whole change against a short list of checks that cut across files. Each family checks the findings the other raised, and your user decides. That's at most four model calls in two steps. It reports and doesn't fix: your user fixes by their own method, then asks for a new review.

**Why whole-change.** Many real defects live between files: the same kind of message handled on two paths that disagree, a widened helper whose other callers weren't checked, a rule enforced at the step methods but not at Start. A reviewer that reads one dimension, or one path, at a time can miss them. A reader holding the whole change, pointed at those seams, can catch them.

**Why two families.** Models from different families miss different things, so a second family is a second reading at little cost. Each finding is checked by the family that didn't raise it, because a model checking its own finding mostly ratifies it. The session that wrote the change doesn't review it, for the same reason.

## 1. Gather the change

Gather all of this with commands, not with a model.

- **Base and head:** the PR's base branch, or what your user names. With no head, review the working tree against the base.
- **The whole diff:** `git diff <base>...<head>`, or `git diff <base>` for a working tree.
- **The change's claims:** the PR's title and description, if there is one (`gh pr view <n>`). They're claims to check, not to trust.
- **The repository's own checks:** `.review/checks.md`, or a "Review checks" section in CONTRIBUTING.md. Add them to the standing checks, and pass on any non-issues the file lists.
- **A context pack:** what reviewers would otherwise spend tool calls finding. At the head:
  - for each function, type or constant the change adds or changes, its callers and references (`git grep -n -w <name>`);
  - where the change adds a case to a message kind, enum or switch, every other place that handles the same type;
  - constructors and Start, recovery or restore paths that touch the state the change touches;
  - the tests that name what changed.

  Give each hit as `file:line` with a few lines around it. Keep the pack under about 2,000 lines, and say what you cut.

## 2. Two fresh reviewers, in parallel, through the bus

Call both through bridge agents, so each starts fresh, holds only what you send it, and has what it used counted:

- **GLM on high:** `acrbus call <agent> --model <glm model> --effort high --session fresh`, to an agent that runs GLM, such as a pi bridge.
- **Opus on medium:** `acrbus call <agent> --model claude-opus-5-5 --effort medium --session fresh`, to an agent that runs Opus, such as a Claude Code bridge.

Give each only its read-only tools with `--tools`, and add `--verbose` so each call prints what it used, for the report. Use the bindings your user names instead, when they name them. If either can't be reached, ask your user whether to go on with one, and say so in the report. Don't give either reviewer your reasoning or this conversation.

Give both the same brief, with the parts they share first, in this order: the role, the checks, the context pack, the diff, then the PR's claims.

- **Role:** "You are the reviewer for this change. Find what is wrong."
- **The checks below,** plus the repository's own.
- **A budget for reading:** at most 8 read-only tool calls, only to confirm or rule out a specific suspicion. The pack and the diff are the review's main input.
- **Answer every check,** with findings, or with what you checked and found sound.
- **A defect that depends on state counts** when you give the sequence of events that triggers it.
- **The answer:** a list of findings. Each has the check it falls under, or "other"; blocker or non-blocker; `file:line`; the evidence; and whether it's a fact or a judgment call. No findings is a valid answer.

### Standing checks

1. **Symmetry across paths.** For each concept the change touches, such as a message kind, a state transition or a resource, list every path that handles it: live and replay, sync and async, success and error, start and stop. Show that they agree. Something journaled, counted, released or validated on one path must be on the others.
2. **Blast radius of a widened helper.** When the change widens what a shared function accepts or returns, check every caller, including examples, tests and secondary entry points. Check every analogous kind too: if it adds something for one kind of thing, do its siblings need it?
3. **Every entry seam.** A rule enforced at the step methods must hold at construction, Start, recovery or restore, and shutdown.
4. **Hot-path cost on unbounded data.** Flag per-step work that copies history, walks the whole configuration, or grows with run length when the step touched nothing relevant.
5. **Dead error returns and read-before-check.** Look for errors that are returned but can never be set, or are never checked, and for values read before the check that guards them.

## 3. Each family checks the other's findings

- **Merge** the two lists. Two findings at the same place about the same defect are one, found by both.
- **Send each finding to a fresh checker** from the family that didn't raise it: GLM's findings to Opus, Opus's to GLM. A finding both raised goes to Opus. That's one call per family, through the same bridges, fresh and in parallel, each given the context pack and the diff. With no findings, skip this step.
- **For each finding, the checker says,** with evidence from the code:
  - whether it's real;
  - whether it was there before the change. One that was never blocks;
  - whether it's a blocker.
- **Not defects:** what a compiler, linter or type checker would catch; style a senior engineer wouldn't raise; a change of behavior the PR says it intends; anything the change neither caused nor exposed.
- **Nothing is dropped.** Every finding goes to your user with the checker's verdict.

## 4. Your user decides

- Read each finding's cited lines, so you can say what it points at. Don't drop or downgrade a finding yourself, and don't fix anything.
- Ask your user about each blocker with AskUserQuestion: **real**, **ticket**, or **not a defect**. Give the finding, who found it and the checker's verdict. Group non-blockers into one question, and the findings the checker called not real into another. Never answer for your user.
- Fixing is your user's, by their own method. When they've fixed, they ask for a new review. Give that review the earlier findings and their verdicts, so it says which blockers still stand and doesn't raise again what was ruled not a defect.

## Report

Give your user:
- the change reviewed;
- what was found, by check, with who found each finding and the checker's verdict;
- each finding's verdict (real, ticket, not a defect), and who gave it;
- for a review after fixes, which earlier blockers still stand;
- the model and effort each reviewer and checker actually ran on, and the tokens each used;
- what wasn't checked.

Label reviewers and checkers as AI agents, for example "Reviewer (AI agent), GLM".
