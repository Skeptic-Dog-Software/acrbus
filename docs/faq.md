# acrbus FAQ

Questions and answers about what acrbus does, how and why, for people
deciding whether to use it and for agents checking what it claims. Every
answer ends with one line saying where to check it: `Code:` as
`path::symbol` (`grep -w <symbol> <path>`), `Config:` keys and environment
variables, `CLI:` commands and flags, and `Tests:` test functions
(`grep -rn "fn <name>" src`).

## What acrbus is

### What is acrbus?

An orchestrator for adversarial convergence review (ACR) of code changes.
Fresh reviewers attack a change one dimension at a time, a separate Judge
checks what they find against the code, fixed rules decide what blocks, the
blockers you choose are fixed, and new reviewers attack the fixed version.
acrbus reviews nothing itself: every reviewer, Judge, Sizer and Orchestrator
is a model running headless in a harness, and acrbus runs what's around them.

Code: src/acr/flow.rs::drive; src/acr/flow.rs::review_round; src/acr/flow.rs::ruling | CLI: acrbus acr run | Tests: a_clean_change_is_confirmed_in_one_round, only_lenses_with_blockers_are_attacked_again_by_new_reviewers

### What are a harness, a bridge and a binding?

A harness is an agent CLI, such as Claude Code, pi or omp, that runs a model
with tools. A bridge runs one harness headless, once per request, and sends
back what it prints. A binding names a bridge's agent with a model, an
effort and a set of tools, and a review's config says which binding each
role uses.

Code: src/bridge.rs::Settings; src/acr/config.rs::Binding; src/acr/config.rs::Role | Config: allowed_bindings, defaults.bindings | Tests: a_request_with_no_options_runs_as_configured, a_call_asks_for_its_bindings_tools_unchanged_unless_it_asks_for_none

### Which roles does a review have?

- **Sizer:** picks the change's tier, and does small housekeeping calls,
  such as grouping one defect several lenses found.
- **Orchestrator:** chooses lenses a description triggers, writes per-lens
  briefs, and, when its binding may write, fixes blockers.
- **Reviewer:** attacks the change in one lens.
- **Judge:** checks each finding against the code and states facts about it.
- **Configurer:** outside any review, proposes a repository's config. Without
  a binding of its own, it uses the Judge's.

Code: src/acr/config.rs::Role; src/acr/flow.rs::DEDUPE; src/acr/flow.rs::choose_lenses | Config: defaults.bindings | Tests: the_configurer_has_its_own_binding_or_the_judges, every_session_is_told_its_role

### Does acrbus run a server or open ports?

No. Its daemon listens on a Unix socket on one machine, and the harnesses
reach their models the way they always do. The one exception is `acrbus
grayboard login`, which opens a loopback port on 127.0.0.1 for the
sign-in's redirect while you sign in.

Code: src/daemon.rs::run; src/network.rs::Local; src/grayboard.rs::login | CLI: acrbus grayboard login | Tests: a_local_bus_needs_no_tailscale_and_reaches_only_this_machine, acrbus_is_local_by_default_and_names_the_networks_it_knows

### Which platforms does it run on?

macOS and Linux. It installs its daemon as a launchd agent on macOS and a
systemd user unit on Linux, and checks a door's caller by the peer's user ID,
which both provide. Windows isn't supported.

Code: src/setup.rs::LaunchAgent; src/setup.rs::UserUnit; src/daemon.rs::peer_uid | CLI: acrbus service install

### Does acrbus choose models, or know about my language?

Neither. Every model comes from your bindings, and everything about a
repository, from its lenses to its test command, comes from that
repository's config. Adding a language needs no change to acrbus.

Code: src/acr/config.rs::Binding; src/acr/config.rs::Config | Config: allowed_bindings, lenses, tests.command | Tests: the_framework_chooses_no_model, the_framework_knows_nothing_about_any_repository, a_repository_in_another_language_needs_no_framework_change

## Compared with other approaches

### How is this different from a single-pass review skill?

A single-pass skill is one agent with a scoped prompt. In one context, the
same model finds a problem, judges it, decides when it's done, and reports
whether it ran the tests. acrbus holds each of those apart in code:

- a fresh session for each lens's reviewers, sharing nothing;
- a separate Judge, whose facts fixed rules turn into a ruling;
- new reviewers attacking again after a fix, round by round;
- the change's tests, coverage and mutants run by acrbus itself, where its
  stages are on.

A skill is cheaper, needs nothing installed and adapts to each change. ACR
is for changes where a miss costs more than the review.

Code: src/bridge.rs::SessionMode; src/acr/flow.rs::rate; src/acr/flow.rs::ruling; src/acr/stages.rs::run_gate | Config: stages | Tests: fresh_sessions_share_nothing, rules_decide_what_blocks_from_the_judges_facts, the_changes_tests_are_run_other_ways_and_a_failure_is_a_finding

### acrbus ships an ACR skill. How does it relate?

The skill is the method, written for a Claude Code session. Without acrbus,
the session runs it with subagents. With acrbus set up, the session plays
the builder: it writes the brief, starts `acrbus acr run`, brings every
question to you, and fixes what you choose when the review leaves fixes to
it. acrbus runs the reviewers, the Judge and the rules.

Code: plugin/skills/acr/SKILL.md; src/acr/flow.rs::Fixer | CLI: acrbus acr run --brief, acrbus acr ask, acrbus acr resume --answers, acrbus acr resume --fixed | Tests: the_builder_briefs_round_one_and_the_steelman_the_next

### How is this different from a workflow designer?

A workflow designer models a task, composes a workflow of steps for that
instance, and exports it; it doesn't run it. acrbus runs one kind of step,
adversarial review, with a fixed method. The steps, their order and the
rules are in code. What a repository's config changes is what each step
looks at and how hard. The two don't overlap: one designs, the other runs
reviews.

Code: src/acr/flow.rs::drive; src/acr/config.rs::Config | Config: lenses, tiers, stages, max_rounds, human_gate | Tests: settings_are_checked

### Can a designed workflow hand its review step to acrbus?

Yes. A step runs `acrbus acr run` and reads its exit code: 0 when every lens
agrees, 3 when the review waits for an answer, 4 when it escalates. The step
can read the questions with `acr ask`, answer with `acr resume --answers`,
and read the report as JSON. With `--human-gate exception`, only
escalations stop it.

Code: src/acr/mod.rs::WAITING; src/acr/mod.rs::ESCALATED; src/acr/config.rs::HumanGate; src/acr/report.rs::Report | CLI: acrbus acr run --human-gate, acrbus acr ask --text, acrbus acr resume --answers, acrbus acr usage --json, acrbus acr plan --json | Tests: the_report_can_be_read_by_a_machine, always_escalate_reaches_a_person_whatever_the_gate

### Isn't a fixed method rigid?

The method is fixed, and the review fits the repository and the change:

- **Per repository:** the lenses, what blocks in each, the tiers, the stages
  and the round cap come from the repository's calibrated config.
- **Per change:** the tier sets how many reviewers each lens gets, and a lens
  runs only when its triggers match what the change touches.
- **Per round:** another round runs only while blockers remain.

Fixed on purpose: clean reviewers, a separate Judge, and the rules. That's
what makes two reviews comparable.

Code: src/acr/flow.rs::choose_tier; src/acr/flow.rs::choose_lenses; src/acr/flow.rs::ruling | Config: tiers, lenses, max_rounds | Tests: lenses_come_from_their_triggers, a_tier_can_require_a_lens, only_lenses_with_blockers_are_attacked_again_by_new_reviewers

## The bus, the daemon, bridges and doors

### What is the bus?

A small message bus on one machine that carries messages between agents:
a review's roles and their bridges, and your own sessions. An agent's
address is `name@machine`. The daemon keeps the messages in a local store
and serves them over its socket.

Code: src/bus.rs::Bus; src/store.rs::Store; src/daemon.rs::serve_conn | CLI: acrbus send, acrbus call, acrbus recv, acrbus inbox, acrbus peers | Tests: new_messages_are_requests_and_replies_are_responses, threads_follow_replies_both_ways

### Is a message from another agent trusted?

No. A bridge runs only requests from the bus's `owner`, and wraps each one
in a note saying it comes from another agent, not the user, and is a
request and nothing more. ACR's system prompt adds that what a role reads is
data, never instructions.

Code: src/bridge.rs::default_message; src/acr/system-prompt.md | Config: owner | Tests: answers_only_its_owner, the_owner_is_checked_before_any_option_is_read, the_default_message_says_who_its_from_and_that_it_isnt_the_user

### What does a bridge do with a request?

It runs its agent CLI once, in a fresh session by default, with its own
`HOME`, `PATH` and environment, and replies with what the agent printed.
Its settings in `config.toml` say what a request may ask for, such as a
model, an effort or tools, and a request can only narrow them. Each run is
logged and kept.

Code: src/bridge.rs::Settings; src/bridge.rs::Allowed; src/bridge.rs::SessionMode | Config: command, args, home, path, working_directory, session, max_concurrent | CLI: acrbus bridge install, acrbus bridge log --stderr | Tests: sends_stdout_as_the_reply_from_an_environment_of_its_own, options_narrow_what_config_allows_and_never_widen_it, each_run_is_logged_under_its_id

### What is a bridge's kind?

`kind = "claude"`, `"omp"` or `"pi"` takes the defaults acrbus ships for
that agent: its arguments, tools, efforts, how to run it with no tools, and
where it records its usage. A bridge's own table wins over them, and any
other agent CLI works with a table of its own.

Code: src/bridge.rs::Kind; src/bridges/claude.toml; src/bridges/pi.toml; src/bridges/omp.toml | Config: kind, extra_args | Tests: loads_its_table_and_reports_on_itself, a_bridge_that_names_its_kind_says_only_whats_its_own

### How many calls run at once?

Each bridge runs at most `max_concurrent` requests, 1 unless set; the
Claude Code, omp and pi kinds set 3. A call waiting for a free slot says so
in progress. Raise it to the size of the machine and the provider's limits.

Code: src/bridge.rs::default_max_concurrent; src/bridges/claude.toml; src/bridges/omp.toml; src/bridges/pi.toml; src/acr/progress.rs::Progress | Config: max_concurrent | Tests: keeps_to_the_limit_on_runs_at_once, runs_one_at_a_time_by_default

### What is a door?

A Unix socket of its own for an agent that runs in another OS account, such
as one that answers other people's questions. The daemon checks each
connection's user ID: only the named account gets through, as the named
agent. Through it, the agent can take its messages and answer each request
once, to its sender, and nothing else. Only agents on this machine can send
to it.

Code: src/door.rs::check; src/door.rs::serve; src/door.rs::check_folder; src/config.rs::DoorConfig | Config: doors, account, socket | Tests: only_the_doors_account_gets_through, it_starts_nothing_and_asks_nothing, it_answers_only_open_requests_to_it, other_machines_cant_send_to_a_door_agent, the_main_socket_cant_act_as_a_door_agent

### Can the bus reach other machines?

Not on its own: acrbus is local. A program built on acrbus can add a network,
as tailbus adds one over Tailscale. A review only uses bindings on its own
machine.

