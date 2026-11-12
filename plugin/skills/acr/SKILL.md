---
name: acr
description: Adversarial convergence review (ACR). Clean-context reviewers attack a piece of work one dimension at a time, and you fold their findings in, round by round, until nothing blocks. Use it for the review step of a /pipe task, or whenever your user asks for an ACR.
---

# ACR: adversarial convergence review

This method comes from Ray Karnes's essay "The Adversarial Agent Fleet: Trust, but Verify" (24 May 2026), and from the review process he runs on his repositories, which follows it.

**Why it works this way.** An agent pursues whatever framing it's handed, all the way to a confident conclusion, and its confidence reads the same whether that framing was sound or not. Asking the agent that did the work to check it mostly ratifies what it concluded, because the same disposition that produced the work is now grading it. The opposition has to come from outside, and you get it by structure, not by prompting.

## Roles

- **You orchestrate and synthesize.** You did the work, so you're the agent with the most context in the system. That's why you never attack it.
- **Reviewers attack.** Each is a fresh subagent with a clean context. It sees the artifact, its dimension's criteria and the scope, and nothing else: not your reasoning, not the conversation that produced the work, and not earlier rounds' rebuttals. A reviewer that inherited your framing would just be you grading yourself. For work that turns on judgment, a reviewer sees the claims, not the case made for them.

## The method

1. **Dimensions.** Use the ones your user chose. Write down each one's criteria: what "wrong" means for this task in that dimension. One reviewer per dimension per round.
2. **Build the strongest version first.** Before any attack, make the work the version its most capable advocate would stand behind. An attack that survives a strong version tells you something; one that knocks down a weak version tells you nothing.
3. **Run a round.** Start one reviewer per open dimension with the Agent tool, passing the task's `model` and `effort` for each run. Default to your own model at `xhigh`, which keeps every reviewer at least as capable as you. Claude Code drops the Agent tool's `model` setting while `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` is on, and then every subagent runs on the forced model.
   - Run **at most 5 reviewers at once.** Start more as slots free up.
   - Brief each with its dimension, the criteria, the scope, and the artifact itself, such as the diff, the document or the answer.
   - Give the instruction **"Find what is wrong"**, never "check this", which invites ratification. Name its role, for example "the reviewer for correctness".
   - Ask for findings, each marked **blocker** or **non-blocker**, with evidence. Ask it to say when a finding is a judgment call rather than a fact.
4. **Synthesize.** Fold each blocker into the work, making it stronger. When you reject a finding, record why. That rejection is a disagreement, not a resolution.
5. **Attack again.** For each dimension that had blockers, start a fresh reviewer, with a clean context again, against the revised work.
6. **Converge.** A dimension is **confirmed** when its latest round has zero blockers.
7. **At most 3 rounds.** If a dimension still has blockers after round 3, stop and ask your user. In a /pipe task, use `pipe_questions` with these choices:
   - one **verify-only** round, in which only the earlier findings can block;
   - **waive** that dimension;
   - **stop** here.
   Wait for the answer.

## Disagreement is output

When reviewers split, when a finding is arguable rather than obvious, or when you reject a finding, record all of it: the finding, each side's case, and that it's your user's call. These are values calls, such as which failure mode is unacceptable, or speed against defensibility. The structure surfaces them, and it doesn't decide them, so don't settle them yourself. The record of where reviewers split is the most honest map of where the work is weak. Keep it, and don't tidy it away.

## The report

For `pipe_result`, or for your user:

- **For each dimension:** its criteria, each round's blockers and non-blockers, how each blocker was folded in, and whether it's confirmed or waived, or still open and why.
- **Disagreements:** each one, with both sides, marked as your user's call. Or say there were none.
- **What ran:** the model and effort the reviewers actually ran on. Claude Code can substitute a model the organization blocks, or use a lower effort than asked for.
- **What it cost:** the report's `by_role`, each round's calls by role, with fresh, cached and output tokens and dollars, as it gives them.
- Label reviewers as AI agents in roles, for example "Reviewer (AI agent), correctness", so nobody reading the report takes them for people.
- **Don't overclaim.** Say only what each review actually covered.

## Cost

This costs two to three times the tokens of working alone, and takes longer. It's spent on purpose where being wrong is expensive. Your user may waive it for something trivial, but it's never yours to skip.

## When acrbus runs it

When your user has acrbus set up for the repository (`acrbus acr doctor --repo .` says it's ready) and wants the review run by it, acrbus starts the reviewers and the Judge. You do what the builder does in the method above: brief them, bring every question to your user, and fold in what they choose.

1. **Brief the reviewers.** Write the builder's brief to a file, in YAML: `scope:`, what the change does, mechanism by mechanism, and what it doesn't touch; and `lenses:`, a map from each lens to what to attack in this change, and how: what to run, what to break, which edges and callers to check. Every role reads your scope as claims to check, not to trust.
2. **Start it in the background.** Run `acrbus acr run --repo . --base <base> --head <head> --brief <file>`. It says what it's doing on stderr, and in `progress.log` in its folder, at least once a minute: every call at work, how long it has run, and when it last did anything. A quiet stretch isn't a hang. A call that stalls is ended by its bridge, and the progress says when.
3. **Every question goes to your user.** When it exits 3 or 4, run `acrbus acr ask <id>`. Show every carousel to your user, each with one AskUserQuestion call, exactly as given and in order: a carousel holds four questions at most, and a review can ask more. Then pass all their answers, from every carousel, keyed by each question's text (or by `[header] text`), with `acrbus acr resume <id> --answers '<json>'`, in the background. A question left unanswered, or an answer for no question, is refused: show the refusal's questions to your user and pass their answers. Never answer for your user, and don't drop or reword a question. While a review runs, `acrbus acr ask <id>` says where each lens stands instead.
4. **Fold in what they choose.** When it waits for fixes, `acrbus acr ask <id>` lists them, with your user's notes. Fix each one in the working tree, with your context. Write tests that pin the fixes. Brief the next round on what changed, in the same format, then run `acrbus acr resume <id> --fixed --brief <file>`.
5. **At the end,** exit 0 means confirmed. Report as below, from what it printed and `acrbus acr feedback <id>`.

With a Grayboard team server set up (`acrbus grayboard status`), a review waiting for answers also sends its questions to your user's phone, and their reply there resumes it, in the background. Before you ask, run `acrbus acr ask <id>`: if the review is no longer waiting, they've answered already. When their answers choose fixes for you, acrbus sends you a message saying so, with the `acrbus acr resume <id> --answers @<file>` that applies them: run it, then fold in the fixes as in step 4.
