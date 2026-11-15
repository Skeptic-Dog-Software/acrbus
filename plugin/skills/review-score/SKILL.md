---
name: review-score
description: Score a finished review against a key of defects known some other way, such as a human's review, a deeper in-session review, or later bug fixes. A fresh scorer matches each known defect to the review's findings (caught, partial or missed), checks the review's other findings, and your user settles what's disputed. Use it after a review when your user wants to know what it missed, or to compare two review setups on the same change.
---

# Score a review against a key

A review's findings say what it caught, but not what it missed. This compares a finished review, or several, with a **key**: defects in the same change found some other way. It reports what was caught, half caught and missed, which of the review's other findings are real, and which lens or check should have caught each miss.

## 1. Gather the review and the key

- **The review:** its findings, each with an id, lens, check, `file:line` and a one-line summary, and the head it reviewed. Take them from the review's report in this session, or wherever your user points.
- **The key:** each known defect, with an id, `file:line` and what's wrong. It can come from:
  - a human's review comments on the PR (`gh api repos/<owner>/<repo>/pulls/<n>/comments`);
  - a deeper review your user ran, such as an in-session ACR;
  - a later commit that fixed a bug the change introduced.

  Keep only defects that exist at the head the review saw, and say which you dropped and why.
- **Several reviews of the same change** are scored together, under neutral labels (A, B, C), so the scorer can't tell which setup made which.

## 2. A fresh scorer matches them

Use one fresh agent that took no part in any of the reviews: a subagent with a clean context, or a bus agent your user names, from another model family than the reviewers where you can. Brief it with the key, the findings under their labels, and the diff. Tell it:

- **For each key defect, per review:**
  - **caught:** a finding names the same defect, at its place or by its cause;
  - **partial:** a finding is at the right place but gives a wrong or incomplete cause, or gives the cause but misses a path where it happens;
  - **missed:** nothing names it.

  Cite the finding id and say why.
- **For each finding that matches no key defect:** **real**, checked against the code with `file:line`; **noise**; or **unverified**, when it can't tell without running something.
- **For each miss:** which lens or standing check should have caught it, or **none**, when no lens or check covers that kind of defect.
- Read the code to settle a match, but don't run the change.

## 3. Your user settles what's disputed

- Read the cited lines for each partial match, each finding the scorer called real, and each miss, so you can say what they point at.
- Ask your user with AskUserQuestion about every partial match and every real or unverified extra: confirm, or change the call. Group them so it's a few questions, not one per finding. Never answer for your user.
- A real extra your user confirms is a defect the key lacked. Add it to the key, so the next score counts it.

## 4. Report

- **For each review:** caught, partial and missed, out of the key's size; real extras and noise; its cost and time, if you have them; and the cost per defect caught.
- **Each key defect, by id:** what each review did with it.
- **The misses, by the lens or check that should have caught them,** and those that no lens or check covers. These are what a recalibration should take up.
- **What was dropped from the key** as not at the head, and why.
- The scorer's model, and what you settled with your user.

Label the scorer as an AI agent, for example "Scorer (AI agent)". Say only what the score covers: one change is evidence about that change, not a rate.

When findings or key text can't leave this machine, send only ids, counts and calls to anyone else.