Code: src/network.rs::Kind; src/config.rs::App; src/acr/mod.rs::check_machine | Tests: a_local_bus_is_its_configured_machine_with_no_peers, every_binding_must_be_on_the_machine_with_the_code, a_review_runs_only_for_this_machine

## How a review runs

### What happens in a review, step by step?

1. The config is read from outside the change, and the repository must be
   calibrated.
2. The Sizer picks a tier, and the triggered lenses are chosen.
3. You approve the scope, if the human gate asks.
4. Stages gather evidence, such as the test gate, and the change map is
   built.
5. Each lens's reviewers attack, in fresh sessions.
6. Each lens's Judge rates its findings, and the rules rule. Findings
   that are one defect are then grouped, to be decided once.
7. You decide each blocker; the chosen ones are fixed.
8. New reviewers attack the fixed version, until no lens has a blocker or
   `max_rounds` is reached.

Code: src/acr/flow.rs::drive; src/acr/flow.rs::Stage; src/acr/flow.rs::Gate | Config: human_gate, max_rounds, stages | Tests: a_person_in_the_loop_approves_the_scope_decides_each_blocker_and_the_result, a_clean_change_is_confirmed_in_one_round

### What is a lens?

One dimension of review, such as correctness or regression safety. It has
`criteria` (what's wrong in this lens), `attack` (techniques that find it),
`blocker` (what blocks here, set at calibration), and `triggers` (which
changes it reviews). Every repository also gets a built-in `elegance` lens,
which its config can reword but not remove.

Code: src/acr/config.rs::Lens; src/acr/config.rs::Triggers; src/acr/config.rs::ELEGANCE | Config: lenses, criteria, attack, blocker, triggers | Tests: every_repository_gets_elegance_worded_its_own_way_if_it_likes, a_finding_blocks_by_the_lenss_definition_only_quoting_it

### What are tiers, and what does the Sizer do?

Tiers, ordered lowest to highest, each set `fan_out`, how many reviewers
each lens gets, and can require lenses. When a tier's path patterns settle
it, no model is asked. Otherwise the Sizer chooses from the file list, the
lines each file changes and what the change says it's for, with no tools; it
reads the change with its tools only when that isn't enough. Unsure means
the higher tier, and a failed Sizer means the highest.

Code: src/acr/config.rs::Tier; src/acr/flow.rs::choose_tier; src/acr/roles.rs::sizer_light_prompt; src/acr/plan.rs::highest | Config: tiers, fan_out, required | Tests: a_path_tier_needs_no_sizer, the_sizer_decides_from_the_file_list_with_no_tools, the_sizer_reads_the_change_only_when_the_file_list_cant_tell, an_unsure_sizer_means_the_higher_tier

### Which lenses run on a change?

Those whose triggers match: by path patterns, by content patterns, or by a
plain-language description the Orchestrator judges. A tier can require a
lens. Each round checks triggers again on the change as it is then, so a
lens a fix now needs joins, and none leaves. If the Orchestrator can't say,
every candidate stays in and you're told why.

Code: src/acr/flow.rs::choose_lenses; src/acr/config.rs::Triggers | Config: triggers.paths, triggers.content, triggers.description, required | Tests: lenses_come_from_their_triggers, a_lens_only_a_description_triggers_needs_the_orchestrator, a_lens_a_later_version_of_the_change_needs_joins_and_none_leaves

### What does a reviewer see?

The change, its lens's criteria, attack and blocker, the round's shared
facts and test gate results, the decided non-issues, the brief for its
lens, and, if it can run things, the runbook. It doesn't see the reasoning
that built the change, other reviewers' conclusions, or earlier rounds'
findings. What the change says about itself reaches it as claims to check.

Code: src/acr/roles.rs::reviewer_prompt; src/acr/roles.rs::ReviewerCall; src/acr/stages.rs::claims | Config: lenses | Tests: every_role_gets_what_the_change_says_its_for_ahead_of_the_diff, a_brief_holds_what_a_reviewer_needs_and_only_that, fresh_sessions_share_nothing

### What does the Judge do?

It checks each finding against the code and states facts about it: is it
relevant and real, was it there before the change, does the change claim to
fix it or make it worse, does a new test pin nothing, does a claim mislead,
does it meet the lens's blocker definition. It starts from the diff hunks
its findings point at, keeps every tool its binding gives, and can't change
the checkout. A lens is judged as soon as its own reviewers finish.

Code: src/acr/roles.rs::judge_prompt; src/acr/roles.rs::Rating; src/acr/change.rs::hunks_for; src/acr/flow.rs::rate | Config: facts.judges | Tests: a_judge_gets_the_hunks_its_findings_point_at, a_judge_gets_the_diff_and_may_not_change_the_checkout, a_lens_is_judged_as_soon_as_its_own_reviewers_finish

### Why a separate Judge, and not the reviewers' own ratings?

A reviewer is there to find, and one that rates its own findings grades
its own case. The Judge didn't make the case: it checks each finding
against the code, states facts, and the rules, not its wording, decide what
blocks. A real finding outside the reviewer's own lens is rated like any
other, and noise is dropped.

Code: src/acr/flow.rs::rate; src/acr/flow.rs::ruling; src/acr/roles.rs::judge_prompt | Tests: rules_decide_what_blocks_from_the_judges_facts, a_real_finding_another_lens_owns_is_rated_like_any_and_noise_is_dropped, a_judge_holds_a_lens_blocking_condition_and_false_claims

### What rules decide what blocks?

Fixed code turns the Judge's facts into a ruling:

- not relevant, not real, or a change that changes nothing: dropped;
- there before the change, neither claimed fixed nor made worse: logged,
  never blocking, and a Critical one goes to you;
- raised to High at least: a weak pin, a misleading claim, a pre-existing
  defect the change claims to fix or makes worse, and a defect of the
  change's that meets its lens's blocker definition;
- anything else keeps the Judge's severity.

A fact resting on the change's own words holds only when the Judge quotes
them from the diff.

Code: src/acr/flow.rs::ruling; src/acr/flow.rs::grounded; src/acr/flow.rs::rules_source | Tests: rules_decide_what_blocks_from_the_judges_facts, every_combination_of_facts_has_one_ruling_and_none_hides_a_defect, a_weak_pin_or_misleading_claim_holds_only_with_a_quote_the_change_adds

### What does each severity do?

Severities are the same in every repository. By default Critical and High
block, Medium and Low are logged, and Rejected is dropped. A lens's
`severity` map can change that for its own findings, for example to make a
Medium block unless you waive it.

Code: src/acr/config.rs::Severity; src/acr/config.rs::Action; src/acr/config.rs::default_action | Config: lenses.severity | Tests: severities_do_what_they_say, severity_means_the_same_everywhere, a_lens_can_change_what_a_medium_does, a_medium_a_lens_makes_block_can_be_waived

### What happens to a defect several lenses report?

One small call on the Sizer's binding groups the findings that are one
defect, so you decide it once and it's fixed once. Pairs at different places
that name the same identifier, such as a function or field, are put to that
call as candidates; the call still decides. Merged, the question names every
lens and place, and the fixer is told to fix each place. If the call fails,
nothing is grouped, and progress says so.

Code: src/acr/flow.rs::same_defects; src/acr/flow.rs::identifiers; src/acr/flow.rs::DEDUPE | Tests: one_defect_two_lenses_found_is_decided_once_and_fixed_once, one_defect_two_lenses_found_at_two_places_is_one_blocker_with_both_places, a_shared_common_word_or_different_names_make_no_candidate, a_pair_the_dedupe_rules_different_stays_two_blockers, findings_go_ungrouped_and_say_so_when_the_grouping_fails

### What if a reviewer answers in the wrong format?

A report written in prose is put into the asked-for form by a quick call on
the Sizer's binding, which adds and drops nothing, and is counted as a
repair. A report that still can't be read is an error, never an empty
result. A binding whose answers keep needing repair is named in progress.

Code: src/acr/flow.rs::findings_of; src/acr/roles.rs::reformat_prompt; src/acr/flow.rs::REPAIR_WARN_SHARE | Tests: a_reviewer_that_reports_in_prose_is_put_in_form_not_failed, an_unreadable_answer_is_an_error_not_an_empty_result, a_binding_whose_answers_keep_needing_repair_is_said_in_progress

### When does `acr run` end?

When every lens's latest round has no blocker, and you approve the result if
the human gate asks. After a fix, only the lenses that had blockers attack
again, with fresh reviewers. Every lens attacks again when you choose
"Another round" at the result, or report a defect the reviewers missed.
`max_rounds`, 3 by default, caps the rounds: a lens still blocked there is
put to you, to run one more round or stop.

Code: src/acr/flow.rs::settle; src/acr/flow.rs::reopen_all; src/acr/config.rs::framework_defaults; src/acr/flow.rs::About | Config: max_rounds | CLI: acrbus acr run --max-rounds | Tests: only_lenses_with_blockers_are_attacked_again_by_new_reviewers, a_blocker_at_max_rounds_stops_and_escalates

### What is a review to convergence?

The fuller form of the method: after each round with a blocker, a steelman
folds the upheld blockers in, a test writer pins the fixes where that stage
is on, and the lenses attack again. Before it can end, a final sweep runs
every lens that hasn't attacked the latest version, so it ends only when one
round on one version has no blocker in any lens. It runs as `acr trial --arm
converge`, with nobody in the loop, capped at 20 rounds; `acr run` doesn't
use it.

Code: src/acr/flow.rs::Steelman; src/acr/flow.rs::settle; src/acr/mod.rs::CONVERGE_CAP; src/acr/mod.rs::pipeline_run | CLI: acrbus acr trial --arm | Tests: a_review_to_convergence_attacks_every_lens_after_each_steelman, after_a_fix_only_what_it_touched_attacks_then_a_sweep_covers_the_rest

### What is the steelman?

In a review to convergence, the Orchestrator, on the binding named
`steelman` in your `acr.yaml`, synthesizes between rounds: it folds in the
blockers the Judge upheld, and records the ones it declines, with its
reason, as disagreements. It holds the change's thread, so it never attacks,
and it briefs the next round's lenses. Its binding must be able to write.

Code: src/acr/roles.rs::steelman_prompt; src/acr/mod.rs::STEELMAN_BINDING; src/acr/flow.rs::Steelman | Config: allowed_bindings | Tests: a_steelman_folds_only_blockers_and_its_declines_are_disagreements, the_builder_briefs_round_one_and_the_steelman_the_next

### What does a review cover: the whole pull request or one commit?

`review_scope: whole-pr`, the default, reviews all of base..head as one
diff, which is what merging puts on the base. `bottom-of-stack` reviews only
the first commit after the base, for stacks whose pull requests merge one
by one.

Code: src/acr/config.rs::ReviewScope; src/acr/change.rs::bottom_of_stack | Config: review_scope | CLI: acrbus acr run --review-scope | Tests: reads_a_change_whole_or_from_the_bottom_of_its_stack, a_stacks_review_is_of_its_bottom_commit_which_must_be_checked_out

### What happens to generated files?

They're left out of the diff a review sends, each listed with its line
counts, and reviewers can still read them in the repository. A lens can ask
for them with `include_generated`, and the config can add or keep paths.

Code: src/acr/config.rs::Generated; src/acr/change.rs::leave_out_generated | Config: generated.exclude, generated.paths, generated.keep, include_generated | Tests: generated_files_are_left_out_and_listed, a_lens_that_takes_generated_files_gets_the_whole_diff, conventions_name_generators_output_and_nothing_hand_written

### Can it review work that isn't code?

Yes, once it's carried into git. `acr carry` commits a version before the
work, if there is one, and the version to review, as text files, to a
carrier repository in acrbus's state. Configure the carrier with that work's
own lenses, like any repository. Two exports of one version must match, or
every diff is noise.

Code: src/acr/carry.rs::carry; src/acr/carry.rs::carrier | CLI: acrbus acr carry --name, acrbus acr carry --base, acrbus acr carry --head | Tests: a_documents_two_versions_become_a_change_git_can_diff, with_no_earlier_version_the_whole_work_is_new_and_folders_keep_their_paths, a_key_for_work_that_isnt_code_is_carried_into_git

## The builder's brief

### What is the builder's brief?

A YAML file from the session that built the change: a `scope` statement,
what the change does mechanism by mechanism and what it doesn't touch, and
for each lens, where and how to attack. Pass it with `acr run --brief`.
Every role gets the scope as claims to check, and each reviewer gets its
own lens's part.

Code: src/acr/stages.rs::BuilderBrief; src/acr/stages.rs::read_builder_brief | CLI: acrbus acr run --brief | Tests: the_builder_briefs_round_one_and_the_steelman_the_next, a_brief_holds_what_a_reviewer_needs_and_only_that

### Doesn't the builder's brief break the rule that a worker doesn't check its own work?

No. The brief gives direction, not judgment: where to look and how to
attack. The builder never reviews, judges or rules on its own change, and
its scope reaches every role as claims to check. Without a brief, reviewers
tend to go off on tangents; the builder knows best where its change could be
wrong.

Code: src/acr/stages.rs::BuilderBrief; src/acr/roles.rs::reviewer_parts; src/acr/flow.rs::rate | Tests: every_role_gets_what_the_change_says_its_for_ahead_of_the_diff, a_brief_holds_what_a_reviewer_needs_and_only_that

### What if there's no brief, or it leaves a lens out?

With the `brief` stage on, the Orchestrator writes each uncovered lens a
brief for this change, as questions to settle, not conclusions. Without it,
a lens with no brief gets none, and says so. `facts.reviewers: facts-only`
gives reviewers no brief a model wrote at all.

Code: src/acr/stages.rs::briefer_prompt; src/acr/stages.rs::read_briefs; src/acr/config.rs::ReviewerInput | Config: stages, facts.reviewers | Tests: a_lens_the_briefs_leave_out_is_no_brief_and_says_so, briefs_written_in_prose_are_put_in_form_not_lost, facts_only_reviewers_get_nothing_a_model_wrote_and_no_decided_non_issues

### Who briefs the rounds after a fix?

Whoever fixed. When the session that ran the review fixes, it passes a brief
on what the fixes changed with `acr resume --fixed --brief`. In a review to
convergence, the steelman briefs the next round.

Code: src/acr/flow.rs::Steelman; src/acr/mod.rs::AcrCmd | CLI: acrbus acr resume --fixed --brief | Tests: the_builder_briefs_round_one_and_the_steelman_the_next

### How are the change's claims about itself treated?

As claims to check, never facts. Its description, commit messages, comments,
docs and test names are claims; with the `claims` stage on, a lens checks
each one. A misleading claim is raised to High by rule, but only when the
Judge quotes the words from the diff.

Code: src/acr/stages.rs::claims; src/acr/stages.rs::claims_prompt; src/acr/flow.rs::grounded | Config: stages | CLI: acrbus acr run --description, acrbus acr run --pr | Tests: a_changes_claims_are_its_docs_comments_test_names_commits_and_description, a_change_carries_its_own_commit_messages_oldest_first

## Configuration and calibration

### Where does a repository's config live, and can a change alter it?

At `~/.config/acrbus/acr/<repository>.yaml`, outside the repository, or
else the repository's own `.acr/config.yaml` read from the base branch,
never from the change. Either way, nothing in a change can switch off its
own review. A repository's config can choose among your bindings, but can't
add secrets, tools, commands or bindings.

Code: src/acr/mod.rs::repo_layer; src/acr/config.rs::REPO_CONFIG; src/acr/config.rs::SCHEMA | CLI: acrbus acr validate, acrbus acr schema | Tests: the_config_comes_from_the_base_never_the_change, a_config_kept_outside_the_repository_wins_and_brings_its_own_list, a_repository_cant_add_secrets_tools_commands_or_bindings, a_repository_chooses_only_from_the_users_bindings

### How is a repository configured?

`acr configure` has the configurer, a strong model, review the whole
repository in a clone of its own: what it's built of, how it's tested and
shipped, and which defects its history keeps fixing. It runs the test
commands it proposes, then proposes the config, with its reasons, beside
where the config goes.

Code: src/acr/configure.rs::propose; src/acr/configure.rs::prompt; src/acr/configure.rs::Proposal | CLI: acrbus acr configure --repo | Tests: its_brief_reviews_the_repository_and_a_recalibration_carries_the_feedback, a_proposal_is_read_from_its_blocks, a_config_that_doesnt_hold_gets_one_call_to_fix_it

### What does accepting a config do?

`acr calibration --accept` puts a waiting proposal in place, or takes the
config as it is, and records its fingerprint. It keeps the config it
replaces as a backup, up to 3, and puts the proposal's decided non-issues in
their own file. `acr run` refuses a repository until its config is
accepted.

Code: src/acr/calibration.rs::accept; src/acr/configure.rs::BACKUPS_KEPT; src/acr/mod.rs::calibrated | CLI: acrbus acr calibration --accept | Tests: a_proposal_waits_then_goes_in_place_keeping_backups_and_non_issues, accepting_a_config_calibrates_it_until_the_config_changes

### What makes a repository need accepting again?

Its fingerprint has two parts: the config and its decided non-issues, and
the Judge's binding, prompt and the rules. When the config part changes,
`acr run` refuses until you accept again, or pass `--uncalibrated`. When
only the Judge part changes, as a new Judge binding or an acrbus update can,
reviews go on with a note.

Code: src/acr/calibration.rs::Fingerprint; src/acr/calibration.rs::standing; src/acr/calibration.rs::Standing | CLI: acrbus acr run --uncalibrated, acrbus acr calibration | Tests: accepting_a_config_calibrates_it_until_the_config_changes, a_calibration_fingerprints_every_rule

### What does the note about an earlier Judge mean?

That the config was accepted when the Judge, its prompt or the rules were
different. Reviews aren't weaker meanwhile: each uses the current Judge.
Accepting again only records that the config still fits that Judge, which
clears the note.

Code: src/acr/calibration.rs::ON_AN_EARLIER_JUDGE; src/acr/calibration.rs::report | CLI: acrbus acr calibration --accept | Tests: an_earlier_judge_says_reviews_are_not_weaker_and_what_accepting_does

### How do I tell it how a review did?

`acr feedback <review id>` lists a finished review's findings. Mark each one
right, not a defect, not a blocker, or should block, with a note if it
helps, and say what it missed and in which lens. A finished review's
findings are kept 90 days for this.

Code: src/acr/configure.rs::Verdict; src/acr/configure.rs::add_feedback; src/acr/configure.rs::FINDINGS_DAYS | CLI: acrbus acr feedback --finding, acrbus acr feedback --verdict, acrbus acr feedback --missed, acrbus acr feedback --lens, acrbus acr feedback --note | Tests: feedback_is_kept_per_repository_and_read_since_a_calibration, a_finished_reviews_findings_are_kept_the_latest_of_each

### What does `acr calibrate` do?

It gives the configurer the config, the repository again, and the feedback
since the last calibration, and it proposes a revised config: criteria and
blockers tightened where findings weren't defects or shouldn't block,
widened or attacked better where defects were missed, and lenses added,
merged or retired. You read it and accept it as before. `acr run` reminds
you while feedback waits.

Code: src/acr/configure.rs::propose; src/acr/configure.rs::feedback; src/acr/configure.rs::pending | CLI: acrbus acr calibrate --repo, acrbus acr calibration --accept | Tests: its_brief_reviews_the_repository_and_a_recalibration_carries_the_feedback, feedback_is_kept_per_repository_and_read_since_a_calibration

### Is the calibrated config a fixed baseline?

No. It's a starting point you read and accept, and every review's feedback
can revise it. A repository with little history gets a config from its code
and leans more on your feedback.

Code: src/acr/configure.rs::propose; src/acr/calibration.rs::standing | CLI: acrbus acr feedback, acrbus acr calibrate | Tests: its_brief_reviews_the_repository_and_a_recalibration_carries_the_feedback

### What are decided non-issues?

Things settled as not findings in this repository, kept in a file beside
the config that `not_findings` points at. The brief stage and the Judge get
them. They're part of the config's fingerprint, so changing them means
accepting again.

Code: src/acr/mod.rs::not_findings; src/acr/calibration.rs::Fingerprint | Config: not_findings | Tests: the_brief_and_the_judge_get_the_decided_non_issues

### What is the runbook?

Text that every role able to run things gets, and the brief stage too: how
to set up a copy of this repository on this machine, run its tests, guard a
command that could hang, and where read-only sources are. Write it for a
reviewer starting cold, then follow it by hand once to check it.

Code: src/acr/config.rs::Config; src/acr/roles.rs::reviewer_parts | Config: runbook | Tests: the_brief_stage_briefs_each_lens_and_only_runners_get_the_runbook

### Can a repository limit which models review it?

Yes. `allowed_models` lists patterns, matched without regard to case, and
every binding a review would call, the configurer too, must name a model
one matches, or the config is refused before anything runs. Set in your
`acr.yaml`'s defaults, it holds every repository that doesn't set its own.

Code: src/acr/config.rs::check_models | Config: allowed_models | Tests: a_review_uses_only_the_models_its_repository_allows

### Can some changes always go to a person?

Yes. `always_escalate` lists plain-language descriptions, such as "a change
to storage". At the start of a review, the Orchestrator says whether the
change matches any, and a match escalates to you whatever the human gate.
Split reviewers and values calls always escalate too, in any mode.

Code: src/acr/flow.rs::choose_lenses; src/acr/roles.rs::orchestrator_prompt; src/acr/flow.rs::escalate | Config: always_escalate | Tests: always_escalate_reaches_a_person_whatever_the_gate, split_reviewers_and_values_calls_escalate_in_any_mode

## Stages: evidence acrbus gathers itself

### What stages are there, and which are on by default?

- `gate`: the change's tests, coverage of its lines, and mutants, before any
  reviewer;
- `brief`: per-lens briefs from the Orchestrator;
- `context`: what git says about the changed files, with no model call;
- `claims`: a lens checking each claim the change makes;
- `mutation`: a lens proposing mutants, run by acrbus;
- `probes`: a lens that writes and runs scenarios;
- `test-writing`: pins for each fix, between rounds of a review to
  convergence.

Left out, `stages` is `context` and `probes`. Probes are left out when the
reviewers' binding has no Bash, Write or Edit, and `acr validate` and `acr
plan` say so.

Code: src/acr/config.rs::Stage; src/acr/config.rs::framework_defaults; src/acr/config.rs::PROBES_OFF | Config: stages | CLI: acrbus acr validate, acrbus acr plan | Tests: probes_are_on_by_default_wherever_reviewers_can_run_things, stages_join_as_lenses_and_each_runs_its_own_way

### What does the test gate do?

First in each round, before any reviewer, acrbus runs the change's tests
itself: changed lines no test runs, from a coverage report; mutants of the
changed code every test passes; and the change's own tests run other ways,
such as in random order or another time zone, and again. Every reviewer is
told what it found. A gap the gate measured in the change's own code blocks
by rule, unless the Judge finds it no defect.

Code: src/acr/stages.rs::run_gate; src/acr/stages.rs::uncovered; src/acr/stages.rs::run_mutants; src/acr/flow.rs::gate_ruling | Config: tests.coverage, tests.report, tests.variants, tests.repeat, tests.mutants | Tests: changed_lines_no_test_runs_are_gaps_and_an_unloaded_file_is_one, a_mutant_the_tests_miss_is_a_finding_and_one_they_catch_isnt, the_changes_tests_are_run_other_ways_and_a_failure_is_a_finding

### What's the difference between `tests.gate: warn` and `stop`?

With `warn`, the default, the reviewers run anyway, each told what the gate
found. With `stop`, a gap that blocks ends the round before any reviewer
runs, which spares their calls, but what they'd have found waits a round.

Code: src/acr/config.rs::GateMode; src/acr/flow.rs::gate_round | Config: tests.gate | Tests: a_warning_gate_lets_the_reviewers_run_and_is_the_default, a_test_gap_that_blocks_stops_the_round_before_the_reviewers

### Which tests count as the change's own?

Test files the change adds or changes: a path with "test" or "spec" in it.
In Rust, a source file counts too when the change adds lines to its
top-level `#[cfg(test)]` module; that's the only language whose in-file tests
acrbus recognizes.

Code: src/acr/stages.rs::is_test_path; src/acr/stages.rs::test_modules; src/acr/stages.rs::changed_tests | Tests: a_rust_files_own_tests_are_its_test_module_brace_to_brace, a_pin_in_the_file_it_tests_is_checked_without_the_fix

### How does acrbus find the test command?

From `tests.command`, with `{tests}` standing for the test files to run. Left
out, it's found from the repository's files, for Flutter, Cargo, Go, npm or
pytest. acrbus runs it under `sh -c` in copies of the repository, with no
one at the keyboard, so it must work in a fresh copy.

Code: src/acr/config.rs::Tests; src/acr/stages.rs::test_command; src/acr/stages.rs::run_command | Config: tests.command, tests.setup, tests.timeout, tests.parallel | Tests: a_repositorys_test_command_is_found_from_its_files

### What does `tests.setup` do?

It runs once a round, in one copy of the change, before any reviewer's copy
is made. Every copy that round is then cloned from it with its build
caches, so none starts cold. A setup that fails is logged and told to every
reviewer.

Code: src/acr/stages.rs::run_setup; src/acr/flow.rs::warm_copy | Config: tests.setup | Tests: reviewers_copies_come_from_one_warm_copy_set_up_once, a_failed_warm_setup_is_logged_and_told_to_every_reviewer, the_warm_setup_says_how_it_went_in_its_log_and_progress

### What do probes do?

Probers write scenarios of what the change says it does as tests, in copies
of their own, and run them, concurrent calls and hangs among them. What a
prober writes is kept: after a fix, the next round's probers start with it
and run it again, which shows whether the fix held. A prober that writes no
probe did no work.

Code: src/acr/stages.rs::probes_prompt; src/acr/stages.rs::keep_probes; src/acr/stages.rs::carry_probes | Config: stages | Tests: after_a_fix_the_probers_run_the_earlier_rounds_probes_again, a_probers_work_goes_into_the_next_rounds_copy_and_waits_if_it_no_longer_applies, a_prober_that_writes_no_probe_did_no_work

### What does the context stage give every role?

What the repository's history says about the changed files: earlier commits
to them, docs and tests that mention them, and where the functions the
change touches are used. It's built from git, with no model call.

Code: src/acr/stages.rs::context_pack | Config: stages | Tests: a_context_pack_holds_earlier_commits_docs_tests_and_uses

### What does the test writer do?

Between rounds of a review to convergence, after the steelman's fixes, a
fresh writer that didn't make them pins each fixed blocker with a test. Each
pin must fail on the code from before the fixes and pass after.

Code: src/acr/stages.rs::writer_prompt; src/acr/flow.rs::write_tests | Config: stages | Tests: a_writer_pins_the_fixes_and_each_pin_must_fail_before_and_pass_after, an_inline_pin_is_the_old_code_with_only_the_new_test_module

## Cost, facts and the cache

### What drives a review's cost?

Calls, turns and context. Each call is a session that works in turns, and
every turn rereads everything before it, mostly from the provider's prompt
cache, at the cached rate. So a review's cost is roughly the sum, over its
calls, of what each turn reads and writes, priced at each role's model.
Each round makes about lenses times `fan_out` reviewer calls, plus Judges,
the Sizer, the Orchestrator and stage calls, and a later round runs only the
lenses that attack again. What changes it: the tier, the models per role,
how much of each turn the cache serves, and how many turns a call takes.

Code: src/acr/plan.rs::estimate; src/acr/config.rs::Price; src/acr/report.rs::RoleUsage | Config: tiers, fan_out, allowed_bindings, costs | Tests: cached_input_has_its_own_price_or_the_inputs, the_most_is_every_round_with_a_fix_between

### How do I estimate a review before running it?

`acr plan` shows the tier, lenses, bindings and estimated cost, with no
model calls. It counts only the text the review sends and reads back, so it
leaves out what agents read with their tools. `costs.per_call` adds a
measured average for each role to bring it nearer, and estimates go by the
rounds earlier reviews of the repository took.

Code: src/acr/plan.rs::Plan; src/acr/plan.rs::estimate; src/acr/config.rs::PerCall | Config: costs.per_call | CLI: acrbus acr plan, acrbus acr plan --json | Tests: a_dry_run_estimates_its_cost, a_plan_reads_with_its_estimate, the_expected_cost_goes_by_the_rounds_lenses_took_before

### How do I see what a review cost?

`acr usage <run id>` sums a review's calls by round and role: fresh,
cached and output tokens, dollars and times, with dedupe, repairs,
keep-alives and relay sessions apart. Progress says each call's tokens so
far and the run's total against its plan. On a subscription, with no
prices, it counts tokens and no dollars.

Code: src/acr/report.rs::Report; src/acr/report.rs::session_lines; src/acr/progress.rs::Progress | CLI: acrbus acr usage, acrbus acr usage --json | Tests: calls_are_summed_by_round_and_role_with_dedupe_and_keep_alive_apart, says_each_calls_tokens_so_far_and_the_runs_against_the_plan_and_warns_once, on_a_subscription_it_counts_tokens_and_no_dollars, a_call_without_a_price_leaves_the_total_unknown

### What is the change map, or shared facts?

Each round, tools, not models, map the change: for each changed file, the
definitions the change touches, where they're referenced, the tests that
name them, and its hunks; then the test gate's results. With `--pr`, the
pull request's title, description, linked issues and review comments too.
Every reviewer and Judge gets the map in its system prompt, and it's kept in
the review's folder as `round-<n>.facts.md`.

Code: src/acr/facts.rs::Facts; src/acr/facts.rs::build; src/acr/facts.rs::fetch_pr; src/acr/flow.rs::share_facts | Config: facts.shared | CLI: acrbus acr run --pr | Tests: facts_are_tools_only_deterministic_and_ordered_for_the_cache, a_pull_requests_words_are_read_from_gh_with_every_comment_in_order

### Why does the order of the prompt matter?

A provider's prompt cache serves a prompt's longest prefix it has already
seen. So every reviewer and Judge gets ACR's own instructions first, then
the shared facts, then what differs between calls. A later round keeps
every earlier round's file sections where they were and adds what a fix
changed after them, so it shares the last round's cached prefix through all
of them; the round's number, review comments and gate results come last.

Code: src/acr/facts.rs::render; src/acr/facts.rs::shared_prefix; src/acr/facts.rs::Prefix | Tests: same_role_calls_share_their_head_byte_for_byte_and_what_differs_comes_last, a_fix_to_the_first_file_keeps_every_earlier_section_and_adds_its_new_one_last, a_pull_requests_words_sit_where_the_cache_wants_them

### How do the facts reach each agent?

pi reads them from a file. Claude Code and omp take them in the same
argument as ACR's prompt, up to 100 KB. The map goes to no call unless every
agent the reviewers and Judges are bound to can take it.

Code: src/bridge.rs::FACTS_IN_ARG; src/bridge.rs::FACTS_MAX; src/acr/facts.rs::facts_shared | Config: facts.shared | Tests: a_reviews_facts_cache_key_and_tool_limit_reach_pi_after_acrs_prompt, facts_a_cache_key_and_a_tool_limit_are_only_for_a_review_and_never_widen

### What is learned prefetch?

Each reviewer call's bridge says which files its agent read. acrbus counts
them per lens in a record kept for the repository, and each round's facts
carry the files reviewers keep opening, whole, after the change's own, up to
`learned_bytes` (24 KiB by default), each opened by at least 2 reviewer
calls. Nothing is taken away: reviewers still open what they like. Trials
don't learn or use the record, and `learned: false` turns it off.

Code: src/acr/learned.rs::Learned; src/acr/learned.rs::MIN_OPENS; src/acr/facts.rs::render_learned; src/acr/config.rs::FactsConfig | Config: facts.learned, facts.learned_bytes | Tests: what_reviewers_read_is_learned_and_the_next_review_starts_with_it, often_read_files_keep_to_the_configured_budget, learning_what_reviewers_read_can_be_turned_off_and_trials_never_learn, a_reviewer_calls_reading_counts_once_across_all_its_sessions

### What are warm-up and keep-alive?

A role's first call goes alone until its turn shows it read its prompt, or
for `warm_secs` (60 by default), so the rest of its fan-out start on a
cached prefix. A keep-alive is a minimal call that refreshes a prefix still
in use every `keep_alive_secs` (240 by default), sent only when the bridge
has a slot to spare, never while the last one is still going, and counted
apart.

Code: src/acr/flow.rs::CacheWarm; src/acr/flow.rs::keep_alive; src/acr/roles.rs::KEEP_ALIVE; src/acr/config.rs::FactsConfig | Config: facts.warm_secs, facts.keep_alive_secs | Tests: a_fan_outs_first_call_warms_the_cache_before_the_rest_and_all_share_the_facts, a_keep_alive_refreshes_a_live_prefix_and_is_counted_apart, a_slow_keep_alive_is_never_followed_at_once_by_another_nor_overlapped

### What is the cache key, and how does it reach the provider?

Each review's calls to one bridge and model share a key, so every round
reaches the same cache. A provider that keeps a cache per replica routes by
it. pi's calls send it three ways, for a gateway in between that forwards
only some: the `x-session-affinity` header, `x-bf-eh-x-session-affinity`
for a gateway that forwards only headers with that prefix, and the
request's `user` in an OpenAI-style chat request that sets none.

Code: src/acr/mod.rs::cache_key; src/bridge.rs::CACHE_KEY_ENV; src/bridges/acrbus-pi.js::KEY_HEADERS; src/bridges/acrbus-pi.js::openaiChat | Config: ACRBUS_CACHE_KEY | Tests: the_pi_extension_sends_the_cache_key_where_a_gateway_passes_it_and_holds_tools, every_judge_and_the_judge_keep_alive_ask_for_the_same_prefix_and_cache_key

### What are matched tools?

With `match_tools` on, the default, the reviewers and the Judge bound to one
bridge and model are shown the union of their tools, so their prompts share
a prefix. Each call is still held to its own: pi's calls load an extension
that refuses any other tool and says which it may use. Other agents keep
their own tool lists.

Code: src/acr/flow.rs::matched_tools; src/bridge.rs::ALLOWED_TOOLS_ENV; src/bridges/acrbus-pi.js | Config: facts.match_tools | Tests: matched_tools_are_the_union_for_a_bridge_and_model_only_where_lists_differ, a_reviewers_and_judges_calls_get_the_rounds_facts_key_and_matched_tools

### What does the pi extension log for each turn?

On the call's stderr, for each turn: what the cache served, from pi's own
usage, how the turn ended, how long it took, and where it went, from a few
named response headers, with a routing key logged only as a short hash. It
also says what effort the provider applied, where the provider says.

Code: src/bridges/acrbus-pi.js::routeOf; src/bridges/acrbus-pi.js::applied; src/bridges/acrbus-pi.js::ROUTE_KEY | CLI: acrbus bridge log --stderr | Tests: the_pi_extension_logs_how_each_turn_ended_where_it_went_and_what_effort_it_saw, cache_writes_that_all_read_zero_are_said_to_say_nothing

### What is the cache-adjusted cost?

A derived figure beside the billed one: what a call would have cost had the
provider served from its cache what a turn had just sent, or, for a call
after its fan-out's first, the opening that first call sent. It shows what
cache misses cost. It's never billed.

Code: src/acr/report.rs::ADJUSTED_IS; src/acr/report.rs::adjusted; src/acr/report.rs::Adjusted | CLI: acrbus acr usage | Tests: the_cost_had_the_provider_cached_what_it_should_is_beside_the_billed_one, what_a_turn_billed_fresh_that_the_last_one_sent_is_its_cache_shortfall

### How do I see whether a call read the warmed prefix?

Each call's first turn goes into its usage, where its agent counts it, so
progress and `acr usage` show what each call's first turn read from the
cache. A cache-write count of 0 on every call says nothing: some providers
don't report writes.

Code: src/acr/report.rs::first_turns; src/proto.rs::TurnUsage; src/proto.rs::RunUsage | CLI: acrbus acr usage | Tests: each_calls_first_turn_says_what_it_read_from_the_cache, the_first_call_is_kept_apart_to_show_what_its_first_turn_read, cache_writes_that_all_read_zero_are_said_to_say_nothing

## Savings

### What does ruling reuse do?

A Judge's ruling that didn't block is kept. In a later round, a finding
takes it with no Judge call only when it's the same lens and claim, within 3
lines of the same place, in a file no fix has changed, with nothing the
ruling rested on changed, and the rules still don't make it block. A
blocker's ruling, or one deferred to a ticket, is never reused. It's on by
default.

Code: src/acr/flow.rs::reusable; src/acr/flow.rs::keep_rulings; src/acr/flow.rs::SAME_PLACE; src/acr/config.rs::SavingsConfig | Config: savings.reuse_rulings | Tests: a_ruling_that_didnt_block_is_reused_on_unchanged_code_without_a_judge, a_ruling_on_code_a_fix_changed_goes_to_the_judge_again, a_different_claim_at_the_same_place_isnt_given_the_ruling, a_blocker_deferred_to_a_ticket_is_never_reused_and_is_judged_again, reusing_rulings_can_be_switched_off

### What is a first-pass Judge?

A binding, such as a cheaper model, that rules first and marks what it
isn't sure of. Anything it would have block, rates Medium or above, calls a
split or values call, states a fact the rules raise, or marks unsure goes on
to the Judge, which rules fresh from its own prompt without seeing the
first pass. It's off unless set, and the report says which binding ruled
each finding.

Code: src/acr/flow.rs::first_pass_escalates; src/acr/roles.rs::FIRST_PASS_NOTE; src/acr/config.rs::SavingsConfig | Config: savings.first_pass_judge | Tests: the_first_pass_escalates_whatever_could_block_or_it_isnt_sure_of, what_the_first_pass_sends_on_the_judge_rules_fresh_from_its_own_prompt, a_first_pass_that_settles_a_lens_saves_its_judge_call, a_first_pass_that_rates_nothing_settles_nothing

### How do I check a first-pass Judge before turning it on?

`acr judge-check <key>... --first-pass <binding>` rates each key's findings
with the Judge alone and with the first pass in front of it, and prints both
against the key: blockers agreed, missed and overrated, and which findings
the first pass rated differently. Turn it on only if it misses no blocker
the Judge alone catches.

Code: src/acr/mod.rs::AcrCmd | Config: savings.first_pass_judge | CLI: acrbus acr judge-check --first-pass

### What does `focus_after_fix` do?

After a fix, each lens's reviewers are told, last in their brief, what the
fix changed, its files and diff cut at 24 KB, and what could feel it, from
the round's change map, as where to look first. Their brief and the whole
change are otherwise as before. It's off by default.

Code: src/acr/flow.rs::focus_note; src/acr/roles.rs::focus_after_fix; src/acr/roles.rs::FIX_DIFF_MAX | Config: savings.focus_after_fix | Tests: after_a_fix_reviewers_are_told_where_to_look_first_when_its_on

### What does `blast_radius` do?

After a fix, one small call on the Sizer's binding, with no tools, says
whether each lens that passed before must attack again. A lens it says the
fix can't touch, with a reason, skips that round, and is reported as
skipped, never as passing. A lens with its own blockers, one that failed,
and any answer it can't give attack as before. It's off by default.

Code: src/acr/flow.rs::blast_radius; src/acr/roles.rs::blast_radius_prompt; src/acr/flow.rs::Skipped | Config: savings.blast_radius | Tests: a_lens_the_blast_radius_check_skips_is_reported_skipped_and_swept_later, a_skipped_lens_never_reads_as_passing_the_version_it_skipped

### What's the difference between per-lens and per-finding Judges?

`per-lens`, the default, gives one Judge call each lens's findings with the
reviewers' case. `per-finding` gives each finding a Judge of its own, with
its claim and the lines it cites and none of the reviewer's case. `acr
judge-check --per-finding` sets the two against a key before you switch.

Code: src/acr/config.rs::JudgeMode; src/acr/roles.rs::judge_finding_parts | Config: facts.judges | CLI: acrbus acr judge-check --per-finding | Tests: per_finding_judges_rate_each_claim_alone

### Does any saving take a tool or a file away from a reviewer?

No. Every role keeps its tools and what it may read, and whatever a saving
can't settle goes the way it would have gone without it. A skipped lens is
reported as skipped.

Code: src/acr/config.rs::SavingsConfig; src/acr/roles.rs::reviewer_prompt | Config: savings | Tests: a_reviewer_is_told_to_read_in_fewer_turns_and_limited_in_nothing, a_skipped_lens_never_reads_as_passing_the_version_it_skipped

## Long calls in shorter sessions

### What is a relay?

With `sessions.relay_tokens` set for a role, a call whose turn reads more
prompt than that, while still calling tools, is told to stop and hand off:
what it found, checked and left. A fresh session on the same binding starts
from the call's own brief, so it shares the cached prefix, then the earlier
sessions' handoff. The call's answer is every session's findings, each once.
It's off unless set, and the least it can be set to is 32000 tokens.

Code: src/acr/config.rs::SessionsConfig; src/acr/config.rs::RELAY_MIN_TOKENS; src/acr/roles.rs::Handoff; src/bridges/acrbus-pi.js::HANDOFF_MARK | Config: sessions.relay_tokens | Tests: a_session_past_its_size_hands_off_and_the_next_carries_its_findings_through, a_judge_past_its_size_hands_off_and_its_ratings_are_merged, without_a_relay_a_call_runs_in_one_session_as_before, sessions_are_off_unless_set_and_checked_when_they_are

### Which agents can relay?

Only one acrbus can steer mid-call: pi, through acrbus's extension. A call
on any other agent runs whole, and progress says so once for that agent.

Code: src/bridge.rs::RELAY_ENV; src/bridges/acrbus-pi.js | Config: sessions.relay_tokens | Tests: a_relay_is_only_for_a_review_and_only_an_agent_that_can_be_steered_takes_it, a_relays_size_reaches_pi_and_loads_acrbus_extension

### What does `scout` do?

A scouted lens's reviewer first runs a scout session, which lists everything
it suspects, with how to confirm or rule each out, and verifies nothing.
Each suspicion then gets a short session of its own, from the reviewer's
brief and that suspicion alone. Every suspicion is checked; one that can't
be checked fails the reviewer. It's off unless set.

Code: src/acr/roles.rs::scout_part; src/acr/roles.rs::verify_part; src/acr/roles.rs::Suspect | Config: sessions.scout | Tests: a_scout_lists_suspects_and_each_is_verified_from_its_own_brief, a_scouted_lens_lists_suspicions_and_verifies_each_on_its_own, a_suspicion_that_cant_be_checked_fails_the_reviewer

### Can a handoff lose findings?

It can: a handoff is a summary. A handoff that can't be read, or that did no
work, fails the call. To check that nothing is lost, run the same trial
with `sessions` unset and set, and compare what each caught and what each
cost. Each session shows in `acr usage` as `leg N`, `scout` or `verify N`.

Code: src/acr/roles.rs::handoff_of; src/acr/report.rs::Sessions | Config: sessions | CLI: acrbus acr trial --arm, acrbus acr rescore --results, acrbus acr usage | Tests: a_handoff_that_cant_be_read_or_makes_no_progress_is_never_a_pass, a_handoff_is_read_from_its_mark_or_its_field_and_a_bad_one_is_an_error

## Fixes

### Who fixes the blockers?

By default, a fresh agent on the Orchestrator's binding, when that binding
has `writes: true`. If it can't write, the session that ran the review
fixes them, since it holds the change's context, and resumes with `acr
resume --fixed`. When the Orchestrator can write, the scope gate asks you
which.

Code: src/acr/flow.rs::Fixer; src/acr/flow.rs::session_fixes; src/acr/carousel.rs::fixer; src/acr/carousel.rs::SESSION | Config: allowed_bindings | CLI: acrbus acr resume --fixed | Tests: a_fix_needs_somewhere_to_go, a_fix_by_the_session_that_ran_the_review_is_flagged_too

### Where do fixes go, and does acrbus commit them?

Into the checkout the review was started from, which must hold the reviewed
commit by then. Until the first fix, the review reads a copy of its commit,
so the checkout can be on any branch meanwhile. The fixer is told not to
commit, and acrbus doesn't commit to the checkout.

Code: src/acr/roles.rs::fix_prompt; src/acr/flow.rs::fix | CLI: acrbus acr run --head | Tests: fixes_go_into_the_checkout_which_must_hold_the_reviewed_commit_first, a_review_reads_a_copy_of_its_commit_until_the_first_fix, a_reviewed_commit_must_be_what_is_checked_out

### What is a fix in groups?

With `group_fixes`, the default, the Orchestrator fixes a round's blockers
in groups: blockers in the same file together, a defect several lenses found
with the one it repeats, a defect you reported alone, and the rest one by
one. Each group is a fresh session with the same opening and only its own
blockers. Groups run one after another in the checkout, each seeing what
earlier ones changed. A failed group escalates, and trying again carries on
from it.

Code: src/acr/flow.rs::fix_groups; src/acr/flow.rs::fix_in_groups; src/acr/flow.rs::FixGroup | Config: savings.group_fixes | Tests: fix_groups_put_one_files_blockers_and_a_same_defect_together_and_the_rest_alone, a_fix_goes_in_groups_by_file_each_a_fresh_session_with_only_its_blockers, a_group_that_fails_is_tried_again_from_where_it_stopped, a_session_that_takes_over_a_fix_in_groups_gets_only_whats_left

### How does a fixer's relay differ from a reviewer's?

It measures what the session's prompt grew since its own first turn, not
the prompt itself, and hands off only once the session has made an edit, or
has grown twice the size without one. The handoff carries what it changed,
what it read, the edits it was about to make, and what's left. A fixer that
hands off twice with nothing more done stops its relay.

Code: src/bridge.rs::RELAY_GROWTH_ENV; src/acr/roles.rs::fix_continuation | Config: sessions.relay_tokens | Tests: the_pi_extension_relays_a_fixer_by_its_growth_and_only_after_its_first_edit, a_fixers_handoff_carries_its_reading_and_planned_edits_to_the_next_session, a_fixer_that_hands_off_twice_with_nothing_more_done_stops_its_relay, a_fixers_relay_reaches_pi_as_growth_after_an_edit

### What keeps a fix in scope?

The fixer is told to change code, tests and docs, not what's built from
them, and not to run a command that rewrites tracked files beyond the fix.
Afterwards every file it changed is listed. A binary, or a file none of its
blockers points at, is put to you at the next gate, to keep or revert.

Code: src/acr/roles.rs::FIX_SCOPE; src/acr/flow.rs::flag_fix; src/acr/flow.rs::revert_flagged; src/acr/carousel.rs::KEEP_THEM | Tests: a_fix_that_changes_a_binary_or_a_file_no_blocker_names_is_flagged_and_asked_at_the_next_gate, a_file_the_blockers_name_is_not_flagged_and_a_revert_that_fails_asks_again, what_a_fix_changed_says_which_files_are_binary_and_each_reverts_as_its_command_says

### Which tests does a fixer run?

Only those that cover what it changed, scoped to the packages and test names
it touched. It's told not to run the whole suite, a race or mutation run, or
the same tests again with nothing changed, since every test run's output
stays in its session and is read again each turn.

Code: src/acr/roles.rs::FIX_TESTS; src/acr/roles.rs::fix_prompt | Tests: a_fixer_is_told_to_run_only_the_tests_that_cover_its_change

### What if a fix fails?

It escalates to you, with three choices: the Orchestrator tries again, the
session that ran the review fixes and resumes with `--fixed`, or the review
stops. A fix that leaves the
checkout as it found it did no work and fails, unless it declines each of
its blockers with a reason.

Code: src/acr/flow.rs::fix_call; src/acr/flow.rs::About; src/acr/carousel.rs::AGAIN | Tests: a_fix_group_that_changes_nothing_did_no_work_unless_it_declines_each_blocker, a_fixer_is_told_what_its_handoff_holds_and_how_to_decline

## When things fail

### Can a failed step pass as clean?

No. A failed reviewer or Judge leaves its lens open, never a clean pass, and
an answer that can't be read is an error, never an empty result. A lens with
nothing for its Judge says so. Failures are recorded, and an unfinished lens
is put to you.

Code: src/acr/flow.rs::LensResult; src/acr/flow.rs::escalate_about; src/acr/flow.rs::About | Tests: a_failed_reviewer_is_never_a_clean_pass, a_failed_judge_leaves_the_lens_open, one_failed_reviewer_of_several_leaves_the_lens_open, each_lens_judge_starts_as_its_own_reviewers_answer_and_one_with_nothing_says_so

### What does "did no work" mean?

A step is judged by what it must do, never by what it found: a reviewer
that read the change and found nothing did its work. A step observed doing
none fails, to be tried again or escalated, and progress says "did no work"
and why:

- a fix, or one session of it, that leaves the checkout unchanged, unless
  it declines each blocker with a reason;
- a handoff that adds nothing;
- a reviewer, scout or prober that made no tool call, where its bridge
  counts them, and a prober that wrote no probe;
- a Judge that rated none of its findings;
- a test run that passed but ran no tests.

The Sizer's tool-free first look, keep-alives and grouping duplicates are
exempt.

Code: src/acr/flow.rs::no_work; src/acr/flow.rs::read_something; src/acr/stages.rs::ran_no_tests | Tests: a_reviewer_that_calls_no_tool_did_no_work, a_judge_that_rates_none_of_its_findings_did_no_work, a_prober_that_writes_no_probe_did_no_work, a_coverage_run_that_ran_no_tests_did_no_work, a_fixer_that_hands_off_with_no_change_to_the_tree_did_no_work, a_run_that_says_it_ran_no_tests_ran_none_and_one_that_ran_some_did

### What happens when a call stalls?

A bridge watches each run. One whose processes use no CPU and write no file
for `idle_secs`, 20 minutes by default, is woken; if nothing stirs, it's
ended and tried again in a fresh run. A busy run is never taken for a quiet
one, and nothing cuts off a call that's working.

Code: src/bridge.rs::default_idle; src/bridge.rs::SHAKE_WAIT; src/bridge.rs::MAX_ATTEMPTS | Config: idle_secs | CLI: acrbus bridge status | Tests: a_shake_wakes_a_stopped_process_and_the_run_goes_on, a_busy_run_is_never_taken_for_a_quiet_one, a_run_abandoned_on_every_try_fails_saying_what_the_shake_found, an_abandoned_run_is_tried_again_and_can_still_answer

### Is there a time budget on calls?

No. A call asks its bridge for as long as the bridge allows:
`max_timeout_secs`, or else `timeout_secs`, 600 unless set; the Claude Code,
omp and pi kinds set 14400, 4 hours, as a cap no review should reach. A
binding's `timeout`, if set, is also a cap, not a budget. A call that runs
out of time is a failure, and nothing tries it elsewhere.

Code: src/bridge.rs::default_timeout; src/bridges/claude.toml; src/bridges/pi.toml; src/acr/config.rs::Binding | Config: timeout_secs, max_timeout_secs, timeout | Tests: a_call_asks_its_bridge_for_as_long_as_it_allows_not_its_default, a_call_out_of_time_is_a_failure_and_nothing_tries_it_elsewhere, a_config_with_time_budgets_still_loads_and_says_what_they_do

### What is a stall warning?

A pi call logs each turn as it ends. Progress warns when a call has logged
no new turn for `sessions.stall_warn_secs`, 180 seconds by default, and
again at each multiple while the silence lasts, and again if it moves on and
stops. It cuts nothing off; that's the bridge's idle watch.

Code: src/acr/config.rs::STALL_WARN_SECS; src/acr/progress.rs::STALL; src/acr/progress.rs::stalls | Config: sessions.stall_warn_secs | Tests: warns_when_a_call_logs_no_new_turn_and_again_while_it_lasts_and_after_it_moves

### What is a lost request?

A model request from a pi call that gets no response, no headers or, once
its stream has begun, no event, for `ACRBUS_NO_RESPONSE_SECS` (300 by
default) is aborted as timed out, and pi retries it. After
`ACRBUS_NO_RESPONSE_TRIES` (3) in a row the call ends with exit code 75,
which its bridge fails, so the review retries or escalates it. Bytes with no
event don't count as a response, and a request that's answering is never
cut off.

Code: src/bridges/acrbus-pi.js::guard; src/bridges/acrbus-pi.js::NO_RESPONSE_EXIT; src/bridges/pi.toml | Config: ACRBUS_NO_RESPONSE_SECS, ACRBUS_NO_RESPONSE_TRIES | Tests: the_pi_extension_ends_a_request_with_no_response_and_retries_it

### What happens during a network outage?

There's no outage mode. Each call fails on its own, as above, and a lens
whose reviewers failed is put to you as unfinished: try it again next round,
go on without it, which is recorded as waived, or stop.

Code: src/acr/flow.rs::About; src/acr/carousel.rs::AGAIN; src/acr/carousel.rs::WITHOUT | Tests: one_failed_reviewer_of_several_leaves_the_lens_open, a_failed_reviewer_is_never_a_clean_pass

### What happens when the Sizer or Orchestrator fails?

A failed Sizer means the highest tier. An Orchestrator that can't say which
description-triggered lenses apply leaves every candidate in, and you're
told why.

Code: src/acr/flow.rs::choose_tier; src/acr/flow.rs::choose_lenses; src/acr/plan.rs::highest | Tests: an_unsure_sizer_means_the_higher_tier, an_orchestrator_that_picks_lenses_in_prose_is_put_in_form

### How do I stop a review?

`acr stop <id>`, whether it's running or waiting for you. It stops the
process driving it, by its recorded mark, never by a name or pattern, which
cancels its calls; the review ends as stopped, lets go of its claim, and
lists what it left, touching none of it. Its usage still counts every call
that finished.

Code: src/acr/mod.rs::stop_live; src/acr/mod.rs::stops_calls | CLI: acrbus acr stop | Tests: a_running_review_stops_in_one_step, a_waiting_review_can_be_stopped_whatever_its_agents_and_checkout_are, a_stopped_review_says_what_it_left_and_touches_none_of_it, a_stopped_runs_finished_calls_are_counted_from_their_answers, a_review_keeps_its_calls_in_flight_so_stopping_it_stops_them

### How do I know what a review is doing?

It says on stderr, and in `progress.log` in its folder, what each call is
doing at least once a minute: how long it has run, when its bridge last saw
it work, its tokens so far, and calls waiting for a slot. While it runs,
`acr ask <id>` shows where each lens of the round stands.

Code: src/acr/progress.rs::EVERY; src/acr/progress.rs::Progress; src/acr/flow.rs::LENS_STATES | CLI: acrbus acr ask | Tests: says_every_call_at_work_and_how_its_bridge_sees_it_at_a_steady_pace, says_what_its_doing_even_when_no_call_is_at_work, ask_shows_a_running_reviews_lenses_where_they_stand

### Can I update acrbus while a review runs?

`acrbus service install`, which replaces a running daemon, first waits up
to 30 minutes for runs on its bridges and reviews on this machine to
finish; `--now` doesn't wait. `acrbus bridge install` waits the same way
for that bridge's runs.

Code: src/setup.rs::service_install; src/setup.rs::wait_for_quiet; src/setup.rs::QUIET_WAIT; src/setup.rs::bridge_install | CLI: acrbus service install --now, acrbus bridge install --now | Tests: an_update_waits_for_runs_and_reviews_under_way

## You, in the loop

### When does a review ask me?

It depends on `human_gate`:

- `in-the-loop`, the default: at the scope, at every round's blockers, and
  at the result;
- `per-round`: at every round's blockers;
- `ends`: at the scope and the result;
- `exception`: only escalations.

Escalations always reach you: a values call, a split, a Critical that was
there before, a lens that didn't finish, a failed fix, and `max_rounds`.

Code: src/acr/config.rs::HumanGate; src/acr/flow.rs::About | Config: human_gate | CLI: acrbus acr run --human-gate | Tests: a_person_in_the_loop_approves_the_scope_decides_each_blocker_and_the_result, per_round_lets_a_person_decide_what_goes_in, the_ends_gate_asks_about_the_scope_and_the_result, split_reviewers_and_values_calls_escalate_in_any_mode

### What can I answer for each blocker?

- **Fix it:** the fixer fixes it, and the next round checks the fix; choose
  Other to add a note for the fixer.
- **Defer to a ticket:** it's real, the change ships without the fix, and a
  ticket is drafted.
- **Not a blocker:** for a finding that can be waived; it's logged, and the
  calibration learns it.
- **Not a defect:** dropped, and the calibration learns it.
- **Waive it:** typed, for a finding that can be waived, to ship with no
  ticket.

Code: src/acr/carousel.rs::blocker; src/acr/carousel.rs::FIX; src/acr/carousel.rs::DEFER; src/acr/carousel.rs::NOT_BLOCKER; src/acr/carousel.rs::NOT_DEFECT; src/acr/carousel.rs::WAIVE | CLI: acrbus acr resume --fold, acrbus acr resume --waive, acrbus acr resume --defer | Tests: defer_decides_any_blocker_and_waive_still_only_what_can_be, a_blocker_deferred_to_a_ticket_neither_blocks_nor_is_waived_and_gets_a_draft

### What does deferring to a ticket do?

The blocker is recorded as deferred, neither blocking nor waived, and a
draft ticket, title then body, is written to `tickets/` in the review's
folder. Nothing is filed for you.

Code: src/acr/mod.rs::write_tickets; src/acr/report.rs::tickets; src/acr/report.rs::Ticket | CLI: acrbus acr resume --defer | Tests: a_blocker_deferred_to_a_ticket_gets_a_draft_in_the_reviews_folder, a_blocker_deferred_to_a_ticket_neither_blocks_nor_is_waived_and_gets_a_draft

### What is a values call?

A finding where reviewers split, or whose ruling turns on a choice of
values, such as which failure is unacceptable, rather than on a fact. The
structure surfaces it and doesn't decide it: it's always put to you, and
your ruling stands.

Code: src/acr/roles.rs::Rating; src/acr/flow.rs::About | Tests: a_values_call_is_the_persons_and_their_ruling_stands, split_reviewers_and_values_calls_escalate_in_any_mode

### Can I report a defect the reviewers missed?

Yes. At the end of a round, a question asks what they missed. A defect you
report is folded in like a blocker, fixed in a group of its own, and every
lens attacks again after it, since any lens could own it. After a review
ends, `acr feedback --missed` records one for the calibration instead.

Code: src/acr/flow.rs::Reported; src/acr/flow.rs::reopen_all; src/acr/carousel.rs::NOTHING | CLI: acrbus acr ask, acrbus acr feedback --missed

### How do I answer a review?

Each question is short and multiple-choice. In Claude Code, the ACR skill
shows them as question carousels of up to 4 questions; at a terminal, it's a
numbered prompt; with Grayboard set up, they reach your phone. From any
shell, `acr ask <id>` shows them and `acr resume <id> --answers` answers. A
question left unanswered, or an answer to no question, is refused.

Code: src/acr/carousel.rs::PER_CAROUSEL; src/acr/carousel.rs::Carousel; src/acr/mod.rs::at_terminal | CLI: acrbus acr ask, acrbus acr ask --text, acrbus acr resume --answers | Tests: questions_must_fit_a_question_carousel, what_a_person_types_at_the_terminal_is_their_answer, the_person_can_stop_anywhere_and_must_answer_whats_asked

### What does Grayboard add?

With a Grayboard team server, a review waiting for you sends its questions,
or with `nudge` only that it waits, to your `human` handle, which reaches
Slack, and your reply there resumes it. A review also claims its change on
the server, so a second review of it doesn't start while the first runs or
waits. A crashed review's claim goes after 10 minutes unrenewed. When the
server is away, reviews go on without it.

Code: src/acr/grayboard.rs::serve; src/acr/grayboard.rs::claim_key; src/acr/grayboard.rs::ORPHAN_GRACE_MS; src/acr/config.rs::Share | Config: integrations.grayboard, grayboard.server, grayboard.poll_secs | CLI: acrbus grayboard login, acrbus grayboard status | Tests: a_reviews_questions_reach_you_and_your_reply_resumes_it, a_change_is_claimed_while_its_review_lasts, a_crashed_reviews_claim_is_released_after_a_grace, a_nudge_says_only_that_a_review_waits, a_note_that_fails_doesnt_hold_up_your_replies_or_the_claims

### Can a review post to a pull request?

Only if the repository's config turns it on, with labels or a comment, and
you pass `--post-pr`. A repository that turned nothing on gets neither.

Code: src/acr/config.rs::Integrations; src/acr/config.rs::Labels; src/acr/report.rs::comment | Config: integrations.labels, integrations.comment | CLI: acrbus acr run --post-pr | Tests: a_repository_that_turned_nothing_on_gets_no_labels_or_comment

## Models and bindings

### Which models does a review use?

Whatever each role's binding names, in `~/.config/acrbus/acr.yaml`. `acrbus
setup acr` writes bindings for headless Claude Code; a bridge to pi, omp or
another agent works the same way. `--binding role=name` overrides one for a
run.

Code: src/acr/config.rs::UserConfig; src/setup.rs::acr_bindings; src/acr/mod.rs::parse_binding | Config: allowed_bindings, defaults.bindings | CLI: acrbus setup acr, acrbus acr run --binding | Tests: what_setup_acr_writes_is_a_working_config, the_framework_chooses_no_model

### Can reviewers come from different model families?

Yes. A lens can have its own binding, so lenses on one review can run on
different models and providers, and the Judge can differ from the
reviewers. Each binding must be on this machine and taken by its bridge,
which is checked before a review starts.

Code: src/acr/config.rs::Lens; src/acr/mod.rs::check_machine | Config: lenses.binding | Tests: a_lens_can_have_its_own_binding, a_binding_its_bridge_would_refuse_stops_a_review_before_it_starts

### How is effort set?

A binding's `effort` is passed to its agent where its bridge allows it. pi
rounds a level its model doesn't have to one it does, and says so in an
`acrbus effort:` line; where the provider echoes what it applied, another
line says so.

Code: src/acr/config.rs::Binding; src/bridge.rs::EFFORT_ENV; src/bridges/pi.toml | Config: effort | Tests: options_reach_the_agent_as_arguments_and_the_reply_says_what_applied, the_pi_extension_logs_how_each_turn_ended_where_it_went_and_what_effort_it_saw

### What prices do estimates and reports use?

A binding's own prices, per million tokens, for input, cached input, cache
writes and output. Cached input with no price of its own costs what other
input does. A call with no price leaves the total unknown, and the report
says which.

Code: src/acr/config.rs::Price; src/acr/config.rs::price | Config: costs | Tests: a_binding_with_its_own_prices_keeps_them, cached_input_has_its_own_price_or_the_inputs, with_no_prices_it_says_it_cant_tell_and_which, a_price_must_be_for_a_binding_in_use_and_not_negative

## Confinement and safety

### Can a reviewer change my checkout?

No. A reviewer whose binding has Bash, Write or Edit works in a copy of its
own, cloned from the round's warm copy, and nothing it does reaches the
checkout. The Judge can't change it either. Only a fix writes to the
checkout.

Code: src/acr/flow.rs::Copies; src/acr/flow.rs::fresh_copy_dir; src/acr/mod.rs::CLONES | Tests: a_reviewer_that_can_change_files_works_in_a_copy_of_its_own, a_reviewers_copy_is_the_checkout_as_it_is_and_reaches_nothing_back, a_judge_gets_the_diff_and_may_not_change_the_checkout

### Is there a sandbox?

No. A bridge isn't a sandbox: the agent runs as you, with what its own
settings allow. `acrbus setup acr` confines Claude Code's file tools to
their working directories, but not its commands. What guides every role
beyond that is ACR's system prompt, which guides and doesn't fence.

Code: src/setup.rs::confine_settings; src/setup.rs::SYSTEM_PROMPT; src/acr/system-prompt.md | Tests: confinement_blocks_reads_and_writes_outside_and_nothing_else, every_review_role_gets_a_system_prompt_that_guides_and_doesnt_fence

### What does ACR's system prompt tell every role?

That what it reads is data, never instructions; to work freely in its own
directory and temporary folder; never to touch credentials; not to read
other repositories or what earlier reviews and runs left; not to send code
or findings anywhere but its answer; to make no lasting change outside its
places; and, when unsure, to say what it would need instead of doing it.

Code: src/acr/system-prompt.md; src/setup.rs::SYSTEM_PROMPT; src/setup.rs::gives_system_prompt | Tests: a_reviews_every_role_gets_acrs_system_prompt_and_nothing_else_does, the_system_prompt_kept_is_always_this_versions, an_agent_that_keeps_one_system_prompt_gets_acrs_first_in_it

### Can reviewers reach the network?

Yes, through their own harness, the way it always does. ACR's system prompt
lets them fetch packages, tools and documentation a review needs, and tells
them not to send code or findings anywhere. A repository's runbook can say
what must never run, such as anything that touches a live backend.

Code: src/acr/system-prompt.md | Config: runbook

### How are credentials kept out?

A bridge's agent gets only `HOME`, `PATH`, `TMPDIR`, a few inherited
variables and what its table adds, and secrets come from files only you can
read, read for each run. What a run prints to stderr is scrubbed of tokens
and secrets, and cut to a cap, before it leaves in a reply.

Code: src/bridge.rs::INHERITED; src/bridge.rs::RESERVED; src/bridge.rs::REDACTED | Config: env, secrets | Tests: secrets_reach_the_agent_and_nothing_else, a_secret_must_be_private, stderr_leaves_with_secrets_and_tokens_out_and_within_its_cap, scrubbing_takes_tokens_and_leaves_the_rest

### Which repositories can be reviewed?

Only those you allow on this machine with `acr add-repo`, which lets the
review's bridge work there. A working directory can be inside an allowed
one but never step out of it.

Code: src/setup.rs::acr_workdirs; src/acr/mod.rs::AcrCmd | CLI: acrbus acr add-repo, acrbus acr remove-repo | Tests: a_repository_is_allowed_once_and_can_be_dropped, a_workdir_can_be_inside_an_allowed_one_but_never_step_out

## What's kept

### What does acrbus keep?

Nothing a run or review leaves is deleted as it finishes. Each bridge
request's folder, a finished review's files, a trial's or check's copy and
sessions, and kept findings are packed into a `.tar.gz` named for when it
began. Reviewers' copies stay unpacked, as cheap clones. Everything is under
the state folder, which only you can read.

Code: src/cleanup.rs::sweep; src/acr/keep.rs::pack; src/config.rs::make_state_dir | Tests: a_run_is_packed_once_answered_and_kept_until_the_cleanup_clears_it, a_finished_review_is_packed_and_kept_a_while_and_one_left_waiting_longer, a_packed_folder_is_one_archive_its_files_read_from

### Why are finished things packed?

So an agent that searches files later doesn't come on an earlier review's
findings. A reviewer with a shell could still unpack one, so ACR's system
prompt tells it not to read what earlier reviews and runs left.

Code: src/cleanup.rs::sweep; src/acr/system-prompt.md

### When is it cleared?

The daemon cleans up once an hour. It clears what's older than 30 days, or a
bridge's `log_retention_days`, going by the time in its name, and packs
anything finished that wasn't. A review still open stays however old, and
kept findings stay 90 days.

Code: src/cleanup.rs::KEEP_DAYS; src/cleanup.rs::EVERY; src/acr/configure.rs::FINDINGS_DAYS | Config: log_retention_days | Tests: the_cleanup_clears_only_whats_past_keeping_and_never_an_open_run

### Can I clear copies now?

`acr clean` removes the copies that finished reviews, checks, the doctor and
the configurer left, and keeps those of reviews running or waiting.
`--dry-run` lists them, with their size.

Code: src/acr/mod.rs::clean; src/acr/mod.rs::kept_copies | CLI: acrbus acr clean, acrbus acr clean --dry-run | Tests: clean_clears_only_copies_left_over_and_doctor_counts_them

### Where do I find what an agent did?

`acrbus bridge log <agent> <id>` reads a run's record, with `--stdout` or
`--stderr` for its output. A review's folder holds its progress log, each
round's diff and facts, and where each lens stands.

Code: src/cli.rs::BridgeCmd; src/acr/flow.rs::write_artifact | CLI: acrbus bridge log --stdout, acrbus bridge log --stderr | Tests: each_run_is_logged_under_its_id, writes_the_same_lines_to_a_log_for_whoever_reads_it_later

## Measuring it

### How do I measure a review design?

With a key: a real change, frozen with its repository, base, head, a
paragraph of what it's for, and its dimensions. `acr trial <key>` runs a
design blind, with nobody in the loop, and a scorer compares what it found
with the key's findings. For new work, leave the findings out: trials are
saved unscored, for `acr rescore` once the change's own review is done.

Code: src/acr/trial.rs::Key; src/acr/trial.rs::score; src/acr/trial.rs::rescore | CLI: acrbus acr trial, acrbus acr rescore --results | Tests: new_work_needs_no_findings_and_is_saved_unscored, a_scorer_that_fails_is_tried_again_and_then_leaves_the_run_unscored, a_trials_copy_holds_the_head_and_nothing_after_it

### Which designs can a trial run?

- `one-size`: builder-led, every reviewer at one size;
- `sized`: builder-led, each reviewer sized to its dimension;
- `separated`: acrbus's pipeline, one round;
- `converge`: the same pipeline, to convergence.

Left out, the three one-round designs run one after another; `converge` runs
only when named. A trial's scorer, and the builder of a builder-led design,
run on headless Claude Code.

Code: src/acr/trial.rs::Arm; src/acr/trial.rs::BUILDER_MODEL; src/acr/trial.rs::SCORER_MODEL | CLI: acrbus acr trial --arm, acrbus acr trial --stop-on-failure | Tests: a_one_size_trial_tells_its_builder_so, stop_on_failure_ends_the_run_at_the_first_reviewer_or_judge_failure

### What does `acr check` do?

It checks that a trial on a key can run before you spend one: a session is
started as a builder is, and one through each bridge and set of tools the
pipeline uses. Each must start, read the trial's copy, fail to read a file
outside it, and run a command where it has a shell. A read outside fails the
check only for an agent its bridge confines; for others it's a note.

Code: src/acr/trial.rs::check_session; src/acr/trial.rs::READ_OUTSIDE | CLI: acrbus acr check | Tests: a_check_catches_a_read_outside_and_a_session_outside_the_copy, a_check_says_an_unconfined_agent_reads_outside_and_fails_a_confined_one, a_check_needs_no_trial_builder_where_claude_code_isnt_set_up

### What do `judge-check`, `residual` and `benchmark` measure?

- `judge-check`: how the Judge rates what each key's own review found,
  against how that review rated it: blockers agreed, missed and overrated.
  One Judge call per dimension, no reviewers, and it calibrates nothing.
- `residual`: whether a key's defects, and a run's upheld blockers, are
  still in a final version of the change.
- `benchmark`: a binding set scored against a commit with known blockers.

Code: src/acr/mod.rs::AcrCmd; src/acr/trial.rs::residual | CLI: acrbus acr judge-check, acrbus acr residual --patch, acrbus acr residual --commit, acrbus acr benchmark --known | Tests: a_benchmark_counts_caught_missed_and_false, one_defect_list_holds_the_keys_findings_and_a_runs_upheld_blockers

## Install and setup

### What does acrbus need?

A Rust toolchain to build it, git, and macOS or Linux. A harness CLI for
the roles: `acrbus setup acr` sets up headless Claude Code; pi, omp or
another agent works with a bridge of its own. `gh`, for `--pr` and
`--post-pr`.

Code: src/setup.rs::find_claude; src/acr/facts.rs::fetch_pr | CLI: acrbus setup acr, acrbus acr run --pr

### How do I install and set it up?

`cargo install --locked --git <this repository> --root ~/.local`, then name
the bus's owner in `~/.config/acrbus/config.toml`, and run `acrbus setup
all`: the daemon at login, the Claude Code plugin, and headless Claude Code
as ACR's agent with its token, bindings and service. Each part is its own
command too, and `setup acr` is safe to run again.

Code: src/setup.rs::service_install; src/setup.rs::claude; src/setup.rs::acr | Config: owner | CLI: acrbus setup all, acrbus setup claude, acrbus setup acr, acrbus service install | Tests: what_setup_acr_writes_is_a_working_config, a_new_acr_bridge_has_your_toolchains_and_downloads_nothing, setup_says_what_an_older_acr_bridge_lacks_and_changes_nothing

### How do I review my first change?

1. Allow the repository: `acrbus acr add-repo <path>`.
2. Configure it: `acrbus acr configure --repo <path>`.
3. Read the proposal, change what you like, and accept it:
   `acrbus acr calibration --repo <path> --accept`.
4. Review: `acrbus acr run --repo <path> --base <ref> --brief <file>`.
5. Say how it did with `acrbus acr feedback <id>`, and fold that in with
   `acrbus acr calibrate --repo <path>`.

The [quickstart](quickstart.md) has each step and how to check it.

Code: src/acr/mod.rs::AcrCmd | CLI: acrbus acr add-repo, acrbus acr configure --repo, acrbus acr calibration --accept, acrbus acr run --base, acrbus acr feedback, acrbus acr calibrate | Tests: an_unconfigured_repository_is_told_how_to_get_a_config

### How do I check a machine and repository are ready?

`acr doctor` checks each thing a review needs and says what's wrong and the
command that fixes it. It runs the config's test commands in a copy, as a
review and its reviewers would, checks they find the repository's
toolchain, and signs in with ACR's token: one small model call. `--no-tests`
skips the tests.

Code: src/acr/mod.rs::doctor; src/acr/mod.rs::doctor_toolchain | CLI: acrbus acr doctor, acrbus acr doctor --no-tests | Tests: the_doctor_says_what_to_fix_and_how, the_doctor_checks_reviewers_find_the_repositorys_toolchain, the_doctor_says_how_to_give_a_bridges_agent_the_system_prompt

### How do I use pi or omp instead of Claude Code?

Add a bridge with `kind = "pi"` or `"omp"` to `config.toml`, with what's
this machine's own, such as its owner, command, home, path and working
directory, install it, and point bindings at its agent. The kind supplies
the rest. A review's pi calls load acrbus's extension, which adds per-turn
cache lines, the cache key, relays, held tools and the lost-request watch.

Code: src/bridge.rs::Kind; src/bridges/pi.toml; src/bridges/acrbus-pi.js | Config: kind, extra_args, allowed_bindings | CLI: acrbus bridge install | Tests: a_relays_size_reaches_pi_and_loads_acrbus_extension, a_pi_bridge_drops_pis_own_docs_from_reviews_unless_its_table_keeps_them

### Can I build my own program on acrbus?

Yes. acrbus is a Rust library too: a program names itself with a
`config::App`, adds networks as `network::Kind`s, and runs the shared
command line. tailbus is acrbus with a Tailscale network, built this way.

Code: src/config.rs::App; src/config.rs::set_app; src/cli.rs::main; src/network.rs::Kind | Tests: a_key_only_a_network_declares_is_a_setting_and_a_typo_is_refused, environment_variables_go_by_the_programs_prefixes_in_order

### Can I keep everything in one folder?

Yes. `ACRBUS_HOME` keeps state and config in one folder, and a second home
names its services apart from the default, so two can run side by side.

Code: src/config.rs::Paths; src/setup.rs::instance | Config: ACRBUS_HOME | Tests: a_second_home_names_its_services_apart_from_the_default

## Limits

### Where is acrbus weaker?

- **`acr run` doesn't sweep every lens after a fix.** Only lenses with
  blockers attack again, unless you ask for another round or report a
  missed defect. The full convergence sweep runs only in `acr trial --arm
  converge`.
- **Much of the cost and reliability work is pi's alone:** relays, per-turn
  cache lines, the cache key headers, the lost-request watch, and holding
  each call to its own tools when tools are matched.
- **There's no sandbox.** Confinement covers Claude Code's file tools only;
  everything else rests on ACR's system prompt and the models.
- **The Judge's facts are a model's.** Fixed rules decide from them, but a
  wrong fact gives a wrong ruling; calibration and feedback are how that's
  caught.
- **Grouping, format repairs and blast radius are model calls.** A failed
  grouping groups nothing and a failed blast-radius answer skips nothing,
  each saying so, but a wrong answer can merge or skip what it shouldn't.
- **A relay's handoff is a summary,** and can lose findings; measure it.
- **`acr plan` underestimates** what agents read with their tools, unless
  `costs.per_call` is set.
- **Only Rust's in-file tests** are recognized as a change's own.
- **Setup is real work:** a bridge, bindings, and calibrating each
  repository.
- **Trials need headless Claude Code** for their builder and scorer.
- **It's Unix-only, and one machine.**

Code: src/acr/flow.rs::settle; src/bridges/acrbus-pi.js; src/setup.rs::confine_settings; src/acr/flow.rs::ruling; src/acr/flow.rs::same_defects; src/acr/plan.rs::estimate; src/acr/stages.rs::test_modules; src/acr/trial.rs::BUILDER_MODEL | Config: costs.per_call | CLI: acrbus acr trial --arm | Tests: only_lenses_with_blockers_are_attacked_again_by_new_reviewers, a_relay_is_only_for_a_review_and_only_an_agent_that_can_be_steered_takes_it, findings_go_ungrouped_and_say_so_when_the_grouping_fails
