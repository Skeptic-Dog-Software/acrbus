---
name: review
description: Whole-change review. One fresh reviewer reads the entire change at once, against a standing list of cross-cutting checks, and your user decides each finding. Lean on tokens. Use it to review a branch, a PR or a working tree when your user asks for a review and doesn't ask for a full ACR.
---

# Whole-change review

One reviewer, the whole change, a short list of checks that cut across files, and your user decides. It costs one reviewer call per pass, not a fan-out per dimension.

**Why one reader.** Many real defects live between files: the same kind of message handled on two paths that disagree, a widened helper whose other callers weren't checked, a rule enforced at the step methods but not at Start. A reviewer that reads one dimension, or one path, at a time can miss them. One reader holding the whole change, pointed at those seams, catches them. A separate session still reviews, because the session that wrote the change mostly ratifies its own work.

## 1. Gather the change

- Find the base and head: the PR's base branch, or what your user names. With no head, review the working tree against the base.
- Get the whole diff: `git diff <base>...<head>`, or `git diff <base>` for a working tree. Get the PR's title and description if there is one (`gh pr view <n>`); they're the change's claims about itself, to be checked, not trusted.
- Look for repository checks of its own: `.review/checks.md` or a "Review checks" section in CONTRIBUTING.md. Add them to the standing checks.

## 2. Ask one fresh reviewer

Use a reviewer your user has named, such as a bus agent on a cheaper model (`acrbus call` / `tailbus call <agent>`). Otherwise use one subagent with a clean context. Don't give it your reasoning or this conversation. Brief it with exactly this:

- **Role:** "You are the reviewer for this change. Find what is wrong."
- **The whole diff, in the prompt,** with the PR description marked as claims.
- **The checks below,** plus the repository's own.
- **A budget for reading:** at most 8 tool calls, only to confirm or rule out a specific suspicion. The diff is the review's main input.
- **The answer:** a list of findings. Each has the check it falls under, or "other"; blocker or non-blocker; `file:line`; the evidence; and whether it's a fact or a judgment call. No findings is a valid answer.

### Standing checks

1. **Symmetry across paths.** For each concept the change touches, such as a message kind, a state transition or a resource, list every path that handles it: live and replay, sync and async, success and error, start and stop. Show that they agree. Something journaled, counted, released or validated on one path must be on the others.
2. **Blast radius of a widened helper.** When the change widens what a shared function accepts or returns, check every caller, including examples, tests and secondary entry points. Check every analogous kind too: if it adds something for one kind of thing, do its siblings need it?
3. **Every entry seam.** A rule enforced at the step methods must hold at construction, Start, recovery or restore, and shutdown.
4. **Hot-path cost on unbounded data.** Flag per-step work that copies history, walks the whole configuration, or grows with run length when the step touched nothing relevant.
5. **Dead error returns and read-before-check.** Look for errors that are returned but can never be set, or are never checked, and for values read before the check that guards them.

## 3. Your user decides

- Read each finding's cited lines, so you can say what it points at. Don't drop or downgrade a finding yourself: you wrote the change.
- Ask your user about each blocker with AskUserQuestion: **fix now**, **ticket**, or **not a defect**. Group non-blockers into one question. Never answer for your user.

## 4. Fix only what was chosen

- Change only the files a chosen finding cites, and tests beside them that pin the fix.
- When a fix makes a doc stale, or needs a file outside that scope, don't edit it. List it as "file:line now says X" or "needs file: why", and ask.
- Don't write build outputs into the tree. Build to a temp path, or just run the tests.
- Run the tests that cover what you changed.

## 5. Check the fixes, once

Ask one fresh reviewer, with the same brief, on the fix's diff only: did each fix hold, and did it break any of the checks? Bring what it finds to your user the same way. Stop there unless your user asks for another pass.

## Report

Give your user:
- the change reviewed;
- what each pass found, by check;
- what was fixed, ticketed or ruled not a defect, and by whom;
- the reviewer's model;
- what wasn't checked.

Label the reviewer as an AI agent, for example "Reviewer (AI agent)".
