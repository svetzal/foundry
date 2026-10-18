# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/), and this
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Computed initiative readiness in human and JSON reads, with typed blockers,
  exact dependency evidence and live ledger/registry checks. Offline reads are
  explicitly store-only. Steering and admission share the readiness rules.

- Typed operational recovery on campaigns and initiatives, with exact recovery
  IDs, causes, blocker details and evidence-backed rechecks. `campaign recover`
  and `initiative recover` request reassessment without recording owner policy,
  changing budgets, clearing questions or releasing owner controls. New RPCs
  refuse older daemons without unsafe fallbacks.
- `blocked_on_recovery` task verdicts and explicit `recovery_kind` in executor
  blocker files distinguish missing capabilities, access, environment,
  evidence and prerequisites from genuine owner decisions. Blocked work stays
  preserved. Legacy blocker files retain their decision semantics.

- `initiative note <name> --text "<status>"` and `RecordInitiativeStatus`
  record durable observations separately from binding owner decisions.
  Notes appear in reads and assessment context. They do not answer questions,
  resume work, release holds, change budgets, or request steering.

### Fixed

- Bare initiative dependencies wait for a campaign's completed outcome instead
  of any first landed cycle. Explicit `member:landed` keeps intentional
  first-landing ordering; named evidence milestones remain available.
- Conditional owner holds release when every declared prerequisite is proved.
  Indefinite owner holds and all other authority controls remain binding.

- Runner failures no longer masquerade as owner decisions. Campaign recovery
  retains the failed result and preserved work without replaying that failure
  on an evidence-backed recheck.
- Rejected initiative completion records exact unmet prerequisites instead of
  creating a generic owner question. Independent eligible members can continue;
  fresh passing completion checks can resolve evidence/precondition recovery.

- Initiative decisions answer only explicitly selected question IDs through
  `decide --question <id>` and `RecordInitiativeDecision` with `question_ids`.
  A policy decision without question IDs preserves every question. Answering one
  question preserves unrelated questions, including those for the same member,
  and members with unanswered questions stay blocked. Open and deferred IDs
  appear in `initiative show`. Existing clients must pass IDs to answer.
  Updated CLI decisions use a distinct RPC so older daemons reject them
  without falling back to member-wide question clearing.

## [0.53.1] - 2026-10-17

### Changed

- Initiative contract-testing guidance no longer asks a library to prove
  compatibility with its dependents before release. A library distributed
  through a package registry self-validates with its own tests that consume it
  through its public API and packaged artifact, against its contract documents
  and fixtures, plus a fresh consumer install. Dependents verify when they adopt
  the released version. Paired producer and consumer verification still
  applies to boundaries between running applications or services. On
  2026-10-17 the Mojentic 3.0 port campaigns kept failing review for missing
  "paired" verification that only unreleased consumers could supply.

### Fixed

- A campaign cycle that cannot reach its acceptance boundary now asks the
  owner instead of failing as a missing-proof defect. On 2026-10-17 a Swift
  cycle needed macOS, found only Linux hosts, and stopped without proof.
  Review called that a `defect`, so the next cycle ran the same blocked
  objective again. The executor prompt now offers a third shape: when a
  capability, access or owner decision is missing, the executor records
  `.foundry/blocker.json` (`kind`, `tried`, `missing`, `question`, optional
  `options`) and does not fabricate evidence. Review reads a valid blocker
  before the proof and returns `blocked_on_decision` with the executor's own
  question, what is missing and what was tried, plus its options or a default
  pair. The campaign escalates with that question and the work item settles
  `needs_decision`. An invalid blocker is a `defect` that names
  `.foundry/blocker.json`. Without a blocker, the proof check is unchanged.

## [0.53.0] - 2026-10-16

### Fixed

- A failed gate keeps enough output to diagnose it. A gate's `output` used to
  be the last 200 lines of stdout followed by stderr, so a tool that writes
  many compile lines to stderr pushed every stdout line out. A failed
  `cargo tarpaulin --fail-under 56` gate recorded only compile lines, and the
  failing test name was lost. Now stdout and stderr are tailed separately and
  share the 200-line budget: each keeps at least half when both overflow, and
  a stream that needs less gives its unused lines to the other. A
  `--- stderr ---` line separates them when both have content. `output` stays
  one string, so stored events and readers still deserialize.
- Campaign formation and the task reviewer shorten failed gate output section
  by section, so the stdout failure lines survive a flood of stderr.

### Added

- Full logs for failed gates. When a gate fails, the complete stdout and
  stderr of each command it ran (and the fix and re-check runs when self-heal
  was attempted) are written under `~/.foundry/evidence/gate-logs/`, one
  directory per gate run named by UTC timestamp and uuid. `GateResult` has a
  new `logs` field with the absolute paths. It is omitted when empty, so
  passing results serialize as before. Each stream is capped at its last
  8 MiB. After each write, run directories older than 14 days are deleted,
  then the oldest are deleted until the tree is at most 1 GiB. A write failure
  logs a warning and never changes the gate's result. Retry prompts, the task
  reviewer, campaign formation and red done-gate objectives, maintenance triage
  evidence, land-blocked and rebase gate failures, supply-chain rollback
  details and `foundry validate` now name these files.

## [0.52.2] - 2026-10-16

### Fixed

- An initiative no longer stalls when a member advance settles, and its
  members now advance side by side up to `max_active_members`. Initiative
  `mojentic-3-0` idled on 0.52.1 with four members eligible: a steer advanced
  `mojentic-3-0-ru`, that advance escalated with an owner question for `ru`
  alone, and nothing steered again. Only an accounted cycle result or a
  standalone task woke steering at settlement, and the `campaign_escalated`
  fact skipped because the advance still held its slot. Work admission now
  requests an assessment when a successful member advance moves its member on
  while the precheck lists another member. Moving on means the member
  escalated, stood down, finished or became blocked, or, for an advance a steer
  issued, dispatched a cycle (its dispatched objective and queued cycle item
  hold its slot). So each settled steer-issued formation asks for the next
  member until the slots are full or nothing else is eligible. The precheck is
  recomputed without the settled advance, its slot reservation and the
  initiative's own assessments. A failed advance, a formation that another path
  asked for and that dispatched a cycle, and an advance that left its member
  eligible request none, so a member cannot loop.
- A steering pass recorded as `no_op` (a hold or relay already in force) now
  requests the one follow-up assessment an applied hold or relay requests,
  under the same bounds: never from a follow-up, never for an owner action,
  never while another assessment is queued, running or coalesced behind the
  steer, and only when a member is eligible. On 0.52.1 a fresh pass after a
  stale relay chose the same relay, recorded `no_op`, and the initiative idled
  for about twenty minutes with four members eligible. The 0.52.0 rule that
  no-op passes request no follow-up is withdrawn. A follow-up is now also
  withheld while a fact waits coalesced behind the running steer, so the
  follow-up no longer replaces that fact's request.
- The `escalated` initiative status was confirmed to gate no steering path. A
  new test pins that an escalated initiative still lists a member without an
  open question as eligible, with its question surface full.
- Tests that write a stand-in script and then run it no longer fail at random
  with "Text file busy". GitHub CI run 38902421734 failed
  `a_failed_mix_deps_get_is_reported_and_names_the_project` this way. The
  tests wrote the script with `std::fs::write` in the test process. A sibling
  test that forked during that write gave its child a copy of the write
  descriptor, and on Linux `execve` refuses a file while any process holds it
  open for writing. A new `foundry_sdk::test_support::write_executable`
  (behind the `test-support` feature) writes the file from a child shell, so
  the test process never holds a descriptor to inherit. All six tests that
  ran a script they had written now use it, including the two that already
  wrote from a child shell. Test-only change; no production behaviour differs.

## [0.52.1] - 2026-10-16

### Fixed

- An initiative steer no longer waits "repository busy" behind a member cycle
  on any initiative repository, so the next member starts beside a running
  cycle. A steer reserves every initiative repository only if all are free
  when it starts: pacing settles this at the start (the work item's new
  `reservation_if_free` flag), not at admission, so work that starts on a trunk
  while the steer waits for its own key, host capacity or a lane cannot hold
  it back. A steer that starts without the trunks skips integration (no trunk
  sync, gate, result or regression finding) and records why as
  `integration_skipped` on its steering record, which `foundry initiative show`
  now prints. `pacing show` marks a waiting item's optional reservation as
  `(and … if free)`.

## [0.52.0] - 2026-10-16

### Added

- A steering step that changes state but dispatches nothing (a hold, a relay
  that adds a recipient, an ask, an escalation or a staged campaign) now
  requests exactly one follow-up assessment when the precheck, recomputed after
  the step, still lists an eligible member. Before, the initiative idled with
  that member until the next settled fact: on 2026-10-16 the first steer of
  `mojentic-3-0` relayed a finding while `mojentic-3-0-ex` was eligible, and
  every member stayed staged. The request is an ordinary
  `initiative_advance_requested` through the work ledger, so pacing, the
  precheck and the one-assessment rule apply, and a queued or running
  assessment absorbs it. It carries `follow_up_of` (`steer_id` and `step`),
  which the follow-up's steering record, its `initiative_steered` fact and
  `initiative show` keep. A follow-up never requests another. Wait, no-op,
  skipped, rejected, stale and failed passes, `done` proposals and owner
  actions request none.

## [0.51.0] - 2026-10-16

### Added

- Initiative steering checks in code what a pass could do before it asks an
  agent. The packet now carries an `eligibility` section: the members that can
  advance now, every other member with its reason (owner hold, unmet milestone
  or prerequisite, open question, no free slot, spent budget, busy repository),
  the members using the active-member slots, and whether a task can start. An
  automatic steer with no eligible member, no free task slot, at least one
  member running, and no new relayable finding or owner decision since the last
  steering decision starts no agent. It records `initiative_steered` with
  outcome `skipped` and the reason, and its work item lands. An owner's
  `initiative advance` always starts an assessment. On 2026-10-15 an
  initiative ran about 76 assessments of roughly 50 seconds each, many while
  both slots were busy.
- A `wait` steering decision, with a required reason. It is recorded in the
  steering history and on `initiative_steered` (outcome `waited`), opens no
  question, leaves the initiative status alone and lands its work item. The
  prompt now tells the agent to use `wait`, not `ask`, while workers finish.
  A wait while members are eligible is recorded beside the eligible list
  (`eligible_members`) rather than refused, so the agent keeps its judgement.

### Changed

- A steering decision that is already in force is a no-op, not a failure. A
  `hold` on a member with an owner hold keeps the owner hold, and a `relay` to
  members a finding already reaches adds nobody. Both are recorded with outcome
  `no_op` and their reason, request no action, spend no budget and land the
  work item. Before, the hold settled `failed` with "steering cannot replace an
  owner hold" although the agent agreed with the owner.
- A steering decision refused by a guard (held member, active-member limit,
  busy repository, project outside the initiative, owner-pinned member, invalid
  audit evidence) now has outcome `rejected`, and its work item settles
  `cancelled` with the reason `steering rejected: <guard message>`. It used to
  settle `failed`, so `foundry queue open` listed refusals as work that needs a
  person. Agent crashes, unusable output and store or ledger faults still
  settle `failed`. `initiative_steered.outcome` is now a typed value
  (`applied`, `waited`, `no_op`, `skipped`, `rejected`, `stale`, `failed`)
  and the fact gains optional `reason` and `eligible_members` fields.

- `foundry campaign decide` now records an owner decision on an `active` or
  `paused` campaign, not only an `escalated` one. An owner who rules on policy
  while a campaign runs can record it at once, and the next formation treats it
  as binding. On an active or paused campaign the decision is appended and
  nothing else changes: the status, budget, open escalation record and pending
  run result stay as they are, and a spent budget does not block it. A decision
  recorded while the campaign forms makes that formation form again from it.
  `--add-cycles` there is refused with "campaign '<name>' is '<status>'; decide
  adds cycles only to an escalated campaign; ..." (`FAILED_PRECONDITION`),
  pointing a paused campaign at `foundry campaign resume <name> --add-cycles N`.
  An escalated decide is unchanged. `campaign show` offers `decide` for
  authorized active and paused campaigns.
- A retried `decide` is now refused because it repeats the campaign's latest
  decision: "campaign '<name>' already records this owner decision as its
  latest; nothing to apply" (`FAILED_PRECONDITION`). The check compares the
  trimmed text and the owner, applies to active and paused campaigns, and runs
  before the `--add-cycles` check, so a retried escalated
  `decide --add-cycles N` still adds no cycles. A staged, completed or
  cancelled campaign refuses `decide` with "decide requires Active, Paused or
  Escalated status".
- The initiative control contract is now version 3
  (`contracts/initiative-control-v3.json`). In the `budget` group,
  `decide-campaign-not-escalated` is replaced by `decide-campaign-retry` (the
  duplicate refusal), `decide-active-campaign-with-cycles` (refused) and
  `decide-active-campaign` (records a second decision and stays active). A new
  `steer-by-decision` group covers a paused campaign and the completed and
  cancelled refusals. Version 2 is removed: the daemon no longer honours its
  `decide-campaign-not-escalated` refusal.

### Fixed

- A completed or cancelled member campaign no longer holds one of its
  initiative's active-member slots through stale records. A campaign records a
  cycle's result on the advance after the cycle settles. A campaign cancelled
  with `--now`, or with a queued cycle that the cancellation withdrew, never
  takes that advance, so its last objective kept no result and the member
  counted as running for good. With the default limit of two, one such member
  halved the initiative's capacity, and steering refused the remaining member
  with "initiative '<name>' has reached its active member limit". A terminal
  member's stale reservation or queued or held ledger item no longer counts
  either. A cycle still running after a graceful cancellation keeps its slot
  until it settles, because it uses a real worker. The same stale objective no
  longer blocks a completion proposal. Every admission check (steering,
  formation, owner advance and action admission) now counts occupancy through
  one store function, and the ledger side through one more, where four copies
  existed before.
- Cancelling a queued formation or cycle of an initiative member now releases
  the admission reservation it held. `foundry queue cancel` and the queued
  cycles that `foundry campaign cancel` withdraws used to leave it in place,
  because only a completed traversal released it, so a live member counted
  against `max_active_members` until the next steer or a daemon restart.
  For an item a steer admitted, `queue cancel` locks the campaign store before
  the ledger, saves the ledger, then the store; if the store save fails the
  reservation stays and the error says so. Other cancels do not touch the
  campaign store. `campaign cancel` releases it in the same store save as the
  cancellation.
- `foundry initiative decide` no longer writes owner decisions to completed or
  cancelled members. The decision fans out only to members that can still act
  (staged, active, paused or escalated), and a shared budget extension raises
  only their budgets. Naming a completed or cancelled member with `--member`
  refuses the whole decide with "'<member>' is '<status>'; decide reaches only
  staged, active, paused or escalated members" (`FAILED_PRECONDITION`) and
  records nothing, as `foundry campaign decide` refuses those campaigns. With
  no member left to act, the decision is recorded on the initiative only, with
  no applicable member. The initiative control contract version 3 gains a
  `decide-ended-member` group (seed `ended-member`) covering both refusals and
  the fan-out that skips a cancelled member.

## [0.50.2] - 2026-10-13

### Fixed

- The test suite lints clean on all targets again. 0.50.1 was tagged with a
  clippy failure in test code, so its Linux release build stopped at the lint
  gate and it was never deployed. 0.50.2 carries the same runtime changes.

## [0.50.1] - 2026-10-13

### Fixed

- `foundry initiative decide` on a spent shared budget now requires
  `--add-cycles`, as `foundry campaign decide` does on a spent campaign. It
  used to accept the decision and record it on every member, but nothing
  unblocked: the member that escalated on budget stayed escalated and the same
  question reopened with the same id. Without `--add-cycles` it is now refused
  with "initiative '<name>' exhausted its cycle budget; pass add_cycles > 0 to
  authorize more work" (`FAILED_PRECONDITION`) and records nothing. Refusals
  for an unknown, terminal or unauthorized initiative, an empty decision and an
  outside member still come first. Completing an initiative is unaffected.
- `foundry campaign decide` and `resume` without `--add-cycles` on a spent
  initiative member now name the initiative: "campaign '<name>' belongs to
  initiative '<initiative>', whose cycle budget is spent; decide on the
  initiative with add_cycles > 0". They used to say "pass add_cycles > 0",
  which the member then refused because the initiative owns its budget.

### Changed

- The initiative control contract is now version 2
  (`contracts/initiative-control-v2.json`). In the `budget` group,
  `decide-initiative-spent-without-cycles` is a refusal (status 9, store
  unchanged), `decide-member-campaign-spent-without-cycles` carries the new
  message, and `decide-initiative-adds-cycles` records one owner decision
  instead of two. Version 1 is removed: the daemon no longer honours it.

## [0.50.0] - 2026-10-13

### Added

- Slice 3 owner attention: a shared question limit, coalesced member questions
  with their earliest unanswered age, and a count of questions waiting for a slot.
  Steering advances unblocked members at the limit or waits for the owner.
- Initiative questions older than one day and initiatives inactive for thirty
  days open the ops digest's anomaly gate.
- A shared executable contract for initiative listing, details and scoped
  decisions, verified by Foundry and ops-visualizer.

### Fixed

- Refreshing member questions or answering only some members no longer resets
  the age of a question that remains unanswered.
- Ops digest summaries use an empty temporary directory for their read-only
  agent call. A daemon started at `/` can now apply Claude's scoped policy.

### Changed

- The bundled skill tells briefs to state guarantees the code can actually
  make, with the threat and accepted residual named, because a reviewer holds a
  task to every requirement literally and an impossible absolute fails review
  repeatedly. It also points follow-ups at `--continues <id>`.

## [0.49.0] - 2026-10-12

### Changed

- Initiative steering and member agents receive a shared contract-testing reminder.
  Provider and consumer verify the same versioned contract set and retain both
  results. The initiative guide and story distinguish contract compatibility
  from deployment wiring and the user's full journey.

### Added

- Slice 2 initiative outcome evidence: host-named integration checks on
  disposable copies of current trunks, retained command logs and automatic
  findings for failed checks. Tracker derivation accepts a gate definition file
  with `--integration-gates`.
- Steering can stage a campaign for uncovered work and propose `done` with
  evidence for every outcome review. Completion requires current integration
  results and an explicit owner confirmation.
- Confirmed completion closes verified Tracker interactions and the linked
  goal through the Tracker CLI. Durable receipts support partial-failure
  recovery without reporting a failed closure as completion.
- `foundry registry list --json` provides daemon-owned project paths for
  integration checks on named hosts.

### Fixed

- The canonical installer now uses the tested dependency lockfile for both
  binaries instead of resolving a different dependency set during installation.

## [0.48.0] - 2026-10-12

### Added

- A story-led book section based on Foundry's Tracker journeys. Fictional
  situations cover owner attention, preserved work, campaign decisions,
  cross-repository outcomes and safe promotion of Foundry itself.
- Slice 1 initiative steering: one read-only assessment chooses `advance`,
  `dispatch`, `hold`, `relay`, `ask` or `escalate`. Explicit advance enables
  automatic steering after member settlement. Existing initiatives stay passive.
- Shared member admission, cycle grants, owner holds and milestone checks.
  Durable steering receipts prevent duplicate work during replay and recovery.
  Owner decisions can extend the shared budget with `--add-cycles`.
- First-steer codebase audits and refreshed Tracker intent on every steer.
  Findings retain checked paths and `realised`, `partial`, `absent` or
  `misplaced` classifications. Realised intent produces close proposals only.
- Explicit provider pairings for alternating formation, execution and review.
  Member stage overrides and continuation provider choices remain binding.

### Changed

- Correct the slice boundary in initiative documentation. Integration gates,
  general campaign staging, outcome evaluation and Tracker closure are slice 2.
  Slice 1 was validated on the Mac development daemon before release.

## [0.47.0] - 2026-10-12

### Added

- Slice 0 of initiatives: a shared outcome, member campaigns, owner decisions,
  budgets and open questions in the campaign store. Typed daemon commands and
  explicit offline recovery cover `initiative add/list/show/decide`,
  `initiative complete/cancel`, `brief set/show` and `finding add/list`.
  Decisions fan out to active
  and escalated members. The open queue groups member escalations under their
  initiative, and `queue --source initiative:<name>` lists initiative work.
- Foundry-owned briefs with SHA-256 identity, typed findings from owners and
  task results, and bounded findings in formation and continuations. A finding
  is consumed for a member only when work formed from it lands.
- Per-stage providers for campaigns and initiatives. Formation, execution and
  review select their own provider; campaign reports retain tokens by stage
  and provider. Claude execution uses the tested write policy with worktrees
  in their existing location. Campaign admission checks provider capability
  rather than requiring Codex by name.
- Read-only initiative derivation from Epilogue Tracker goals and journeys,
  using committed projections or the Tracker CLI. Derived intent stays staged.
  Steering and the codebase audit remain for slice 1. Tracker completion and
  integration gates on the production daemon's host remain for slice 2.
  This release promotes
  slice 0 to ops-01 through the manual release procedure.

## [0.46.0] - 2026-10-11

### Fixed

- A campaign cycle no longer continues from preserved work that was settled
  outside the campaign. On production, campaign
  mojentic-py-transient-recovery-v2 cycle 14 preserved its work; the owner
  cherry-picked it onto main by hand, closed the item with `foundry queue
  close`, recorded a decision to start from origin/main, added a cycle and
  advanced. Cycle 15 still tried to continue from cycle 14's ref, the
  continuation guard refused it (4 commits behind trunk, conflict in
  `RECOVERY-CONFORMANCE.md`), and the owner's cycle was spent with no agent
  session. Cycle 13 had failed the same way on cycle 12's closed ref. The
  manual advance replayed the campaign's pending run result, and neither
  `queue close` nor supersession detection touches the campaign store.
  Formation now reads the work-item ledger: when the campaign's cycle item
  whose disposition records the ref is settled `cancelled` or `landed`, the
  next cycle starts from the registered trunk, and formation's prompt names the
  ref, the item, its state and its reason so it does not treat that work as
  pending. An item still open keeps carrying its work forward. An unreadable
  ledger keeps the old behaviour, with a warning. `AdvanceCampaign::new` takes
  the ledger path as a new argument.
- When the last cycle the budget allowed was a refused continuation, the
  campaign's escalation reason now says that the cycle was refused before any
  agent session ran, includes the refusal summary (ref, preserved commit, how
  far behind trunk, conflicting files), and says what the owner can do: add a
  cycle and advance from trunk, and close the preserved item if its work was
  reconciled by hand. It used to read only "campaign cycle budget exhausted (N)
  after an unlanded non-complete result". The refused cycle is not refunded.
- `foundry campaign decide` takes `--add-cycles <n>`, and on a spent budget it
  requires it. A budget-exhausted escalation offered both `decide` and
  `resume --add-cycles`. An owner who ran `decide` got an `active` campaign
  whose next advance escalated on budget again, and `resume --add-cycles` then
  failed with "campaign is 'active'; resume requires Paused or Escalated
  status". `decide --add-cycles <n>` now records the decision, adds the cycles
  (checked addition) and returns the campaign to `active` in one transition.
  When `cycles_completed >= max_cycles`, `decide` without `--add-cycles` is
  refused with "campaign '<name>' exhausted its cycle budget; pass add_cycles >
  0 to authorize more work" and records nothing. `decide` still accepts only an
  escalated campaign, so a retry after a success is refused and adds no cycles.
  `DecideCampaignRequest` has a new `add_cycles` field (tag 3); older clients
  send 0. The owner options of an escalated campaign with a spent budget now
  print `foundry campaign decide <name> --decision "<text>" --add-cycles <n>`.
- `foundry campaign advance --offline` on a paused or escalated campaign now
  lists that campaign's owner options. It used to suggest plain `foundry
  campaign resume <name>` "when the escalation was budget-only", which the
  engine refuses on a spent budget without `--add-cycles`.
- Formation no longer escalates because its read-only sandbox cannot run
  cargo. After a landed cycle on the development daemon, formation escalated
  with "Clippy and cargo test cannot run because the read-only sandbox denies
  opening target/debug/.cargo-build-lock" and asked the owner for a writable
  validation environment, although the campaign's required done-gate had
  passed and the cycle had landed only after the project's required gates
  passed. The formation prompt now says the sandbox is read-only, that builds,
  tests and linters must not be attempted there, that test and gate evidence
  comes from the done-gate results and the last run result (a run with
  `landed: true` reached trunk only after every required gate in the project's
  gate file passed in its worktree), and that being unable to run a command
  there is never a reason to escalate.

## [0.45.1] - 2026-10-11

### Fixed

- A continuation (a campaign cycle after an unlanded cycle, `foundry queue
  resume`, a nightly majors continuation) now starts on current trunk. On
  2026-10-11 mojentic-sw cycle 15 continued from a branch preserved on a base
  three commits behind trunk, worked about 17 minutes, and then failed at
  finalize with "trunk advanced during the run", although trunk had not moved
  during the run. mojentic-py cycle 12 failed the same way that evening.
  Workspace preparation now fetches the registered trunk and, when the
  preserved commit does not contain it, rebases the preserved commits onto
  trunk in the new task worktree before the agent starts. The preservation ref
  is not rewritten. A continuation that already contains trunk starts exactly
  where it did before. When origin cannot be fetched, the local branch is the
  trunk, so bundle continuations still work while origin is down.
- A continuation whose rebase onto trunk conflicts is refused before any agent
  session, instead of running a full session on a stale base that finalize
  cannot land. An agent may not rewrite history, so it cannot repair the
  conflict; finalize would replay the same commits and conflict again. The
  refused run ends `runner_error` with the new `land_blocked` value
  `continuation_conflict`, no `preservation_ref`, and a summary that names the
  continuation ref, the preserved commit, how far behind trunk it was and each
  conflicting file. Its work item settles `failed` with that reason; the
  preserved work and the item that preserved it are untouched. A campaign does
  not escalate this as a runner fault: formation runs from trunk with the
  refusal in its prompt and decides whether to reapply the work, cut another
  objective, or escalate.
- Finalize no longer reports every rebase as "trunk advanced during the run".
  The executor records where each task worktree started (`workspace_base`:
  its first commit and the trunk commit it observed) and carries it on the
  run's payloads. When that start already lacked trunk, a blocked landing
  reports the new `stale_base_conflict` or `stale_base_gates_failed`, and a
  landed summary says it rebased onto commits "that the run's starting base
  lacked". A run with no recorded start keeps `trunk_moved_conflict` /
  `trunk_moved_gates_failed`, with a detail that says the cause is unknown.
  Existing `land_blocked` values are unchanged on the wire.
- A second `foundry campaign advance` for a campaign no longer starts a
  second advance workflow. On 2026-10-11 an operator retried a hung advance on
  production and got two advance workflows for one campaign, which had to be
  cancelled by hand. The `AdvanceCampaign` RPC checked only the campaign
  status. It now also refuses to duplicate work in flight: when the campaign
  already has an advance or a cycle submitted, queued, held or running in the
  work-item ledger (cycles by their `campaign:<name>` source, advances by a
  `CampaignAdvanceRequested` root naming the campaign), or an accepted advance
  or cycle workflow in the tracker not yet admitted to the ledger, it
  dispatches nothing. `AdvanceCampaignResponse` has a new `in_flight` field
  (`CampaignWorkInFlight`: kind, work item id, state, root event id) and an
  empty `event_id` in that case; existing fields and numbers are unchanged.
  The check and the dispatch run under the same campaign store lock, and the
  tracker records the workflow before the lock is released, so two
  simultaneous advances for one campaign dispatch exactly one workflow. The
  CLI prints the existing item with `next: foundry queue show <id>` and exits
  0 without watching. The automatic advance after a cycle is unaffected.
- `foundry campaign advance` now follows only its own advance. It watched the
  whole project and stopped at the first `campaign_advance_completed`, so on
  2026-10-11 two clients printed and exited on another campaign's formation.
  It now follows the advance's own trace and also stops when its work item
  settles without a campaign event, which is what an advance that fails
  without one produces (one whose campaign changed during every formation).

- A campaign formation no longer blocks every other campaign. On 2026-10-11
  five concurrent `foundry campaign advance` calls on production, one per
  campaign, admitted three; the other two hung for minutes and were admitted
  only as other formations finished. Formation held the campaign store's
  cross-process file lock across its done gates and its decision agent, and
  the `AdvanceCampaign` RPC (like every other campaign control) needs that
  lock to validate. A process-wide mutex also serialised formations for
  different campaigns. Formation now holds the file lock only for two short
  sections: it records the arriving run result and saves it, releases the
  lock, runs the gates and agent, then locks again and applies the outcome
  only if the campaign record is unchanged since it read it. A campaign paused,
  cancelled, completed or escalated while it formed keeps that status, the
  outcome is discarded, and the advance stands down (see the next entry). A
  campaign edited in any other way (a budget or mission change) keeps the
  edit, and the advance forms again from the edited record, still holding
  that campaign's formation lock. The second pass re-runs every check the
  first made, so a budget cut mid-formation is enforced; it records the
  arriving run result only once, so no cycle is counted twice. After three
  formations that each found the record changed, the advance fails with a
  summary that says so, and its work item shows in `foundry queue open`. The
  process-wide mutex is now one mutex per campaign, so different campaigns
  form at the same time and one campaign never forms twice at once. Tests in
  `crates/foundryd/tests/campaign_advance_concurrency.rs` reproduce the hang.
- An advance that stands down because its campaign was stopped now settles
  `landed`, not `failed`. Before, an advance that found its campaign paused or
  cancelled (when the cycle's result arrived, or while it formed) emitted no
  `campaign_advance_completed`, so the work-item ledger settled it `failed`
  and `foundry queue open` listed it as needing a person, although it had done
  what the operator asked. It now emits `campaign_advance_completed` with
  `decision: "stood_down"`, `campaign_status` (the status that stopped it) and
  a `reason`. A stand-down dispatches nothing, changes no status and counts no
  cycle; a cancelled campaign stays `cancelled` and a paused campaign keeps the
  arriving result pending for the advance after `resume`. The same applies
  when the campaign was completed or escalated while it formed. The payload
  also gains `success`: `true` for a decision or a stand-down, `false` for a
  fault (campaign store unreadable, save failed, campaign missing). Those
  faults emitted an `escalate` completion with no `success` field, so their
  work items settled `landed`; they now settle `failed`. Events written before
  the field read as `success: true`. `failed` is now reserved for faults.
- The work-item ledger no longer reports a workflow that succeeded as failed
  because it read the workflow's result while it was being written. The
  ledger and the trace store both write `{root}.json` for a started root, and
  the ledger reads it back the moment the item settles. A read during the
  other writer's truncate-and-write failed to parse, so the waiting caller got
  a synthesized failure terminal (for an advance, a `campaign_advance_completed`
  with `decision: "escalate"` and `success: false`) beside the real one. Trace
  files are now written beside their destination and renamed into place.

- The `foundryd` single-instance tests no longer fail at random. The 0.45.0
  release build on `mojility-ops-01` failed once because the lock-held test
  reported "address already in use" instead of "already running". The test
  picked a free port, released it, and another test in the same binary bound
  that port before the spawned `foundryd` could. `foundryd` binds its address
  before it takes the instance lock, so the test hit the bind refusal. The
  tests in `crates/foundryd/tests/single_instance.rs` now run one at a time on
  a lock that every test `Home` holds. No assertion changed, and `foundryd`'s
  start-up order is unchanged.

## [0.45.0] - 2026-10-11

### Fixed

- `foundry campaign show` now says why an escalated campaign waits on its owner
  and what the owner can run. On 2026-10-11 four mojentic recovery campaigns
  sat `escalated` on production, and no operator surface showed the reason: it
  lived only in the `campaign_escalated` event. Formation now records the
  escalation on the campaign itself (`open_escalation`: reason, time, cycles,
  and the task agent's question and choices for a `blocked_on_decision`), in
  the same store save as the status. It is present only while the campaign is
  `escalated`; `decide`, `resume`, `complete`, `cancel` and `pause` clear it.
  `GetCampaign` and `ListCampaigns` carry it as a new `CampaignEscalation`
  message. A campaign that escalated before this release, such as the four on
  production, has no record, so reads recover the reason from the newest
  `campaign_escalated` event in the event log and say so; nothing is written
  back. `campaign show` prints an `Escalation:` block and `Owner options:` with
  the real command lines from the status table (`decide`, `resume`, `complete`,
  `cancel` for an escalated campaign, `resume --add-cycles <n>` when the budget
  is spent). `campaign report` prints the same, and its `--json` gains
  `open_escalation` and `owner_options`. `campaign show` gains `--json`.
- `foundry queue open`, and the open group of the `foundry queue` overview, now
  list every escalated campaign. On 2026-10-11 four mojentic recovery campaigns
  were escalated on production but `queue open` showed two items: the other
  two (`mojentic-ex-transient-recovery-v2`, `mojentic-sw-transient-recovery-v2`)
  escalated after their final cycle landed, so the ledger held no open item for
  them. An escalation is campaign state, not executed work, so it is not
  written into the ledger. The CLI reads it from `ListCampaigns` (offline: the
  campaign store, with the event-log fallback for older escalations) and
  renders it as its own row: `campaign <name> <project> escalated <when>
  <reason>`, then `next: foundry campaign show <name> (<controls>)`. Every
  escalated campaign is listed, including one with an open cycle item; the row
  names that item as `(open cycle <id>)`. `--source campaign:<name>` keeps that
  campaign's row, and other source kinds show none.
- `--source` on `foundry queue` is accepted after `open` as well as before it,
  as the documentation already showed (`foundry queue open --source …` was
  refused).
- The advisory-suppression guard no longer stops a run for a flag that is
  only quoted. It read every added line in every file, so on 2026-10-11 a
  mojentic-py campaign cycle needed review for "a pip-audit --ignore-vuln
  flag" in an archived controller log under `evidence/` that quoted the
  guard's own earlier rejection, although the repository contains no
  `--ignore-vuln`. An earlier cycle was stopped for a brief that forbade
  ignore flags. The guard now reads added lines only in files where a
  suppression takes effect: the auditors' configuration files, and files
  that run commands (`.hone-gates.json`, CI workflows, scripts, Makefiles,
  justfiles, `package.json`, the Python task runners' configuration). Logs,
  evidence archives and documents are not read. Every suppression the guard
  caught in those files before, it still catches, with the same message.

### Changed

- **JSON shape for scripts:** `foundry queue --json` and `foundry queue open
  --json` now emit an object, `{"items": [...], "escalated_campaigns": [...]}`,
  instead of a bare array of items. Read `.items[]` where a script read `.[]`.
  Each `escalated_campaigns` entry carries `name`, `project`, `status`,
  `cycles_completed`, `cycles_landed`, `max_cycles`, `open_escalation`,
  `open_cycles` and `owner_options`. Keeping campaigns out of `items` means a
  consumer that counts ledger items never counts a campaign. `queue show
  --json` is unchanged.
- Each owner option (`campaign show --json`, `campaign report --json`, the
  queue's `escalated_campaigns`) carries a `control` key naming the control it
  runs.

## [0.44.4] - 2026-10-11

### Fixed

- A task or campaign cycle in a repository whose `.gitignore` ignores
  `.foundry/` commits again. Staging used a `:(exclude).foundry` pathspec, and
  git rejects any pathspec that names an ignored path ("paths are ignored",
  exit 1), so every commit in such a repository failed (three Bedrock
  repositories from 2026-10-10). The worktree is now staged with
  `git add -A -- .`, which skips ignored paths, and the index under `.foundry`
  is then reset to HEAD. What lands is unchanged: nothing new or edited under
  `.foundry`, only deletions of files HEAD tracks there.

## [0.44.3] - 2026-10-11

### Fixed

- A workflow that recovered from a retried failure now settles `landed`. Since
  0.43.0 a work item settled `failed` whenever any block in its chain failed,
  so a nightly maintenance run whose first gate attempt failed and whose retry
  passed was recorded as failed although its completion reported success and
  its work was pushed (four items on 2026-10-11). A workflow with a terminal
  event is now judged by its last terminal; a block or outcome event that fails
  after that terminal, such as the local install, still fails the item.

## [0.44.2] - 2026-10-11

### Added

- `FOUNDRY_GATE_TIMEOUT_SCALE` on the daemon stretches every gate timeout on
  that host, both a gate's own `timeout` and the ten-minute default, so gate
  files stay calibrated to the fastest development machine and a slower host
  sets one factor (ops-01 uses `2.0`). Unset or invalid means `1.0`.

### Changed

- Foundry's own coverage gate may run for 40 minutes instead of 20.

## [0.44.1] - 2026-10-11

### Changed

- serde_json 1.0.151 to 1.0.152 (nightly maintenance).

### Fixed

- The run-completion gRPC tests isolate their git fixture from inherited git
  configuration, such as an agent session's `remote.origin.pushurl` override,
  so a local push in the fixture succeeds when the suite runs inside an agent
  session or under coverage.

## [0.44.0] - 2026-10-11

### Added

- `foundry task <project> "<description>" --continues <id>` (repeatable)
  records that a follow-up task continues open work, such as fixing the review
  findings on an earlier item's branch. Before, the follow-up was a plain task
  whose objective named the branch, nothing recorded the lineage, and the
  original stayed open until someone closed it by hand. Each id must be an open
  item (`preserved`, `needs_decision` or `failed`) of the same project; an
  unknown id is `INVALID_ARGUMENT`, another project's item or a settled one is
  `FAILED_PRECONDITION`, and a refused dispatch admits nothing. The new item
  records the ids in `continues`, shown by `queue show` (human and JSON), the
  queue overview and lifecycle payloads. When it lands, each continued item
  still open settles `landed` with `landed_commit` set and the reason
  `superseded by <commit> via <new item id>`, on its own trace, followed by the
  guarded branch cleanup. It is lineage only: the task's base and objective
  are unchanged, it does not wait (use `--after`), and a continuation that
  does not land leaves the continued items open. `queue resume` is unchanged.

### Changed

- A campaign's formation runs no longer stay open after the campaign moves on.
  Since 0.43.0 each advance is a `workflow` ledger item with the campaign's
  source, and one that ended `failed` stayed in the open list. When a later
  cycle of the campaign starts, an open formation run submitted before it
  settles `cancelled` with `carried forward by campaign <name> cycle <n> (<item
  id>)`; when the campaign completes or is cancelled, its open formation runs
  settle with the terminal reason. Only a cycle carries other items.

### Fixed

- A cancelled workflow retains its host slot and repository ownership until its
  traversal finishes. Closing an item refuses active cleanup. Immediate campaign
  cancellation records completion after abort and disposal; deferred joins wait
  for the owner to finish. Restart recovery releases interrupted ownership while
  preserving the recorded cancellation decision.

## [0.43.0] - 2026-10-11

### Changed

- All engineering workflows now enter the same durable queue before execution.
  Manual and nightly maintenance, campaign formation, releases, remediation,
  iteration, validation, supply-chain scans and digest agents obey the shared
  host cap and repository ownership. Downstream steps retain ownership through
  installation and cleanup. Independent derived tasks queue before it releases.
  Dry runs also queue because observers can invoke agents or checks. Mutator
  simulation stays active, and the settlement reason identifies the dry run.
- The nightly stages all project roots before waiting. Queued and held children
  survive restart and recovery rejoins their gather. Cancellation publishes a
  typed failed terminal so watchers and the maintenance cycle complete honestly.
- Supply-chain auto-fix reserves all active repositories for its sequential scan.
  A busy repository delays the scan instead of being skipped. Audit-only scans
  consume a system slot without project reservations.

- A campaign now settles the cycles it has moved past, so they leave the
  "needs a person" list. Before, nothing settled a cycle once the next one was
  formed from its preserved ref, and production showed four `preserved`
  cycles of one campaign open on the same commit while a fifth ran. When the
  scheduler starts a later cycle of the same campaign, before its agent runs,
  every earlier open cycle (`preserved`, `needs_decision` or `failed`) settles
  `cancelled` with the reason `carried forward by campaign <name> cycle <n> (<item id>)`. When the
  campaign completes or is cancelled, its remaining open cycles settle
  `cancelled` with `campaign <name> completed` or `campaign <name> cancelled`,
  plus the recorded reason. A gracefully cancelled campaign's in-flight cycle
  settles the same way once it finishes. Each settlement emits
  `work_item_cancelled` on the cycle's own trace, keeps the preservation ref
  and worktree fields, and records no `operator_action`. An escalated cycle
  stays open until the campaign advances again or ends. Items without a typed
  campaign source, other campaigns' items and settled items are never touched,
  and an owner action that reached the ledger first wins. A new block, Settle
  Carried Campaign Work, does this from campaign cycles' lifecycle events and
  the campaign's terminal events.
- Supersession now settles every open ledger item whose preserved work already
  reached trunk, not only `preserved` ones. The landing-triggered check and the
  scheduled reconciler both consider `preserved`, `needs_decision` and `failed`
  items that recorded a preservation ref. Before, a `needs_decision` item whose
  preserved tip a later campaign cycle had already landed stayed in the "needs
  a person" list indefinitely. Owner-cancelled items stay cancelled, an item
  changed after inspection is never overwritten, and an open item without a
  preservation ref is not a candidate. A `needs_decision` or `failed` run that
  made no commits still preserves its base commit, which every later trunk
  contains, so such an item settles only when its preserved tip was committed
  after the item started; otherwise it stays open with a diagnostic.
- Supersession has a third proof, content containment, tried after ancestry
  and `git cherry` patch equivalence. When `git merge-tree --write-tree`
  merges the preserved commit into trunk without a conflict and the result is
  trunk's own tree, trunk already holds every change the branch made. This
  recognises work a later cycle squashed or rebased with conflict resolution,
  which used to fail with "preserved commits have unmatched patches". A
  conflict, a merge that would still change trunk, missing objects, shallow
  or unrelated history, or unexpected output is never proof, and the
  diagnostic now gives the patch-equivalence and containment reasons
  together. The settled reason stays `superseded by <commit>`; the structured
  log and the reconciler's finding name the proof. Branch cleanup and the
  orphan-branch inventory share the proof, so an orphan branch can now read
  `content contained in <trunk>`. The proof writes only unreferenced loose
  objects and needs Git 2.38 or later.

### Fixed

- Maintenance commands wait for their own durable completion. Full maintenance
  waits until its queued summary finishes. Unrelated completions cannot end a
  waiting command.
- Linux release gates use temporary campaign, ledger, worktree and preservation
  paths, so fixtures cannot wait on production campaign locks.

### Compatibility

- The new `workflow` work-item kind and persisted start evidence extend the
  ledger schema. Older binaries cannot read records with this kind. Preserve
  the ledger and event stream before promotion. Prefer a forward fix after new
  work is admitted; do not discard queue records to permit a binary rollback.

## [0.42.1] - 2026-10-10

### Changed

- A quality gate (or its `fix_command`) without its own `timeout` now runs for
  up to 10 minutes, not 5. The 5-minute limit was the shell runner's general
  default for git and housekeeping commands, which gates inherited; a full test
  suite on a large workspace routinely passes it. Set `timeout` (seconds) on a
  gate in `.hone-gates.json` for a different limit.

### Fixed

- Task finalization no longer destroys files under `.foundry/` without a
  record. Before, it committed only the `.foundry` paths HEAD already tracked
  and then ran `git clean`, so evidence an agent wrote there (screenshots,
  JSON results, harnesses, logs under `.foundry/<topic>/`) disappeared. Now
  `.foundry/` in a task worktree is Foundry scratch and nothing under it lands
  in a task commit, on trunk or on the preserved branch. The one exception is
  a deletion of a file HEAD tracks there, which is committed so a task can
  remove legacy content. Projects that already track content under `.foundry/`
  keep it until a task or a person deletes it; an agent's edits to those files
  do not land. Before cleanup, finalize copies every `.foundry` file that did
  not land (new files, gitignored files, and the agent's version of edited
  tracked files) to `~/.foundry/evidence/<trace>/<attempt>/worktree/`, at its
  repository path, and names that directory in the task result's new
  `uncommitted_artifacts` field. It then restores the edited tracked files and
  removes the rest. A failed copy stops finalization before anything is
  removed or restored. The proof archive in the parent directory
  (`proof_evidence`) is unchanged. A task that must keep something in the
  project puts it in a normal project path, such as `docs/` or the test tree.
  Evidence under `.foundry/` lives in the evidence store and is referenced
  from the work item, not from repository files. Every task execution prompt
  now says this, and every task review treats project material left only
  under `.foundry/`, or repository files that cite `.foundry/` paths as
  committed, as a defect.

## [0.42.0] - 2026-10-10

### Added

- A pacing stage between a work item's admission and its start. Every
  task-shaped item (`foundry task`, a campaign cycle, a nightly majors-lane
  upgrade, a `queue resume` child) is recorded `queued` with a reason, and a
  scheduler inside `foundryd` starts it when the rules allow, evaluated each
  tick in order: one mutating item per repository (a running maintenance item
  counts), the host running cap from `~/.foundry/pacing.json`
  (`{"max_running": 2}` by default), `not_before` passed, every `depends_on`
  item settled `landed`, the lane not paused; interactive work first, then
  oldest submission first. The queued reason is the scheduler's current
  verdict (`repository busy: wi_…`, `host at capacity N/N`,
  `not before <time>`, `waits on wi_…`, `lane paused`, `ready`) and shows on
  queue rows, `queue show`, `pacing show` and in `--json`. A dependency that
  settles any way other than landed moves the dependent to `needs_decision`
  with `waits on <id>, which settled <state>`. Queued and held items survive a
  restart as they are; running items still settle `failed`.
- `foundry task --after <id>` (repeatable) and `--not-before <RFC3339|duration>`,
  also on `foundry queue resume`. An `--after` id not in the ledger is refused.
- `foundry queue hold <id>` and `foundry queue release <id>`, with the new
  `held` state and the `work_item_held` / `work_item_released` events;
  `queue cancel` also accepts a held item.
- `foundry pacing show [--json] [--offline]`, `pacing pause` and
  `pacing resume [--lane interactive|campaign|maintenance|all]`, and
  `pacing drain [--timeout <duration>]`. Pause state is persisted to
  `~/.foundry/pacing-state.json` through an atomic rename and is in force after
  a restart; `pacing_paused` / `pacing_resumed` are durable and on Watch.
- Additive proto: `WorkItem.depends_on` and `not_before`,
  `ResumeWorkItemRequest.depends_on` and `not_before`, and the `HoldWorkItem`,
  `ReleaseWorkItem`, `GetPacing`, `PausePacing` and `ResumePacing` RPCs.
- `FOUNDRY_PACING_PATH` and `FOUNDRY_PACING_STATE_PATH`.

### Changed

- `foundry campaign cancel` also cancels the campaign's queued cycles.
- The nightly majors lane no longer re-dispatches an upgrade that is already
  queued or held in the ledger.
- A `queue resume` child is recorded `queued` and started by the scheduler;
  admission appends only `work_item_submitted`, and `work_item_started` comes
  from the start.

- Every work item records a typed `source` at submission, beside the untouched
  free-text `origin`: `campaign` (the campaign name and cycle number),
  `sentinel` (the sentinel name), `operator` (the submitting CLI's hostname)
  or `work_item` (the parent item id, for a resume child and for a release cut
  after a remediation). The source rides the event envelope from the root a
  submitter emits and propagates like `trace_id` and `gather_id`, so the
  ledger records it off whichever event opens an item's chain. Items recorded
  before the field existed have none, and every reader treats that as not
  recorded.
- `WorkItem` and `EmitRequest` on the wire carry the source as an additive
  message; `ListWorkItems` takes an exact-match source filter (an unknown kind
  is `INVALID_ARGUMENT`). `foundry queue --source <kind>:<ref>` uses it in both
  list forms, online and offline; rows show a source column, `queue show`
  prints a `Source:` line only when one was recorded, and `--json` carries a
  `source` object or no key.
- `foundry campaign show` lists the campaign's cycles from the ledger through
  that filter, rather than by trace inference.

## [0.41.2] - 2026-10-09

### Fixed

- Cleanup resolves path aliases before checking Foundry roots and live worktree
  ownership. This preserves ownership across macOS temporary paths and symlinks.
- Work reconciliation recognizes landed work against either local trunk or a
  freshly fetched origin trunk. It retains ambiguous and unmatched work.
- Landed task branches are cleaned up only after fresh Git proof and ownership
  checks. Unlanded preservation refs, checked-out branches, and refs owned by
  other open work remain protected.

- Cleanup Branches retains live task and campaign worktrees, operator worktrees,
  and orphaned worktrees with dirty or unpushed work. Ownership is rechecked
  against durable state immediately before each removal, with ledger and campaign
  locks released before Git awaits. Admission precedes workspace creation, but
  the path is recorded afterwards: live items without workspace evidence protect
  all project candidates, and creation refuses existing paths. Merged branch
  cleanup protects live work and recorded preservation refs; task finalization
  and operator-directed campaign disposal retain ownership of their own removals.

- Inference costing includes the configured Claude 5.1/5.5 and GPT-6 models,
  with their published cache rates verified on 2026-10-04. Recognised old seed
  entries refresh without overwriting custom rates. Sonnet 5 uses the current
  $2/$10 price after 2026-08-31 and retains historical accounting before it.
- Codex cache writes are priced separately from fresh input. Model lookup accepts
  OpenCode prefixes and OpenAI dated snapshots. Campaign reports show known
  list USD and identify missing billing dimensions instead of presenting an
  incomplete estimate as total cost. Provider-reported cost remains authoritative.

## [0.41.1] - 2026-10-01

### Fixed

- Early acceptance prompts show exact behavioral and direct JSON shapes and
  require validation. Proof checks accept source-path arrays and report wrong
  field types precisely without relaxing exit-code or log checks.
- Task finalization archives early proof and its logs under Foundry's evidence
  directory, records `proof_evidence` in the result, and excludes new `.foundry`
  artifacts from commits while retaining already tracked project files.

- Campaign RPCs wait for the campaign-store lock without blocking Tokio workers,
  keeping reads and formation agent output responsive during concurrent controls.
  Formation lock acquisition also runs on the blocking pool; `cancel --now`
  still aborts its target before waiting for the store.

## [0.41.0] - 2026-10-01

### Added

- `foundry capture -- <command>` retains full stdout/stderr logs, returns the
  command's exit code, and prints a bounded failure tail.
- `foundry campaign report <name> [--json]` reports dispatched and landed cycles,
  stage sessions and tokens, elapsed time, external completion, and unknown or
  unpriced usage. Per-formation records include decisions, task-result event
  references, transcript paths, request bytes, and command activity. Native Codex
  observations remain separate from terminal usage, including partial counters
  for interrupted sessions.
- Campaign tasks require early acceptance evidence in `.foundry/proof.json`
  before paid review. The proof records a real rejecting input, its corrected
  passing form, commands, exit codes, and retained logs. Nonbehavioral changes
  use the documented direct-proof form.

### Changed

- Campaigns reassess the full remaining mission after each implementation cycle.
  The charter, campaign guide, maintainer guidance, and operating skill state
  this design principle explicitly. Implementation experience can change scope,
  priority, or task decomposition within the mission and owner constraints.
- Formation uses balanced/medium reasoning; independent task review remains
  deep/high. Context files are listed by path for selective inspection. Prompts
  omit passing gate logs and repeated task prompts. Git evidence and failure
  diagnostics are bounded, with references to full evidence.
- Formation defaults to a positive 32 KiB prompt limit. Missions, owner policy,
  and typed reviewer findings retain their wording. Campaign agents have no
  clock deadline, including project agent timeouts. Legacy stage time fields
  are ignored when loading existing definitions.
- Campaigns enforce one writable repository in an isolated worktree through
  Codex's workspace sandbox. Multi-repository declarations and unsupported
  campaign providers fail at admission; standalone tasks retain their existing
  provider behavior. Exhausted budgets with unlanded non-complete results
  escalate without another formation invocation.
- Native Codex sessions request a 2,000-token tool-output limit. Coding prompts
  direct verbose checks through `foundry capture` to retain complete evidence.

### Fixed

- Escalation retains the pending typed task result and preserved base reference.
  Extending and resuming a campaign now carries reviewer findings and accumulated
  work into reassessment without manual recovery.
- Formation reports structured provider errors and stops after one unsupported
  model rejection instead of retrying it three times.
- The reconciliation integration fixture uses canonical temporary paths, matching
  Git's worktree paths on macOS without weakening its assertions.

## [0.40.4] - 2026-09-30

### Fixed

- The codex gateway now passes the prompt on stdin, so a large prompt no longer
  fails to spawn. On 2026-09-30 campaign `foundry-work-settlement-v1`
  (`agent_provider: codex`) escalated at its first formation after three
  `agent_session_ended` records reading `failed to spawn codex: Argument list
  too long (os error 7)`: its formation prompt was over Linux's 131072-byte
  `MAX_ARG_STRLEN` cap on one argv element, and codex took the prompt as the
  last argv element. `codex exec` now gets `-` as its last argument, which makes
  it read the prompt from stdin (documented in `codex exec --help` for
  `codex-cli` 0.159.2). The agent stream runner writes the prompt and then
  closes stdin, as it already does for claude, so codex does not wait on an
  open stdin. The persona preamble and every other codex flag are unchanged.
  This removes the "no stdin escape hatch" limit named in the review-prompt
  entry below. The opencode gateway still takes its prompt in argv: `opencode`
  is not installed on the host where this fix was made, so there was no
  `opencode run --help` to confirm that it reads a prompt from stdin.
- The task reviewer no longer crashes codex with `Argument list too long`
  (`E2BIG`) on a verbose but fully-passing gate run. A `parite` campaign_cycle
  task on 2026-09-30 finished a real coding session with 12 files changed and
  every gate passing (`fmt`, `clippy`, `test`, `build`, `coverage`,
  `security-deny`, `security-audit`), but the follow-up review never started:
  `build_review_prompt` folded every gate's full (200-line-capped) output
  verbatim into the reviewer's prompt, and the seven gates' `cargo test
  --verbose`-heavy output summed to 162,152 bytes — over Linux's 128 KiB
  `MAX_ARG_STRLEN` ceiling on a single `execve()` argument string, which binds
  far tighter than the 2 MiB `ARG_MAX` (argv+envp combined) the incident first
  suspected. Codex's prompt rides as the last positional argv element with
  stdin deliberately closed for every invocation, so there is no stdin escape
  hatch for this provider. `build_review_prompt` now caps a passing gate's
  output to 800 bytes (a passing gate's noise isn't reviewer-relevant) and a
  failing gate's to 16 KiB (enough to diagnose a real failure), with a 64 KiB
  ceiling on the whole gate-results block regardless of gate count, and a
  logged last-resort truncation of the objective itself if it alone is ever
  over budget — the review now runs instead of stalling completed work as
  `runner_error`. Also flagged: `foundry_sdk::campaign::MAX_INLINE_CONTEXT_BYTES`
  (256 KiB) is sized against macOS's 1 MiB `ARG_MAX` and does not account for
  Linux's much tighter `MAX_ARG_STRLEN`, so a campaign that inlines close to
  its permitted context could hit the same class of failure on any codex
  invocation, not just review — not fixed here, needs its own decision.

## [0.40.3] - 2026-09-30

### Fixed

- A second `foundryd` can no longer change shared state. On 2026-09-30 an agent
  ran `foundryd --version` beside a live daemon; `foundryd` had no such flag, so
  the command started a second daemon whose start-up restart sweep settled a
  still-running task `failed` with the reason `daemon restarted` before the
  process failed to bind and exited. `foundryd` now binds its listen address and
  takes an exclusive advisory lock on `~/.foundry/foundryd.lock` (override:
  `FOUNDRYD_LOCK_PATH`), recording its pid there, before it runs any recovery
  sweep, seeds or merges any store, starts the scheduler or emits any event. If
  the address is in use or the lock is held it prints one line naming the
  running daemon's pid when known, exits non-zero and changes nothing on disk.
  What the sweeps do when they legitimately run is unchanged. Accepted gRPC
  connections keep `TCP_NODELAY` on, as they had before the listener was bound
  up front.
- `foundryd --version` (`-V`) and `foundryd --help` (`-h`) now print and exit 0
  without taking the lock, binding, touching any store or starting anything.
  Unknown flags and arguments are rejected with a usage error instead of being
  ignored.
- Agent sessions that a daemon stop left without an end no longer show as
  running forever. On start, once it holds the single-instance lock and before
  it accepts work, `foundryd` reads the last
  7 days of the event log for `agent_session_started` events with no
  `agent_session_ended` for the same `session_id`, and records an
  `agent_session_ended` for each with the new status `interrupted`, the error
  `daemon restarted`, the original session's project and trace, and `ended_at`
  set to the daemon start time. The ends are persisted and published on the
  Watch stream like any other event, so consumers that rebuild session state
  from the log (such as ops-visualizer's `/agents` page) see the session close.
  A second start records nothing for the same session, and unparseable log
  lines are skipped rather than failing startup. Live sessions start and end as
  before.

## [0.40.2] - 2026-09-30

### Added

- `foundry queue show <id>` now prints the item's own `work_item_*` events after
  its record — one line each with when it occurred, its type, the state it left
  the item in, the event id and the reason — or an explicit `(no events)` line
  when there are none. `--json` keeps every existing key unchanged and adds an
  `events` array. The events come from a new, additive `ListWorkItemEvents` RPC
  in `foundryd`, which reads the durable event log (`FOUNDRY_EVENTS_DIR`) at
  request time, caches nothing, never writes and never takes the ledger write
  gate. It selects events by the exact item id in the event payload — never by
  trace or project, so the `maintenance`, `remediation` and `release` items of
  one run each show only their own events — reads every monthly file however
  old, and returns them oldest first, ties in log order. An unknown id is
  `NOT_FOUND`; a missing events directory is an empty list; a fault reading the
  log is `INTERNAL`, never an empty list. A malformed or glued line in the log is
  skipped with a daemon-side warning and does not hide the item's other events.
  `--offline` reads the ledger and `FOUNDRY_EVENTS_DIR` directly through the same
  selection (`foundry_sdk::work_item_events::read_work_item_events`), and the
  online path still never reads the client-side ledger or events files.
  `GetWorkItem`, `ListWorkItems`, `foundry queue` and `foundry queue open` are
  unchanged.

### Fixed

- Codex failures now surface the provider's real error instead of a useless
  stdin-probe line. `CodexAdapter::interpret()` always set `Interpreted.failure`
  to `None`, so `build_agent_execution_result()` fell back to stderr's first
  line for every codex failure — and codex's stderr opens with "Reading
  additional input from stdin..." on every run, success or failure alike,
  carrying no diagnostic value. A `parite` campaign_cycle task hit this on
  2026-09-30: the summary read "plan execution failed: Reading additional input
  from stdin..." while the actual cause — an invalid `codex.balanced` model id
  in the operator's `~/.foundry/agents.json`, rejected by OpenAI with a 400 —
  sat unread in the JSONL stdout stream. `interpret()` now extracts the real
  `turn.failed`/error message (unwrapping the nested JSON-encoded provider
  error body when present), and the summary prefers that message over stderr's
  first line whenever one is present, unchanged for every other case.
- A task whose trunk moved while it ran now lands instead of being reported as a
  defect. When landing-eligible work (a `complete`, or a converging `remainder`
  with green required gates) cannot fast-forward because the registered trunk
  advanced, Finalize Task fetches, rebases the task branch onto the new trunk
  inside the task worktree, re-runs the project's required gates on the rebased
  tree, and lands when they pass. If trunk moved again during the gate re-run it
  tries once more — two rebase attempts at most. A conflicting rebase, a red
  required gate, or a trunk that keeps moving preserves the reviewed branch as
  before. On 2026-09-30 a 57-minute parite task that passed its gates and review
  was recorded as `defect` because two documentation commits reached
  `origin/main` while it ran; the owner had to land it by hand.
- A blocked landing no longer rewrites the reviewer's verdict. A `complete` that
  cannot land stays `complete` in `task_run_completed`, with `landed: false`,
  `success: false` and a typed `land_blocked` reason (`trunk_moved_conflict`,
  `trunk_moved_gates_failed`, `trunk_moved_repeatedly`, `checkout_not_ready` or
  `git_failed`). The payload also records the trunk commits that arrived during
  the run in `trunk_arrivals`. Both fields are absent when trunk did not move and
  landing succeeded, so that wire shape is unchanged. Consumers treat an unlanded
  `complete` as preserved work to reconcile: its work item settles `preserved`
  with the block in its reason, the campaign history shows
  `complete, did not land (<reason>)`, and the nightly majors lane does not
  re-dispatch that upgrade while the preserved branch exists. `defect`,
  `blocked_on_decision` and `runner_error` handling is unchanged.

- `foundry campaign cancel <name> --reason … --now` now settles the aborted
  cycle's work item instead of leaving it `running`. The abort means the
  `TaskRunCompleted` that normally settles the item never arrives, so until now
  the item sat `running` in `foundry queue` until the next daemon restart closed
  it `failed` with the reason `daemon restarted` — reporting an operator's
  deliberate stop as a fault. The `running` item carrying the aborted run's
  trace now settles `cancelled` with the `--reason` text verbatim, `settled_at`
  set, and a disposition recording the cycle's worktree, whether that worktree
  is gone (observed after disposal) and the branch or `bundle:` ref any
  preserved work is recoverable from. This also makes `work_item_cancelled`
  real: it is emitted through the engine, on the aborted cycle's own trace, so
  it reaches `foundry watch` and the durable JSONL log like the other three
  ledger events. Correlation is by trace alone, exactly as a normal settlement,
  with no fallback to the project's newest running item. Recording only — no
  dispatch, abort, landing, preservation or worktree cleanup behaviour changed.
  A graceful cancel, a `--now` cancel with nothing in flight, and an
  already-cancelled campaign each change no item and emit nothing; a ledger
  fault never turns a successful cancellation into an error, and never prevents
  the abort or the `CampaignCancelled` event. The killed run's trace reaches the
  settlement through an additive optional `aborted_trace_id` field on the
  `campaign_cancelled` payload.

## [0.40.1] - 2026-09-30

### Added

- `foundry task <project> "<description>" [--origin <text>]` and
  `foundry campaign advance <name> [--origin <text>]` now record operator
  context on the work item they dispatch: the CLI client's hostname, plus any
  `--origin` text verbatim. A campaign cycle keeps its campaign name and cycle
  and gains the operator context beside them. The value reaches the daemon
  through an additive optional `operator_origin` field on the
  `ExecutionRequested` payload and on `AdvanceCampaignRequest`; a payload or
  request that omits it records exactly the origins it did before (`foundry
  task`, `campaign <name> cycle <n>`, `nightly majors lane`), and cycles from
  the automatic post-result advance are unchanged. Origin stays opaque —
  nothing parses, validates, filters or groups on it, an empty `--origin` is
  accepted, and no dispatch is rejected, delayed or altered because of it. A
  hostname lookup that fails records the stated fallback `host unknown host`
  rather than failing the dispatch.
- `CHARTER.md` now states that work admission, pacing and settlement are in
  scope — a deliberate scope expansion decided by the owner on 2026-09-29 —
  and that the queue holds only executable work and is deliberately not a
  backlog.

### Fixed

- Agent prompts whose answer is parsed as a typed JSON object now carry a
  standing single-turn rule: the session gets exactly one turn, no notification
  will arrive, no command may be started in the background, every command runs
  to completion in the foreground, and the answer must end with the required
  JSON object. Two sessions on 2026-09-30 ended their turn waiting for a
  backgrounded command to notify them — the reviewer's cycle was preserved as a
  defect it did not have (`reviewer returned no valid task verdict`) and the
  campaign escalated (`campaign agent returned no valid decision`). The rule is
  worded once and applied to the reviewer, campaign formation, plan, drift
  scout, strategic continue-assessment and every `json_output_prompt` caller.
- The Claude gateway now recovers a missing typed answer instead of failing on
  it: when a block that parses JSON gets output with no JSON object in it, the
  gateway resumes that same session once (`claude --resume <session_id>`) at
  the same tier and under the same tool access, and accepts a parseable second
  answer. A resume that also returns no object leaves the original answer
  untouched, so the caller still reports its original parse failure. Every
  resume is recorded with a `tracing::warn!`.
- That recovery could not actually succeed as first shipped, and now can. The
  Claude CLI minted its own session id, so `claude --resume <id>` was handed
  Foundry's id — one the CLI had never heard of — and every recovery failed
  with "No conversation found with session ID". The Claude invocation now
  passes `--session-id <foundry uuid>`, so the CLI adopts Foundry's id and the
  id Foundry logs under is the id the conversation is stored under. The
  gateway test no longer compares the resumed id against the
  `AgentSessionStarted` payload (which passed on the wrong id): its fake CLI
  reports a session id of its own in the result envelope, and the test asserts
  the original invocation and the resume name that same id.
- The recovery now covers every block that parses a typed JSON answer.
  `requires_json` was hard-coded `false` in the read-only observer wrappers,
  which silently excluded `ScoutDrift`, `AssessProject`, `StrategicAssessor`
  and `TriageAssessment` — all four parse JSON and all four were the blocks
  the single-turn rule was written for. It is now a field on
  `ReadOnlyAgentSpec` that each block sets, with a test per block asserting
  its request carries `requires_json: true`. Observers whose answer is prose
  (commit and ops digests, result summaries, the assessment-naming call) stay
  `false`.
- The `foundry queue show` example in the work-queue guide showed an `Origin` of
  `majors`; the ledger records `nightly majors lane`.

## [0.40.0] - 2026-09-30

### Added

- A work-item ledger: one durable record per unit of work Foundry dispatches,
  at `~/.foundry/work-items.json` (override with `FOUNDRY_WORK_ITEMS_PATH`).
  Until now nothing answered "what is running, what still needs a person"
  without reading raw events, worktrees on disk and remote branches. Each item
  carries its project, objective, `kind` (`task`, `campaign_cycle`,
  `maintenance`, `major_upgrade`, `release`, `remediation`), `lane`
  (`interactive`, `campaign`, `maintenance`), opaque `origin`, timestamps,
  `state` (`submitted`, `queued`, `running`, `landed`, `preserved`,
  `needs_decision`, `failed`, `cancelled`), a one-line `reason`, and a
  settlement `disposition` holding the verdict, the landed commit or
  preservation ref, the worktree path and whether it was removed. The store is
  daemon-owned and authoritative: every mutation loads the file, applies the
  change, and saves it through a same-directory temp-file rename.
- `foundryd` records every task-kind dispatch in the ledger: `foundry task`
  (kind `task`, lane `interactive`), each campaign cycle (kind
  `campaign_cycle`, lane `campaign`, origin naming the campaign and cycle),
  and each nightly majors-lane upgrade task (kind `major_upgrade`, lane
  `maintenance`). The item reaches `running` before the coding agent is
  invoked, and settles from the run's existing typed result: `complete` or a
  landing `remainder` lands; a non-landing `remainder` or a `defect` is
  preserved work, with its preservation ref; `blocked_on_decision` needs a
  decision; `runner_error` — including a dispatch that faults before the agent
  starts — fails, with the reason. On daemon start, every item still `running`
  is settled `failed` with the reason `daemon restarted`, so a restart never
  leaves work visibly running.
- Four event types carry ledger state onto the Watch stream and into the
  durable JSONL log: `work_item_submitted`, `work_item_started`,
  `work_item_settled` and `work_item_cancelled`. `WorkItemStarted` pairs with
  `WorkItemSettled` rather than a `*Completed`, because an item does not
  complete — it settles, into a state that may still hold an obligation.
  No dispatch is delayed, reordered, merged or deduplicated, and a ledger
  fault never fails, delays or alters a dispatch.

- The work-item ledger now covers the three dispatch paths it had left out, so
  all six kinds are recorded rather than only the task-shaped three. Each
  per-project maintenance run inside a cycle is an item (kind `maintenance`,
  lane `maintenance`, origin `maintenance cycle`), opened at its root
  `ProjectRunStarted` and settled from its own `ProjectRunCompleted`. Each
  release is an item (kind `release`), opened at `ReleaseRequested` (lane
  `interactive`, origin `foundry release`) or at the clean `MainBranchAudited`
  that cuts an automatic release (lane `maintenance`), and settled from
  `ReleaseCompleted` with the new tag named in the reason;
  `ReleasePipelineCompleted` and `LocalInstallCompleted` are downstream
  observation and leave the settled record untouched. Each remediation is an
  item (kind `remediation`), opened at the dirty `MainBranchAudited` or the
  failing `PipelineChecked` that dispatches it and settled from
  `RemediationCompleted` — a remediation Foundry stopped for review settles
  `failed` with that review text, since its commits are not pushed and no
  release follows. Whether an item is opened is decided by the same predicate
  the dispatching block's `accepts()` already uses, so an item exists exactly
  when the run does, and a dry run records nothing. Settlement correlates on
  trace, project *and* kind, so a cycle's fan-out siblings, and a run holding
  a nested remediation and an automatic release on one trace and project, each
  settle only the item their own root opened. Task-kind correlation is
  unchanged. No dispatch is delayed, reordered or altered.

- Two typed gRPC read RPCs serve the work-item ledger from `foundryd`:
  `ListWorkItems` and `GetWorkItem`. Both load `FOUNDRY_WORK_ITEMS_PATH` on
  every call and cache nothing, so a caller always sees the ledger as it
  stands; neither writes, and neither takes the ledger write gate.
  `ListWorkItems` takes optional exact-match `project` and `state` filters
  (`alpha` does not match `alpha-2`; an unknown state tag is
  `INVALID_ARGUMENT`) and returns a deterministic order independent of the
  store's contents and insertion order: `running` by `started_at` ascending,
  then `submitted`/`queued` by `submitted_at` ascending, then open items
  (`preserved`, `needs_decision`, `failed`) by `settled_at` descending, then
  terminal items (`landed`, `cancelled`) by `settled_at` descending, with ties
  broken by id ascending. `GetWorkItem` returns the full record, with the
  settlement fields absent rather than empty when the item is unsettled, and
  `NOT_FOUND` for an unknown id. A missing or empty ledger is an empty list; a
  malformed one is `FAILED_PRECONDITION` and an unreadable path is `INTERNAL`.
  No dispatch or ledger-write behaviour changes, and no CLI command is added
  yet.

- `foundry queue` reads the work-item ledger from the operator's side. Until now
  the ledger was recorded and served but had no command, so the answer to "what
  is running, and what still needs me?" was still a JSON file. `foundry queue`
  prints four groups on one screen: running, queued (`submitted` or `queued`),
  open (`preserved`, `needs_decision`, `failed` — settled but still owing
  something to a person), and the newest 20 settled items (`landed`,
  `cancelled`), each line carrying the item's id, project, kind, lane, state,
  the timestamp that placed it in its group and its one-line reason. The 20-item
  cap applies to the settled group alone. `foundry queue open` prints only the
  open group, and `foundry queue show <id>` prints one item's full durable
  record, where an optional field the ledger never recorded produces no line at
  all — so a recorded `worktree_removed: false` reads `no` while an unrecorded
  one is silent, and the two are never conflated. All three take `--json` (an
  array for the list forms, an object for `show`, optional fields absent when
  unset) and render the human and JSON forms from the same fetched data. Group
  membership and order come from the daemon's `ListWorkItems` response; the CLI
  groups by state and never re-sorts. All three are read-only and all three are
  daemon-authoritative online: they render the response directly, never read,
  create or mutate the client-side `FOUNDRY_WORK_ITEMS_PATH`, fail with an error
  naming the matching `--offline` command when `foundryd` is unreachable, and
  surface a `NOT_FOUND` from `GetWorkItem` as the id-not-found error rather than
  an empty record. `--offline` reads the ledger file directly in the same
  grouping order the RPC documents: a missing store renders empty groups and
  exits zero without creating the file, and a malformed one is an error naming
  the parse failure. `foundry queue show` prints the record alone — no existing
  read RPC accepts a `trace_id`, so the item's `work_item_*` events are not
  listed, and no RPC was added to reach them. No daemon, block, ledger, proto or
  dispatch behaviour changes.

### Fixed

- A child agent that exits before draining its prompt from stdin is now
  reported by its own exit code and stderr. The stdin writer treated the
  resulting broken pipe as the run's error, so a `claude` process that died
  early on a large prompt was settled `unavailable` with no exit code, no
  stderr and no interpretation — the runner's `EPIPE` masked the child's real
  failure. The write is now best-effort: it logs a warning and lets
  `child.wait()` plus the drained stderr report the outcome as they do for any
  other exit.
- The Claude CLI now receives its prompt on stdin instead of as an argv element,
  so a large prompt no longer fails the spawn outright. Linux caps a single
  argv element at `MAX_ARG_STRLEN` (131072 bytes); campaign formations whose
  prompt crossed that line died instantly with `E2BIG`, recorded only as
  `failed to spawn claude`, and escalated the campaign. `claude -p` reads the
  prompt from stdin in print mode, and the streaming runner now takes an
  optional stdin payload that it writes on its own task and then closes. With
  no payload, stdin stays closed as before, since `opencode` hangs on an open
  inherited stdin. The `codex` and `opencode` argv are unchanged.
- A failed agent spawn now records the underlying OS error — for example
  `failed to spawn claude: Argument list too long (os error 7)` — in the
  `agent_session_ended` payload's `error` field, instead of a bare
  `failed to spawn claude` with no cause.
- The real-git tests now run Git hermetically, so `cargo test --workspace`
  passes inside a Foundry agent session. An agent session injects
  `remote.origin.pushurl = foundry://agent-push-disabled` through `GIT_CONFIG_*`;
  those variables are inherited by every child process and outrank repository
  config, so fifteen fixture tests failed on a push that had nothing to do with
  the behaviour under test. The env-stripping helper and the real-process shell
  gateway that already existed for the `FinalizeTask` tests now live once, in
  `test_helpers::git_repo`, and every real-git test uses them. Test-only change;
  no production behaviour differs.
- The ledger now records a task dispatch from the workflow's **root**
  `ExecutionRequested` event rather than from `PreflightCompleted`, so a
  dispatch that stops before the coding agent starts is no longer missing from
  the ledger entirely. A dispatch that fails its charter check, or fails
  preflight, is now recorded `running` and then settled `failed` with that
  stopping event's own reason — the charter guidance, or the names of the gates
  that failed. The chain itself is untouched: the same blocks run at the same
  points, and the existing settlement from `task_run_completed` is unchanged.
  A dispatch naming a project the registry does not know is declined rather
  than recorded — the chain refuses it without emitting anything that could
  settle an item, so recording one would leave work visibly `running` forever.
- Every ledger mutation in the daemon process now takes one shared write gate
  instead of each mutation site owning a private lock. A record in one spawned
  workflow and a settle in another could previously interleave their
  load-modify-save of `work-items.json` and silently drop the earlier write.
  The file stays the single source of truth; the gate only orders
  read-modify-write sequences, and a poisoned gate is absorbed — never a panic
  in `foundryd`.
- A settlement can no longer close the wrong item. When a run's result names a
  trace no running item carries, nothing is settled; the project's
  newest-running fallback now applies only to a result that carries no trace at
  all. Two concurrent runs on one project previously risked one run's verdict
  settling the other run's item.

## [0.39.9] - 2026-09-29

### Fixed

- The remediation agents are now checked like the maintain agent. 0.39.7
  gave `Remediate Vulnerability` and `Remediate Pipeline` the prompt rules and
  the push-disabled environment, but not the post-agent checks, and an agent
  asked to fix a CVE is the one most tempted to allowlist it. Both now record
  `HEAD` and `origin/<branch>` before the agent, then run the direct-push
  check and the suppression check. A tripped check fails the remediation with
  `needs_review` on `remediation_completed` (new optional field). Its commits
  stay local, so no `project_changes_pushed`, no post-push audit, no release
  and no install follow. There is no retry.
- Iterate and task agent runs get the same two checks (all three workflows
  already ran with pushing disabled). An iterate suppression fails the
  attempt and is retried with the suppression called out; a task that trips
  either check gets a `blocked_on_decision` verdict without a review session
  and does not land.
- The suppression check now sees files the agent never added to Git.
  `Commit and Push` stages them with `git add -A`, so an untracked
  `.supply-chain-allow.json` would otherwise have been pushed unchecked. This
  also applied to maintain runs.
- The suppression check diffs from the commit `HEAD` and `origin/<branch>`
  shared at the start of the run, not from `HEAD`. A suppression commit a
  failed run left unpushed now fails the next run instead of being pushed
  with it.

### Changed

- `LoopContext.maintain_base` / `MaintainBase` are now `run_base` /
  `RunBase`, since every guarded workflow records one. A first attempt always
  records a fresh start; only a retry inherits one.

## [0.39.8] - 2026-09-29

### Fixed

- A summary for an interrupted cycle is headed with the time the cycle
  started, not the time foundryd restarted. The first one written (the
  2026-09-26 nightly on the ops host) was headed 2026-09-29.
- The summary's majors column said "would dispatch (dry run)" whenever
  dispatch was off, including for an interrupted cycle that was not a dry
  run. It now says "would dispatch (not started)".

## [0.39.7] - 2026-09-29

### Fixed

- A maintain agent can no longer push past Foundry's checks. On 2026-09-29
  mojentic-kt's agent pushed its own commit; the suppression check then
  diffed against an `origin/main` that already held it and saw nothing, and
  `Commit and Push` reported nothing to push. Now:
  - The maintain, retry and remediation prompts say to commit locally and
    never push; Foundry pushes after its checks.
  - Maintain, iterate, task and remediation agent sessions run with
    `remote.origin.pushurl` set to `foundry://agent-push-disabled` through
    `GIT_CONFIG_*` environment variables, so a `git push` to `origin` fails.
    Foundry's own push is unaffected.
  - A maintain run records its starting `HEAD` and `origin/<branch>` before
    the first agent session and carries them through retries
    (`maintain_base`). The suppression check diffs against that start, so it
    covers pushed commits and a retry is checked against the same commit as
    the first attempt.
  - If `origin/<branch>` moved during the run to commits the checkout holds
    (checked after a fetch, so an explicit-URL push is seen too), the run
    fails with "needs review: agent pushed directly to `origin/<branch>` …",
    naming the commits and the suppression check's result. It is not
    retried. The failure metadata carries a new `needs_review` field, and
    `Route Gate Result` stops without retrying whenever it is set.
- A maintenance cycle foundryd was stopped during now gets closed and
  summarized. The 2026-09-26 nightly on the ops host never completed and
  wrote no summary: at 08:44 UTC every process owned by the service user,
  foundryd included, was killed with SIGKILL (the systemd user manager and
  the user's syncthing service died in the same second; no OOM kill was
  logged), and foundryd did not start again until 12:03. On start, foundryd
  now looks back seven days for system cycles with no
  `maintenance_cycle_completed`, records one with the unfinished projects as
  `missing`, and writes the summary from the events the cycle logged. The
  summary opens with "Cycle interrupted", lists unfinished projects as
  failed, is dated by the cycle, never overwrites an existing summary, and
  dispatches no major-upgrade tasks and no triage.
- A block that fails before a project's `project_run_completed` no longer
  leaves the cycle without a `maintenance_cycle_completed`. When nothing is
  left to run, the engine closes any gather still open and records the
  children that never arrived (`missing`, with the failing block's error).
- `Install Locally` no longer starts `brew` on a host that is not macOS. It
  records the step as skipped with "install via brew not supported on
  `<os>`". An install command that cannot start is recorded as a failed
  `local_install_completed` event instead of an engine error that only
  reached the daemon log.
- The maintenance summary's Local Installs table has a Detail column with
  each failure's or skip's reason, and the result line warns when install
  steps failed.
- Skill installs are no longer derived from the project name when a
  project has no install config. `foundry registry show hone-cli` on a host
  with no install config said it would run `hone-cli init --global --force`;
  the binary is `hone`. It now reports the skip and why.

### Added

- `foundry registry add` and `foundry registry edit` take
  `--installs-skill <true|false|"command">`, online and `--offline`.
  `RegistryAddRequest.installs_skill` (17) and
  `RegistryEditRequest.installs_skill` (27) carry it over gRPC.

### Changed

- `comfy-table` upgraded from 7.2.2 to 8.0.1. The CLI only uses
  `Table::new`, `set_content_arrangement`, `set_header` and `add_row`, all of
  which are unchanged in 8.x, so every rendered table is byte-identical.
- `foundry init` now installs on cmx-core 0.4. The release adds generated
  agent installation and Codex frontmatter preservation, neither of which
  foundry uses, so skill install, remove, lock tracking and `--json` output
  are unchanged.

## [0.39.6] - 2026-09-25

### Fixed

- A repository with no dependency manifests now reaches the supply-chain
  digest's "No dependency manifests" section. 0.39.4 detected it but dropped
  the flag between the scanner and the scan payload, so the project read as a
  clean scan.

## [0.39.5] - 2026-09-25

### Added

- The maintain prompt, the maintain retry prompt and the task/iterate
  execution prompt carry advisory rules: never suppress, ignore or allowlist
  an advisory that has a fixed release (upgrade instead); never edit
  `.supply-chain-allow.json`, `ignore_advisories`, Dependency-Check
  suppressions or equivalent files; never claim no fix exists without citing
  the registry or OSV entry. Maintain agents had explained away fixable
  advisories twice on 2026-09-25.
- A maintain run that adds a suppression fails with "needs review: …" and is
  not pushed. Foundry diffs the checkout against `origin/<branch>` after the
  agent and flags new `.supply-chain-allow.json` entries, `ignore_advisories`
  in `mix.exs` or `mix_audit` skips, Dependency-Check `<suppress>` entries,
  `pip-audit --ignore-vuln`, `cargo audit --ignore` and `deny.toml` /
  `audit.toml` advisory ignores, and npm overrides pinned to a version the
  latest supply-chain scan marks vulnerable.

## [0.39.4] - 2026-09-25

### Fixed

- npm audit findings reached through a dependency chain were reported with a
  package name (`@serenity-js/rest`) as the advisory ID and as a policy call
  with no fix. Foundry now reads npm's `via` chains: each finding carries the
  GHSA ID (npm's advisory number as an alias), the vulnerable package and
  range, and npm's fix availability, including `fixAvailable: true` without a
  version and semver-major fixes. A fixable finding is never a policy call.
- A repository with no dependency manifest in any ecosystem (such as
  CloudFormation templates with Python tests) is reported under "No
  dependency manifests: nothing to audit", not as a scan failure. A manifest
  without a lockfile is still "not scanned".

### Added

- Low-disk protection. Before a maintenance run (`Validate Project`) or a task
  (before its worktree is created) starts, Foundry checks free space on the
  filesystems holding the project, the worktree and `~/.foundry`. Under
  `FOUNDRY_MIN_FREE_DISK_GB` (15) and `FOUNDRY_MIN_FREE_DISK_PERCENT` (10) it
  fails at once with "insufficient disk: …" instead of failing late ("Out of
  diskspace" at finalize). The maintenance summary gains a "Low disk" section
  and the ops digest raises a P0 `host_disk_low` anomaly.
- `scripts/build-linux-release.sh <tag>`: the on-host Linux release build. It
  uses one target directory and deletes it (and its worktree) on exit, keeping
  only the tarball. AGENTS.md documents the ops-host release procedure.

## [0.39.3] - 2026-09-25

### Fixed

- The audit covers every ecosystem in a repository, not only the registered
  stack, and finds lockfiles in subdirectories (skipping build output,
  installed dependencies, fixtures and vendored trees). epilogue-tracker's
  `apps/cli/Cargo.lock` was reported as "Couldn't load Cargo.lock". Results
  are merged, and a part that could not be audited is named in the error.
- A text `bun.lock` (and `yarn.lock` or `pnpm-lock.yaml`) is audited with
  `osv-scanner`. Before, Bun projects were sent to `npm audit`, which refuses
  a project without `package-lock.json`, and the refusal read as clean. A
  binary `bun.lockb` is reported as not scanned.
- Mix projects run `mix deps.get` before `mix deps.audit`, so a lockfile that
  moved ahead of `deps/` no longer stops the audit. A failed fetch is
  reported with the project's path.
- The dependency classifier also finds manifests with lockfiles in
  subdirectories, so a monorepo's every ecosystem is classified.

## [0.39.2] - 2026-09-25

### Fixed

- Vendored code is no longer classified or put in the maintain brief. A scope
  under `vendor/` or `third_party/`, or a path `.gitattributes` marks
  `linguist-vendored`, is listed as "vendored, updated upstream" instead. The
  security audit still scans it. Bedrock's `vendor/roost` was being briefed.
- A hold whose cap is below the locked version is reported as "stale hold:
  locked X is above cap Y, re-decide" and never produces a downgrade. The brief
  also refuses any move that is not newer than the locked version.
- `foundry deps` no longer says "Everything classified is up to date" while a
  hold suppresses an update or is stale, and it lists stale holds and vendored
  scopes.
- A review names the commit it classified and fetches the remote first; when
  the checkout is behind `origin/<branch>` it says so. A stale ops-01 checkout
  made the 0.39.1 review of bedrock describe lockfiles origin had already
  moved past.

## [0.39.1] - 2026-09-25

### Fixed

- Major-upgrade task objectives read "The npm dependency is declared in the
  repository root." instead of "It is a npm dependency declared in ..".
- Builds clean under clippy 1.96 (the ops host's toolchain).

## [0.39.0] - 2026-09-25

### Added

- Registry field `update_policy` (`patch`, `minor` or `major`), set with
  `foundry registry add/edit --update-policy`. It is the ceiling for how far
  maintenance may move a project's dependencies. `foundry registry show`
  prints it on an `Updates:` line; a project without one behaves as `minor`.
- `foundry registry edit --clear-install` removes a project's install
  configuration. Passing `--install-command ""` never cleared it.
- Deterministic dependency classification before the maintain agent runs.
  `Classify Dependency Updates` reads each ecosystem's manifest and lockfile
  (Cargo, Hex for every Mix project the audit covers, npm with
  `package-lock.json` or `bun.lock`, PyPI with `uv.lock`, the Gradle version
  catalog, SwiftPM) and each registry, and classifies every outdated direct
  dependency as patch, minor or major. A `0.x` minor bump is a major. A stack
  or file it cannot read is reported as "not classified", never as up to date.
- The maintain prompt is now a brief: the exact updates to apply under the
  project's policy, the updates held back and why, and the majors left for
  separate tasks. The agent must not go beyond the list. A maintain retry may
  not move dependencies further.
- The majors lane. After the nightly summary, each major a `major`-policy
  project may take is dispatched as its own `foundry task` ("Upgrade `<pkg>` from
  `<a>` to `<b>` in `<project>`: adapt call sites, keep all gates green"), one after
  another. Dedupe skips a major whose task is in flight or left preserved work.
  `FOUNDRY_MAJOR_TASKS_PER_PROJECT` (default 2) and
  `FOUNDRY_MAJOR_TASKS_PER_NIGHT` (default 6) cap it; overflow is reported with
  its command. `minor` and `patch` projects get proposals with the command.
- Security fixes override the ceiling: a patch or minor fix is applied anyway;
  a major fix goes to the majors lane whatever the policy. The supply-chain
  auto-fixer follows the same rule (`task_lane` outcome) and reverts a full
  update that took a major.
- `.dependency-holds.json`: a committed, per-repo hold that keeps a package at
  or below a version until an expiry. Lapsed holds are listed for a new
  decision; a malformed file applies no holds and warns.
- The maintenance summary gains a "Dependency drift" section near the top and
  per-project dependency detail: policy, updates applied by class, updates
  applied beyond the brief, held updates, majors lane decisions with commands,
  lapsed holds and unclassified scopes.
- `foundry deps <project> [--policy X]` shows a project's outdated
  dependencies, its brief and what the majors lane would dispatch, changing
  nothing.
- Events `dependency_review_requested`, `dependency_updates_classified` and
  `major_upgrades_planned`.

### Changed

- `Execute Maintain` now runs on `dependency_updates_classified` (phase
  `before`) instead of `gate_resolution_completed`, and `Generate Summary` runs
  on `major_upgrades_planned` instead of `maintenance_summary_requested`.
- Gate commands and their `fix_command` now run with `CARGO_INCREMENTAL=0`.
  A gate run builds once and exits, so Cargo's incremental cache was only disk
  cost: on one Mac, `target/debug/incremental` held about half of 176 GiB of
  Rust build output. Agents that edit and rebuild during `foundry iterate`
  still get incremental builds.

## [0.38.5] - 2026-09-25

### Fixed

- A Python audit that printed no report read as clean. On ops-01, pip-audit
  exited 1 with no stdout after PyPI answered 503 for the project's own
  (unpublished) package, and researcher-cli and zk-chat were reported as "no
  vulnerabilities found". cargo audit, npm audit and pip-audit output with no
  JSON report is now a scan failure that quotes stderr, and pip-audit runs
  with `--skip-editable` so it no longer queries PyPI for the project itself.
  A repeated pip-audit advisory is reported once.

## [0.38.4] - 2026-09-25

### Fixed

- Elixir audits failed for every Elixir project. The parser expected a JSON
  array, but `mix deps.audit --format=json` (`mix_audit` 2.x) prints one
  `{"pass": …, "vulnerabilities": […]}` object, sometimes after dependency
  compilation lines. Foundry now reads that object (advisory ID, severity,
  first patched version) and treats exit 1 as "findings", with output that has
  no report counted as a failure.
- Repositories without a root `mix.exs` (bedrock) are audited per Mix project:
  every Mix project that declares `mix_audit` is audited and the findings are
  merged. `deps`, `_build`, `node_modules` and hidden directories are skipped.
- A scanner failure no longer reads as success. The post-push and release-tag
  audits mark the block failed when the scan did not run, the release-audit
  table shows "scanner failed" instead of "clean", and the maintenance summary
  lists **Scanner failures** beside **Unpushed commits**.
- The post-push auditor (and `scan_requested`) now apply the repository's
  `.supply-chain-allow.json` with the supply-chain scan's semantics (active
  entries accept, lapsed ones resurface). `audit_exceptions` still works and is
  documented as legacy. Allowlist entries and exceptions match any alias the
  scanner reports (pip-audit, osv-scanner and cargo-audit aliases), in both the
  auditors and the nightly supply-chain scan, so a CVE entry accepts the
  PYSEC finding for the same advisory. Accepted and lapsed findings are named
  in the audit's result line.

### Added

- After the maintain agent, Foundry checks the checkout is on the configured
  branch. An agent branch that fast-forwards it is merged back (the configured
  branch moves forward, is checked out, and the agent branch is deleted with
  `-d`); otherwise the run fails with a reason naming the branch. Validation
  records a wrong branch as `sync_failure: "wrong_branch"`, and the maintenance
  summary lists those projects under **Projects skipped: wrong branch**.
- The Kotlin scan's result line says how many findings fell below the
  project's `failBuildOnCVSS` and were not counted.

## [0.38.3] - 2026-09-24

### Fixed

- Kotlin audits run `./gradlew dependencyCheckAggregate --rerun`. Without
  `--rerun`, Gradle reported the task UP-TO-DATE on a repeat scan, left the
  previous report untouched, and the scan was recorded as "exited 0 without
  writing a fresh report" (seen on mojentic-kt with 0.38.2). `--rerun` makes
  every audit check the current vulnerability data.

## [0.38.2] - 2026-09-24

### Fixed

- Kotlin audits failed with "exited 0 without writing a fresh report" on
  Dependency-Check 13, which writes its report to
  `build/reports/dependency-check/dependency-check-report.json`. Foundry now
  looks in both the old and the new location and uses the newest report this
  run wrote; the freshness check is unchanged.
- Kotlin findings now follow the project's own `failBuildOnCVSS` (read from
  `build.gradle.kts` or `build.gradle` when it is a numeric literal). A finding
  whose highest CVSS score is below it does not count as a vulnerability, which
  matches the project's build: those are reported for triage, not failures.
  Before, every medium finding counted, so a project whose own audit passed
  would have raised a vulnerability for each of them. The number left out is
  logged.

## [0.38.1] - 2026-09-24

### Fixed

- Nightly maintenance commits were stranded on the build host. When the
  maintain agent committed its own work, `Commit and Push` found nothing left
  to commit, reported "No changes to commit", and never pushed. On
  mojility-ops-01 ten repositories held up to four nights of unpushed commits.
  The push now depends on whether the branch is ahead of `origin/<branch>`, not
  on whether the step made a commit, and a run reporting `changes: false`
  still pushes commits stranded by an earlier run.
- After rebasing onto a remote that moved during the run, the required gates
  are re-run on the rebased commits before the push
  (`push_failure: "gates_failed_after_rebase"` when they fail or the project
  has no gates). A rejected `git push` is now recorded as `push_failed` instead
  of being reported as "Committed and pushed changes". A run that reports
  `success: false` keeps its commits local (`run_failed`). Never force-pushes.

### Added

- The maintenance summary checks every push-enabled project after the run and
  lists any with unpushed commits ("N commit(s) ahead of origin/main") in an
  **Unpushed commits** section at the top of the report, with a warning in the
  block result.

## [0.38.0] - 2026-09-24

### Added

- `swift` and `kotlin` registry stacks, so SwiftPM and Gradle projects can be
  registered for maintenance, audit, and push.
  - Swift audits run `osv-scanner scan source --format json --lockfile
    Package.resolved`. Each osv-scanner advisory group is one finding, named by
    its CVE alias when there is one. Exit 1 means findings; 127/128 and a
    missing `Package.resolved` are reported as not scanned.
  - Kotlin audits run the project's own `./gradlew dependencyCheckAggregate
    --no-parallel --no-daemon` with a 60-minute timeout and read
    `build/reports/dependency-check-report.json`, so the project's
    Dependency-Check suppression file decides what counts. A report is accepted
    only when this run wrote it; Gradle exits 1 for both a CVSS failure and a
    broken build, so a missing or stale report is a scan failure, never a clean
    one. Findings carry no fix version and are policy calls.
  - Supply-chain auto-fix: Swift gets the full update (`swift package update`,
    `Package.resolved` only) and explicitly refuses a targeted pin; Kotlin
    reports `no_fixer`.

## [0.37.0] - 2026-09-18

### Added

- Optional per-tier effort caps in `~/.foundry/agents.json`. Each provider may
  declare `effort_caps` keyed by model tier (for example
  `{"deep": "high", "balanced": "medium"}`); a request whose reasoning effort
  exceeds its tier's cap is lowered to the cap before it is mapped to the
  provider's CLI token, and a lower request is never raised. Absent caps change
  nothing, the default seed carries none, and the seed merge never adds or
  rewrites them. `AgentSessionStarted` now carries `effective_effort` alongside
  the requested `effort`, so a cap is visible in the event stream.

## [0.36.0] - 2026-09-18

### Added

- Token accounting on agent sessions. Every token Foundry spends is spent inside
  an agent CLI session, and every session already writes its own usage into its
  transcript — but nothing read it back, so the estate's cost was invisible.
  `AgentSessionEnded` now carries `usage` (per-model input/output/cache-read/
  cache-write counts) and `cost` (USD, plus a `basis` saying how much to trust
  it). Reconstructed over 30 days of history, this was ~$1,800/month of
  previously unmeasured spend at list price.
- A token price book at `~/.foundry/token-rates.json` — published vendor list
  rates per model per token type, seeded and seed-merged on start exactly like
  `agents.json`, with each rate carrying the source URL and date it was verified.
  Rates are runtime data: they change on vendor schedules, not release
  schedules, so the file is editable without a rebuild. Rates may carry an
  `until` date and a `then` successor so introductory and negotiated pricing
  expires on its own date. Override the path with `FOUNDRY_TOKEN_RATES_PATH`.
- Codex sessions are now priceable. Their transcripts report token counts but
  never name a model, so their spend could not be costed at all; the gateway now
  supplies the model it resolved from the request's tier.
- A session that ends without writing a terminal usage record reports
  `basis: "unmeasured"` rather than a cost of zero. Spend that happened but
  could not be measured must not read as free.
- The nightly per-project chain now syncs each registered checkout with its
  remote before any work. `Validate Project` refuses a dirty tree
  (`sync_failure: "dirty_tree"`), fetches `origin/<branch>`, fast-forwards
  only, and refuses a diverged branch (`sync_failure: "diverged"`) — in every
  refusal leaving the checkout untouched. `project_validation_completed`
  records how many commits were fast-forwarded (`fast_forwarded`). Previously
  the chain never fetched, so a clone lagging `origin` was maintained as stale
  code. Under `dry_run` nothing is fetched or merged; the would-be count is
  reported with `dry_run: true`.
- `Commit and Push` fetches and fast-forwards before pushing. If the remote
  moved during the run, the maintenance commit is rebased onto it and pushed
  only if the rebase is clean; otherwise the rebase is aborted, the commit is
  left on the local branch for a human, and `project_changes_committed`
  records `push_failure: "push_rejected_diverged"`. Never forces.

### Changed

- `AgentSessionStarted` / `AgentSessionEnded` now carry the `trace_id` of the
  workflow that spawned the session, on the event envelope as well as in the
  payload. Previously both were always empty, so token spend could be attributed
  only to a project and a timestamp — ambiguous the moment two runs on one
  project overlap. `AgentRequest` gained a `trace_id` field and blocks forward it
  from their triggering event.
- Supply-chain auto-fix (`RemediateSupplyChain`, still gated dark behind
  `FOUNDRY_SUPPLY_CHAIN_REMEDIATE`) now prefers a full compatible dependency
  update over a single-package pin. For a project with fixable findings it first
  runs the ecosystem's full update (`cargo update`, `uv lock --upgrade`,
  `npm update` / `bun update`), re-runs the same scanner that detected the
  findings to confirm which cleared, re-runs the gates, and commits the lockfile
  as `chore: update dependency lockfile to latest compatible versions (fixes
  <CVE>)`. Only findings the full update does not clear — or every finding, when
  it fails, clears nothing, or fails a required gate — fall back to the previous
  targeted pin (`cargo update -p <pkg> --precise <ver>`, `uv lock
  --upgrade-package`, or the manifest rewrite), still committed as
  `chore(deps): bump …`. Each outcome's `detail` now starts with the path taken,
  `full_update` or `targeted_pin`. The verify-and-rollback contract is
  unchanged: dirty trees and gate-less repos are skipped, and any failure
  restores the touched files.

### Fixed

- `foundry campaign complete <name> --offline` on a `Cancelled` campaign no
  longer silently flips it to `Completed`. Cancellation is terminal and
  distinct from completion; recording a cancelled campaign as complete wrote
  a false evidence claim into the store. The campaign status transition rules
  (`pause`/`resume`/`decide`/`complete`/`cancel`/`advance`) now live in one
  place, `foundry_sdk::campaign::transition`, and both the daemon's gRPC
  handlers and the CLI's `--offline` recovery path delegate to it — see
  `book/src/guide/campaigns.md`.

### Changed

- `foundry campaign resume --offline` and `foundry campaign decide --offline`
  error wording now matches the daemon's canonical messages byte-for-byte
  (e.g. "pass add_cycles > 0 to authorize more work" instead of "pass
  --add-cycles N to authorize more work", and "requires Paused or Escalated
  status" instead of "resume applies only to paused or escalated campaigns").
  The legality of a transition no longer depends on which door — online or
  `--offline` — the caller walked through.

## [0.35.0] - 2026-07-25

### Added

- `foundry campaign cancel <name> --reason "<text>"` stops a campaign
  permanently. Cancellation is a distinct terminal `cancelled` status rather
  than a flavour of `completed`, because completion in Foundry is an evidence
  claim and recording an abandoned campaign as complete puts a false assertion
  into the audit trail and the ops digest. It is not resumable — use `pause` for
  a campaign meant to come back — and, unlike `complete`, does not require
  `authorized_by`, since an unauthorized campaign has no other reachable
  terminal state.
- `--now` on `campaign cancel` terminates the in-flight cycle immediately
  instead of letting it finish, killing the running agent process. A whole
  campaign runs inside one daemon task, so the daemon now retains that task's
  join handle and aborts it; dropping the task drops the agent's child process,
  which `kill_on_drop` then kills. The abort is awaited so the campaign store
  lock the aborted task held is provably released before cancellation takes it.
  Kills the agent process only — tool subprocesses it spawned are reparented and
  survive.
- `--discard-work` on `campaign cancel --now` throws the terminated cycle's
  uncommitted work away instead of committing and preserving it to a branch or
  bundle. Remote branches pushed by earlier cycles are never deleted. The flag
  requires `--now`: a graceful cancellation has already preserved its work, so
  there would be nothing uncommitted left to discard.
- `campaign_cancelled` event, flattening the shared campaign terminal payload so
  existing terminal observers parse it unchanged, plus `terminated_now`,
  `discard_work`, and `aborted_event_id`. An aborted run persists no trace, so
  `aborted_event_id` is the only handle onto its partial events.

### Fixed

- `foundry campaign resume --offline` never checked campaign status, so it could
  resume a campaign from any state — including, once cancellation existed,
  silently reviving a terminal one. It now requires `paused` or `escalated`,
  matching the daemon-side `ResumeCampaign` behaviour.

### Changed

- Claude's deep model tier now uses `claude-opus-5`, replacing the retired
  Opus 4.6/4.8 canonical aliases. Existing generated `agents.json` stores
  migrate those known defaults on daemon startup while preserving custom model
  overrides. The balanced tier remains `claude-sonnet-5`; runtime defaults,
  gate derivation, tests, and workflow documentation now consistently use the
  Claude 5 model family.

## [0.34.3] - 2026-07-24

### Changed

- Every event published to Watch is now persisted idempotently in the monthly
  event log. This includes non-routable `block_started` and `block_completed`
  observations plus agent-session and service-completion signals that were
  previously Watch-only. The live stream can no longer be richer than the
  authoritative event history, and repeated delivery of one event ID cannot
  append duplicate audit facts.

## [0.34.2] - 2026-07-24

### Changed

- `block_started` and `block_completed` observations are now persisted in the
  monthly event log as durable audit evidence while remaining non-routable.
  They were previously Watch-only progress messages, which made the live stream
  richer than the authoritative event history and allowed a dashboard restart
  to erase evidence of block execution.

## [0.34.1] - 2026-07-23

### Fixed

- The campaign inline-context budget is now enforced on the daemon's
  `AddCampaign` path, not only the CLI's `--offline` branch. `foundry campaign
  add` goes through the daemon by default and the daemon carries its own
  context validation, so the 0.34.0 guard never ran in practice — a live smoke
  test admitted a definition inlining 347,694 bytes without complaint. The
  check now lives in `foundry-sdk` and both admission paths call it, so the two
  copies cannot drift apart again.

## [0.34.0] - 2026-07-23

### Fixed

- Campaign context no longer inlines source files, which could make a campaign
  impossible to form at all. `read_context_files` inlined every declared path
  whole; one campaign declared 21 paths totalling 1,039,114 bytes — 17 of them
  source — which pushed the formation command line past `ARG_MAX` (1,048,576 on
  macOS). Every provider passes the prompt as a command-line argument, so
  `execve` failed before the agent started and the only symptom was an opaque
  "unavailable" that the retry logic then burned three attempts on. Two further
  campaigns had completed while sitting within 44 KB of the same cliff.

  `context_paths` was conflating two jobs. A charter or intent projection is
  **binding**: its wording must reach the acceptance criteria verbatim. A source
  file is **orienting**: it says where to look, and the formation agent already
  holds `Read`, `Glob`, and `Grep` over the same checkout, so inlining one pays
  whole-file token cost for the few functions it needs. Formation now inlines
  binding artifacts and lists orienting ones as a manifest it is told to open
  itself. Measured against the live store with no definition edited, the three
  largest campaigns drop from ~1 MB to 85,163 bytes inlined — a 92% reduction —
  while every declared file stays reachable.

### Added

- `foundry campaign add` enforces a 262,144-byte budget on the *inlined* portion
  of campaign context, naming the largest offenders and what to do instead.
  Source paths cost nothing in the prompt and do not count against it. Oversized
  context does not degrade gracefully — it fails the spawn outright — so this is
  caught at definition time rather than mid-campaign.

### Documentation

- Added a repository README with installation, project setup, workflow
  selection, task and campaign concepts, scheduled work, observability, and
  contributor gates.
- Reworked the mdBook onboarding and campaign documentation around the current
  daemon-authoritative control plane, evidence design, safe remainder landing,
  preserved-work continuation, provider pauses, owner decisions, cycle budgets,
  and cycle-level tracing. Updated the bundled agent skill and CLI, event,
  tracing, workflow, and gRPC references to match.

## [0.33.0] - 2026-07-23

### Added

- A campaign cycle can now be reconstructed from the event stream alone.
  Formation was the only agent-invoking block that recorded no `prompt`, so what
  the decision agent actually saw — whether it was shown the accumulated
  unmerged work or a stale tree — could only be inferred from its prose.
  `CampaignAdvanceCompleted` now carries `prompt`, `agent_provider`, and the
  `gate_results` its done-evidence gates produced. Decisions forced without
  asking an agent record no formation, and the absence is asserted rather than
  implied.

- Campaign events now carry tracing identity. The advance RPC built its root
  with a bare `Event::new`, and `stamp_context` only ever *inherits* — so every
  campaign event in production carried `trace_id: null` and `span_id: null`,
  leaving the longest-running workflow in the system the only one
  `foundry trace` could not reconstruct. The root now mints both: the trace
  groups a campaign run, and the span groups one cycle, since
  `CampaignAdvanceRequested` is a span opener and each advance mints a fresh
  one.

- `campaign_cycle` is stamped onto every task-side event. Of the twelve events a
  cycle emits, only `CampaignAdvanceCompleted` recorded `cycles_completed`, so
  grouping events to a *campaign* worked but grouping them to a *cycle* meant
  sorting on time and treating each `CampaignAdvanceRequested` as a boundary — a
  guess that breaks on concurrent campaigns in one project and on out-of-order
  arrival. The field sits beside `campaign` in both `ChainContext` and
  `LoopContext` so it survives exactly the hops `campaign` survives, and is set
  from the `cycles_completed` incremented for that dispatch so both sides of a
  cycle report the same number.

### Fixed

- Formation no longer restates the campaign's mechanical done-gates as task
  acceptance evidence. Those gates run at formation against the delivered trunk;
  a dispatched task runs the project's own discovered gates in an isolated
  worktree and cannot produce a result for a command it is never given. The
  response-shape hint literally asked for "gates" in the objective, so an
  unsatisfiable item appeared in three of one campaign's five cycles — including
  as the sole outstanding gap on an otherwise fully satisfied final cycle. It
  cost reviewer attention rather than blocking completion, since campaign
  done-gates are evaluated separately at formation.

## [0.32.1] - 2026-07-22

### Fixed

- `cargo doc` no longer fails the build. The new public `objective_history`
  field's doc comment linked to a private constant, which
  `rustdoc::private_intra_doc_links` rejects under CI's
  `RUSTDOCFLAGS=-D warnings`. v0.32.0 published binaries with a red CI run; this
  release supersedes it. `AGENTS.md`'s quality-gate list omitted the doc check
  and has been corrected to match `.github/workflows/ci.yml`, which is what
  allowed the gap through in the first place.

## [0.32.0] - 2026-07-22

### Fixed

- Work preserved to a Git bundle is now resumable. `preserve` wrote the bundle
  with only `refs/heads/<task branch>` and no `HEAD`, while resume used a bare
  `git fetch <bundle>` — which resolves `HEAD`. Every bundle resume therefore
  died with `fatal: couldn't find remote ref HEAD`, and because the bundle is
  only written when pushing to `origin` already failed, the fallback was a dead
  end exactly when it held the only copy of the work. It escalated two client
  campaigns (`backend-ort-screener-resilience-2026-07`,
  `frontend-ort-segmenting-audio-resilience-2026-07`) that were progressing
  normally. New bundles now record `HEAD`, so a human can `git clone` or fetch
  one by hand; independently, resume reads the branch ref out of the artifact
  via `git bundle list-heads` rather than depending on `HEAD` at all, which is
  what keeps bundles already on disk recoverable.

- A transient provider failure no longer ends a healthy campaign. Any
  decision-agent transport failure became a terminal escalation with no retry,
  which twice killed `parite-phase-3-independence-aware-selection` at a 100%
  landing rate — once with a completely empty reason. Foundry now re-asks up to
  three times with a widening backoff and reports every attempt in the final
  escalation, so an empty stderr can no longer produce an unexplained
  escalation. A malformed decision is still not retried: the agent answered, and
  re-asking would only burn three identical invocations.

- A campaign whose provider is unusable — exhausted account, revoked
  authentication, open circuit breaker — now moves to `paused` rather than
  `escalated`, from both the formation path and the executor's `runner_error`
  verdict. Nothing about the campaign's own work is wrong, so no cycle is
  consumed and the pending run result is preserved for `campaign resume`. This
  previously terminated `parite-phase-3-2-probe-observability-closure` at cycle
  zero, twice, before it did any work.

- A `done` decision rewritten because a required gate is red now carries the
  campaign mission and each failing gate's actual output into the synthesized
  objective, and forbids reverting landed mission work or broadening a lint
  allowance to turn the gate green. It previously carried only the failing
  command, discarding all mission context — which cost cycles in two campaigns
  that had substantially landed their mission and were losing to a
  `too_many_lines` lint on test files they had just written.

### Added

- `EventType::CampaignPaused` makes an automatic pause observable. Pausing on an
  open circuit breaker is the right behaviour, but it stops a campaign with
  nobody watching — an operator who runs `foundry campaign pause` already knows,
  whereas a campaign that quietly stops itself is indistinguishable from one
  still working. The event carries the reason and is deliberately non-terminal:
  it does not end the campaign, consume a cycle, or discard the pending result.

- Campaign formation now sees the objectives it has already cut. The prompt
  carried only the single most recent run result, so a decision agent could not
  tell it was restating an earlier request —
  `foundry-daemon-authoritative-state-v1` re-cut essentially one objective
  across nine cycles, and `parite-remote-nvenc-conversion-v1` oscillated between
  unrelated sub-gaps for twelve. Campaigns now retain their eight most recent
  objectives with each one's typed verdict and whether it landed, rendered into
  the prompt with an explicit no-restatement rule: a repeat means either the
  work exists on a tree the agent is not reading, or the same request has
  already failed once and needs reformulating or escalating. Existing campaign
  stores load unchanged and are not rewritten with the new field.

- `foundry campaign add` now rejects a **required** gate that asserts state on
  another host (`ssh`, `scp`, `sftp`, `rsync`, `systemctl`, `launchctl`,
  `journalctl`, `service` in command position). A required gate is re-run every
  formation and blocks `done` outright, so one that cannot be satisfied from a
  disposable task worktree makes the campaign structurally incapable of
  completing and guarantees it burns its whole cycle budget — which is exactly
  how `parite-remote-nvenc-conversion-v1` spent 12 cycles and landed nothing.
  The error names the offending command and points at the two correct homes for
  deployment evidence: a `review` statement the owner verifies, or an explicitly
  optional gate. Optional gates may still probe host state advisorily.

## [0.31.0] - 2026-07-22

### Changed

- Converging work now integrates instead of accumulating a divergent branch. A
  `remainder` verdict — the reviewer's term for a finite list of missing work on
  a converging implementation — fast-forwards trunk when at least one required
  gate ran and every required gate passed; its reviewer gaps travel forward in
  the typed result and become the campaign's next objective. Previously only
  `complete` could land, so sound-but-unfinished work was held back exactly like
  a faulty approach: nothing merged until some later cycle happened to return
  `complete`, and campaigns that never did stranded every cycle they ran. The
  green required-gate evidence is the safety boundary — a `remainder` with a
  failed required gate, or with no required gate to vouch for it, stays
  preserved, and `defect`, `blocked_on_decision`, and `runner_error` never land.
  A branch is now cleaned up whenever it lands, not only when it was `complete`.

## [0.30.4] - 2026-07-22

### Fixed

- Campaign formation now reasons about the tree the next cycle actually starts
  from. A non-landing cycle (`remainder` or `defect`) preserves its branch and
  chains forward as the next execution's `base_ref`, but the decision agent was
  only ever shown the canonical checkout — so from the second such cycle onward
  it inspected a tree that contained none of the campaign's accumulated work and
  re-cut objectives that branch had already satisfied. The formation prompt now
  carries a distinct `ACCUMULATED UNMERGED WORK` section listing the commits and
  changed files reachable from the base ref but absent from the trunk, and
  directs the agent to cut its objective from mission minus
  trunk-plus-accumulated while still judging `done` against the delivered trunk
  state alone.

### Added

- `foundry campaign complete <name> --reason <evidence>` now provides an
  owner-authorized external completion path. It records the reason, clears stale
  pending results, and emits the normal campaign terminal event so automation
  and operations digests observe the transition.

## [0.30.2] - 2026-07-20

### Changed

- Claude's balanced model tier now uses `claude-sonnet-5`, replacing the
  outdated `claude-sonnet-4-6` mapping for coding-agent work and gate
  derivation.

### Fixed

- Campaign formation now rejects decisions with blank reasons or objectives, and
  provider failures without diagnostic output receive an explicit fallback
  reason instead of creating an unexplained owner-decision escalation.

## [0.30.1] - 2026-07-19

### Fixed

- `foundry campaign add` now resolves the registered project checkout and
  rejects missing, absolute, parent-traversing, or checkout-escaping context
  paths before mutating the campaign store. Errors identify the campaign and
  offending path, unknown projects remain distinct, and rejected definitions
  leave existing campaign data byte-identical.

## [0.30.0] - 2026-07-18

### Added

- Campaigns now expose typed `PauseCampaign`, `ResumeCampaign`,
  `AdvanceCampaign`, and `RecordCampaignDecision` gRPC operations, with matching
  `foundry campaign pause|resume|advance|decide` commands. Owner decisions are
  durable campaign state, exhausted budgets require an explicit positive
  extension, and every extension is applied exactly once.
- Real-server generated-client integration coverage now verifies campaign pause,
  resume, advance, and owner-decision behavior at the wire boundary. Concurrent
  pause requests from independent clients also prove that campaign store updates
  do not lose unrelated writes.

### Changed

- `foundry campaign advance` preserves its synchronous wait-and-render CLI
  experience while the typed daemon RPC remains asynchronous, allowing UI and
  automation clients to issue lifecycle commands without holding the request
  open for a complete formation cycle.

### Fixed

- Campaign continuation now resolves the exact landed task commit and retains a
  local task reference before removing its worktree, so the next cycle can
  inspect the proven result without depending on an ephemeral remote branch.
- The task workspace Git guard now protects nested test shells as well as the
  primary agent process, while Foundry's finalizer retains sole authority to
  commit and land reviewed work.
- Online campaign decisions now fail through the typed daemon boundary instead
  of silently falling back to direct campaign-file mutation.

## [0.29.4] - 2026-07-18

### Added

- Campaign gate evidence can declare repository-relative artifacts. A gate now
  fails before running its command when any declared artifact is absent or
  unsafe, preventing test runners from silently accepting a missing test path.

### Fixed

- Results arriving while a campaign is paused are stored durably with their
  typed verdict and preservation reference. The next manual advance consumes
  that result, forms from the stopped work, and dispatches from the preserved
  branch without double-counting a landing or erasing the last run identity.
- Skeptical review now requires tests to exercise a requested real or generated
  boundary and assert its observable request or response; mocks installed above
  that boundary no longer satisfy the evidence requirement.

## [0.29.3] - 2026-07-18

### Fixed

- Isolated task executors can no longer push through the checkout's configured
  `origin`. Foundry now applies a provider-neutral, process-scoped Git guard and
  explicitly reserves commits, pushes, merges, rebases, tags, and ref updates
  for its post-review finalizer.

## [0.29.2] - 2026-07-18

### Added

- `GetCampaign` exposes one campaign's complete durable definition and runtime
  state through a typed, read-only gRPC query. The detail contract preserves
  typed done evidence and returns explicit `NOT_FOUND` for absent campaigns.

### Fixed

- Task finalization now detects commits already authored on the task branch,
  lands them on trunk, and reports a typed `landed` result. Campaign accounting
  no longer treats reviewed but unintegrated work as landed or repeats the same
  objective against an unchanged trunk.
- Complete tasks with no deliverable changes now report that no landing was
  required, while failed landings remain preserved and non-complete.

## [0.29.1] - 2026-07-18

### Fixed

- Structured agent output extraction now prefers the final fenced JSON object
  and falls back to the last structurally valid object. Typed verdicts from
  providers such as Claude are no longer rejected when preceding review prose
  contains brace notation.

## [0.29.0] - 2026-07-18

### Added

- `ListCampaigns` exposes the durable campaign inventory through a read-only
  gRPC query, with deterministic ordering and an optional exact project filter.
  Store access is request-time, and errors do not expose the campaign file path.

### Fixed

- A task that lands in the final authorized campaign cycle now receives its
  completion evaluation. Foundry escalates only if that evaluation attempts to
  dispatch another task beyond the cycle budget.

## [0.28.2] - 2026-07-18

### Added

- `foundry campaign resume --add-cycles N` explicitly extends an exhausted
  campaign's cycle budget with owner authorization. Exhausted campaigns now
  reject a no-op resume that could only re-escalate immediately.

### Fixed

- Skeptical task review now labels every gate as required or optional and states
  that optional failures are advisory. The prompt also reflects the actual
  lifecycle boundary: tracked modifications and untracked additions are both
  valid review inputs because Finalize Task commits them only after a complete
  verdict.

## [0.28.1] - 2026-07-18

### Fixed

- Post-execution verification now treats failed optional gates as advisory at
  the block-result boundary. Required gates remain the landing criterion while
  `all_passed: false` and the optional failure details remain visible in the
  emitted gate event.

## [0.28.0] - 2026-07-18

### Changed

- Release blocks (`CutRelease`, `ExecuteRelease`) now use the canonical
  `TaskBlock` + `SimulatedSuccess` + `dry_run_via_simulation!()` composition,
  eliminating the parallel
  `WorkBlock`/`EventAdapter`/`OutputMapper`/`ComposedStep` abstraction.
- Dirty-branch release triggers are now filtered at dispatch (via `accepts()`)
  rather than skipped inside `execute()`.
- Dry-run no longer fabricates `ReleaseCompleted` for projects with
  `release: false` in their action flags.
- `foundry init` now installs on cmx-core 0.2, which reconciles the skill's
  `metadata.version` to the foundry binary version at install time. foundry's
  `SKILL.md` previously carried no version, so `cmx doctor` saw the installed
  skill as unversioned; it now declares `metadata.version` (e.g. `"0.27.0"`)
  automatically, with no in-tool stamping.

### Added

- **Durable campaign formation.**
  `foundry campaign add|list|show|advance|pause|resume` manages
  evidence-terminated objectives in `~/.foundry/campaigns.json`. Each advance
  inspects current repository state and neutral context artifacts, then chooses
  exactly one of done, one next task, or escalation. Campaigns enforce a cycle
  budget, auto-advance from typed task results, and surface completion or
  escalation through the existing ops-digest anomaly path.
- **Hardened one-shot task lifecycle.** `foundry task` now runs in an isolated
  worktree, performs a skeptical agent review after mechanical gates, and ends
  with a structural `complete`, `remainder`, `defect`, `blocked_on_decision`, or
  `runner_error` verdict. Non-complete work is committed and preserved on a
  named remote branch, falling back to a Git bundle; only complete work can
  fast-forward the registered trunk checkout. Task retry routing is disabled.
- **Supply-chain per-language auto-fixers (EXP-003 Slice 2b-2).** The dark,
  explicitly gated remediation engine now supports Bun/npm lock updates,
  TypeScript direct/override requirement rewrites, and uv/Python requirement
  plus lock updates through the existing verify-and-rollback rail. npm's
  `fixAvailable.name` is preserved as `fix_package`, preventing a transitive
  package from being paired with its direct parent's fix version. Only touched
  dependency files are committed locally; nothing is pushed.
- Synchronous workflow commands now stream transient block progress messages
  such as `running block Run Verify Gates` and `finished block Run Verify Gates`
  while long-running blocks execute.

### Fixed

- **Campaign control-plane races are serialized.** CLI add/pause/resume and
  daemon advancement now share a cross-process advisory lock around campaign
  store read-modify-write operations, preventing lost state during concurrent
  control commands.
- **Campaign dry-runs now terminate after one simulated task.** Skeptical review
  uses the registered checkout when dry-run execution intentionally creates no
  worktree, and dry-run task results do not recursively request another campaign
  advance.
- **Campaign formation failures now close the workflow.** Invalid context
  artifacts, registry failures, malformed agent decisions, and campaign-store
  failures emit `CampaignAdvanceCompleted` plus `CampaignEscalated` instead of
  leaving synchronous clients waiting without a terminal event.
- **`AuditReleaseTag` no longer reports a failed or mis-targeted scan as
  `vulnerable: false`.** Git checkout failures and scanner errors now propagate
  via `ReleaseTagAuditedPayload::scan_error` (an optional field, absent when the
  scan ran cleanly). Upstream `vulnerable` and `cve` payload values are
  forwarded unchanged when the scan cannot run, so the chain continues with the
  last known state rather than silently resetting it to "clean".
- **`extract_json` no longer panics on inverted brace order** (e.g.
  `"} then {"`). The helper previously indexed into the string with
  `start..=end` without verifying `start <= end`, which would panic as an
  out-of-order slice on inputs where `}` appears before `{`.
- **Rust supply-chain findings are no longer discarded as scanner failures.**
  `cargo audit` conventionally exits 1 when it finds vulnerabilities while
  writing a valid JSON report to stdout, matching npm and pip-audit semantics.
  Foundry now parses that report and surfaces the Rust advisories for triage and
  remediation instead of marking the project not scanned.
- **Sentinels sharing a scheduled instant now all fire.** The scheduler
  previously selected only one sentinel at the earliest deadline; after it
  fired, recomputing from the boundary skipped every other sentinel due at the
  same time. This starved `nightly-supply-chain` because its 06:00 schedule
  coincides with `ops-digest`. The scheduler now claims and emits the complete
  same-time cohort.
- Claude agent invocations now pass `--agent <name>` instead of the resolved
  `~/.claude/agents/<name>.md` path, matching current Claude Code CLI behaviour
  and preventing maintenance retries from failing before execution.

## [0.27.0] - 2026-07-04

### Changed

- **`foundry init` now uses cmx-core for skill installation.** The hand-rolled
  install mechanism has been replaced with
  [cmx-core](https://github.com/svetzal/context-mixer2/tree/main/cmx-core)
  (`SkillInstaller`). Behavioural changes:
  - **Default scope is now global** (`~/.claude/skills/foundry/`). Use
    `foundry init --local` for project scope.
  - `--global` is retained as a no-op alias (load-bearing: registry's derived
    skill-install command is `{binary} init --global --force`).
  - New `--remove` flag uninstalls the skill and cleans the lock entry.
  - Installed files are **byte-identical to bundled content** — the version
    stamp previously injected into `SKILL.md` frontmatter (`foundry-version:` /
    `metadata.version`) is gone. The cmx-core lockfile
    (`~/.config/context-mixer/cmx-lock.json`) is the sole source of truth for
    the installed version.
  - `--json` output shape changed: `targets[]` replaces `files[]`;
    `foundry-version` stamp field removed.

### Fixed

- **Post-push auditor honours per-project accepted CVEs.** New optional
  `audit_exceptions` field on registry `ProjectEntry` lets a project declare
  formally-accepted CVE/advisory IDs (case-insensitive). Matching
  vulnerabilities are filtered from the post-push auditor's output and logged as
  suppressed. Absent/empty preserves prior behaviour. Set by editing
  `~/.foundry/registry.json` (CLI wiring deferred).
- **Release pipeline watch no longer times out on healthy pipelines.**
  `gh run view` in the pipeline watcher's run-by-id polling ran without `--repo`
  from the daemon's working directory (not a git repository), so every poll
  failed and the watcher burned its full 30-minute timeout before emitting a
  false `release_pipeline_completed` timed-out/failure — which also stalled
  `foundry release` clients waiting on `local_install_completed`. Regressed in
  the run-by-id polling change released in 0.18.0.

## [0.26.1] - 2026-06-26

### Fixed

- **Terminal provider/account failures now stop agent workflows immediately.**
  Claude account exhaustion, including monthly spend-limit failures surfaced in
  session JSONL records with `api_error_status=429`, is now classified as a
  terminal provider/account condition. Foundry propagates structured failure
  metadata through agent session, execution, gate verification, and project
  completion events; bypasses retry routing for those failures; and opens an
  in-memory provider circuit breaker so foundryd stops launching additional
  doomed agent sessions during the same daemon lifetime.
- **Nightly maintenance sentinel duplicate emissions are guarded.** The
  scheduler now rechecks wall-clock time after waking and re-arms if it woke
  before the cron boundary, preventing the maintenance sentinel from emitting
  two runs for the same scheduled instant.

## [0.26.0] - 2026-06-16

### Added

- **Supply-chain auto-fix engine (EXP-003 Phase 2, Slice 2b — first increment,
  shipped dark).** `RemediateSupplyChain` can now *apply* a fixable advisory's
  fix, not just classify it. It is **off by default** and acts only when
  `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` is truthy *and* the throttle is `Full` (never
  `dry_run`); otherwise it is byte-for-byte the classifier. Every fix runs a
  mandatory verify-and-rollback rail and is reversible — **committed locally,
  never pushed**: (1) skip any project whose working tree is dirty; (2) apply
  the fix — this increment ships the in-range Rust bump,
  `cargo update -p <pkg> --precise <fix>`; (3) re-run the repo's own gates; (4)
  on passing required gates, commit just the lockfile, else
  `git checkout`-revert it. Each applied fix commits immediately so a later
  finding's rollback cannot clobber an earlier success. A version the manifest
  forbids → `apply_failed` (the override-pin rewrite case, a later increment);
  non-Rust stacks → `no_fixer`; a repo with no gates is skipped (an unverifiable
  fix is never applied).
- `SupplyChainRemediatedPayload` carries per-finding `outcomes`
  (`RemediationOutcome`); the digest gains a **Remediation** section
  (*Auto-fixed* / *Reverted* / *Not auto-fixed*) shown only when the engine ran.
- New env var `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` (default off).

## [0.25.2] - 2026-06-16

### Fixed

- **pip-audit exit code 1 (vulnerabilities found) misread as a tool failure.**
  Like `npm audit`, `pip-audit` exits non-zero when it finds advisories — the
  JSON report still goes to stdout. The scanner treated that as "audit tool
  failed" and discarded the findings, so a Python project *with* a real advisory
  landed under "Not scanned". `is_audit_vuln_exit_code` now recognises Python
  exit 1 as "vulnerabilities found" (stderr warnings are ignored; the stdout
  JSON is parsed). Completes the Python supply-chain scanning fix begun in
  0.25.1 — real advisories (e.g. `chromadb` `CVE-2026-45829`) now surface as
  live findings rather than scan errors.

## [0.25.1] - 2026-06-16

### Fixed

- **Python supply-chain scanning now works (venv-local tool + correct parser).**
  Two bugs kept every Python project reading as "not scanned". (1) `pip-audit`
  was invoked as a bare command expecting a global PATH; it is a *project*
  dependency that lives in the project's virtualenv, so it is now resolved from
  `{project}/.venv/bin/pip-audit`. A project without it reports a clean,
  informative "pip-audit not found in .venv" rather than a spawn error. Global
  PATH is never consulted for language/project tooling. (2) Even when it ran,
  the parser expected a top-level JSON array; real `pip-audit --format=json`
  emits `{"dependencies": [...], "fixes": [...]}` with each dependency carrying
  its own `vulns` list. A dedicated `parse_pip_audit` handles the real shape
  (advisory `id`, `fix_versions`); `mix deps.audit` keeps the generic parser.
  Validated live: 4 Python projects now scan, surfacing real advisories (e.g.
  `chromadb` `CVE-2026-45829`, no fix → policy call).

## [0.25.0] - 2026-06-16

### Added

- **Supply-chain remediation triage (EXP-003 Phase 2, Slice 2 — non-mutating
  first increment).** A new `RemediateSupplyChain` block is inserted into the
  supply-chain formation between the scan and the digest, with a new mid-chain
  event `SupplyChainRemediated`. It triages every live finding by _fix
  availability*: a populated fix version → mechanically **auto-fixable**; an
  empty one → a **policy call** (an exploitability judgement about our usage
  that stays human). The chain is now
  `SupplyChainScanStarted → ScanSupplyChain → SupplyChainScanned → RemediateSupplyChain → SupplyChainRemediated → WriteSupplyChainDigest → SupplyChainScanCompleted`.
- **Scanner fix-version enrichment.** `Vulnerability` and `SupplyChainFinding`
  now carry an optional `fix_version`, populated from each tool's output
  (`cargo audit` `versions.patched`, `npm audit` `fixAvailable.version`,
  `pip-audit` `fix_versions`). A `bare_version` helper reduces version
  requirements (`">= 0.2.5"`, `"^0.28.1"`) to bare versions. Unknown fix
  versions fail safe to `None` (classified as a policy call, surfaced — never
  silently auto-fixed).
- **Digest triage surface.** The digest now opens with a
  `N auto-fixable · M policy-call` triage line and the per-project findings
  table gains a **Fix** column (resolving version, or `policy call`).

### Notes

- This increment is **advisory and non-mutating** — `RemediateSupplyChain`
  classifies only. The auto-fix engine (in-range bump; override-pin manifest
  rewrite with gate-verify-and-rollback; no-fix policy surface) lands in a later
  increment behind an explicit env gate, inert until enabled even under `Full`
  throttle. `remediated_count` is always `0` today.

## [0.24.0] - 2026-06-16

### Fixed

- **Triage formation self-emit loop (event-log runaway).** `WriteTriageDigest`
  sank on `MaintenanceTriageCompleted` *and* re-emitted that same event type,
  re-triggering itself in an unbounded loop. Each iteration appended a large
  verdicts-bearing event, ballooning `~/.foundry/events/2026-06.jsonl` to ~56 GB
  (≈1000× a normal month) and eventually filling the disk. The block now emits a
  distinct terminal event, `MaintenanceTriageDigestWritten` (consumed by
  nothing), matching the terminal-event pattern of the ops/commit/supply-chain
  digest writers. A regression test asserts the terminal type differs from the
  sink type.
- **Startup hang on an oversized event file.** `detect_legacy_event_names` (the
  0.17.0 migration guard) read every `.jsonl` fully into memory via
  `read_to_string`, wedging daemon boot on the 56 GB file — any restart hung. It
  now streams line-by-line through a `BufReader`, capped at 64 MB per file, so
  memory stays bounded and an oversized file can never block startup again.

### Added

- New terminal event type `MaintenanceTriageDigestWritten`.

## [0.23.0] - 2026-06-16

### Added

- **Supply-chain scan formation (nightly, working-tree, advisory).** A new
  `nightly-supply-chain` canonical sentinel (06:00 local, enabled by default)
  emits `SupplyChainScanStarted`, driving a two-block formation that scans every
  active project's working-tree lockfile for dependency advisories. This is the
  *detection* lane the release-tag `ReleaseTagAudited` audit does not cover — it
  scans what is checked out now, on a schedule, independent of whether code
  changed. `ScanSupplyChain` runs each stack's audit tool (`cargo audit`,
  `npm audit`, `pip-audit`, `mix deps.audit`), classifies each advisory against
  that repo's committed `.supply-chain-allow.json`, and emits
  `SupplyChainScanned`. `WriteSupplyChainDigest` renders a *deterministic*
  (no-agent) markdown digest to `{FOUNDRY_SUPPLY_CHAIN_DIR}/{YYYY-MM-DD}.md` and
  emits `SupplyChainScanCompleted`. The formation is advisory and read-only: it
  never mutates a working tree and never fails a project run — a supply-chain
  advisory is an external, time-triggered fact, not a regression in the
  project's code.
- **Committed per-repo supply-chain allowlist (`.supply-chain-allow.json`).** A
  neutral artifact Foundry reads (never writes) giving the supply-chain function
  the memory a stateless gate lacks: an accepted-advisory record with an
  `expires` date. An active acceptance suppresses the advisory; a lapsed one
  resurfaces it as a live finding for re-decision. Acceptances are authored by a
  human and land through the repo's normal commit flow, so each decision lives
  in git history. Read via `foundry_sdk::supply_chain`.
- New event types `SupplyChainScanStarted`, `SupplyChainScanned`,
  `SupplyChainScanCompleted`; path helper `supply_chain_dir()` with
  `FOUNDRY_SUPPLY_CHAIN_DIR` override.

## [0.22.0] - 2026-06-12

### Added

- **Post-maintenance failure triage formation (propose-only).** After each
  nightly maintenance run (`MaintenanceSummaryRequested`), two new blocks
  classify every gate failure from that run and write a dated digest to
  `~/.foundry/triage/YYYY-MM-DD.md`. `TriageMaintenance` reads the Foundry JSONL
  event log, extracts `PreflightCompleted` failures from the run window,
  classifies them into one of 12 domain classes (format drift, vuln with fix,
  test breakage, infra flake, etc.), correlates N≥3 same-signature infra
  failures into a single suppressed `InfraIncident`, detects N≥3 consecutive
  failures on the same gate (escalate), and emits `MaintenanceTriageCompleted`
  with a typed `MaintenanceTriageCompletedPayload`. `WriteTriageDigest` renders
  a markdown triage digest (summary table, escalations, auto-fixable proposals,
  infra-suppressed incidents, policy calls, needs-investigation, benign) and
  writes it atomically; dry-run skips the write. The formation is strictly
  propose-only — no changes are applied to any project. Override the output
  directory with `FOUNDRY_TRIAGE_DIR`.

## [0.21.0] - 2026-06-12

### Added

- **Self-healing gates via optional `fix_command`.** A gate in
  `.hone-gates.json` may now declare a `fix_command` — an in-place command that
  mechanically repairs its failure (a formatter or lint autofixer). When the
  gate `command` fails and a `fix_command` is present, the runner runs the fix
  once and re-checks; a passing re-check resolves the gate and sets
  `fix_applied` on the result, leaving the repaired tree for `CommitAndPush`
  (`git add -A`) to commit. This is the structural fix for the preflight
  deadlock: a required format/lint gate previously aborted the run before the
  maintain step that would have reformatted the code, so a project with
  formatting drift failed every night with no path to self-heal. The field flows
  resolve → payload → preflight parse → runner, and `foundry gates` derivation
  now emits it for safely auto-fixable gates only (never tests, build, or
  security gates).

- **Abstract model tiers and reasoning effort, configurable per provider.**
  Blocks now request work in provider-neutral terms — a `ModelTier`
  (`deep`/`balanced`/`fast`, mirroring hopper) and a `ReasoningEffort`
  (`minimal`/`low`/`medium`/`high`/`max`) — replacing the conflated
  `AgentCapability`. Each provider resolves a tier to a concrete model id and an
  effort to its CLI token (clamping levels it does not support; e.g. claude
  `--effort` has no `minimal`/`max`). claude now receives an `--effort` flag it
  previously ignored.
- **`~/.foundry/agents.json`** — a new seed-merged config store (same discipline
  as sentinels) that overrides the baked-in per-provider tier→model and
  effort→token maps without rebuilding. Defaults ship in code so the daemon
  works with no file; on first start the full seed is written, and upgrades
  additively fill any missing provider/tier/effort key while preserving
  hand-edited values. Override the path with `FOUNDRY_AGENT_CONFIG_PATH`.
- **Codex agent backend** — `CodexAgentGateway` drives the `codex` CLI
  (`codex exec --json -o …`) as a third provider alongside `claude` and
  `opencode`, all behind the same `AgentGateway` trait. Capability maps to
  `OpenAI` models with `model_reasoning_effort`; `ReadOnly` access maps to
  codex's *enforced* `-s read-only` sandbox (a real guarantee opencode's
  advisory mode lacks), `Full` to `--dangerously-bypass-approvals-and-sandbox`.
  Validated against `codex-cli` 0.134.0.
- **Per-request agent provider override.** A run may select its backend via an
  `agent_provider` field on the request event (`claude` | `opencode` | `codex`),
  carried through the whole iterate/maintain chain in `ChainContext` and honored
  by every agent invocation. Surfaced on the CLI as `--agent` for
  `foundry iterate`, `foundry scout`, and `foundry pipeline` (e.g.
  `foundry iterate myproj --agent codex`). Unknown names are rejected at the
  CLI.
- `AgentProvider` enum and an `AgentRequest.provider` field in the SDK gateway
  contract; a new `RoutingAgentGateway` dispatches each request to the matching
  backend, falling back to a process default.

### Changed

- `FOUNDRY_AGENT_PROVIDER` is now the *default* provider rather than the sole
  selector. The daemon constructs all three backends up front and routes per
  request; absent an override, requests use this default (still defaulting to
  `claude`, with a warning on an unknown value).
- **Event taxonomy renames** (wire format changes — no aliases, hard cutover):
  - `PromptExecutionRequested` → `ExecutionRequested` (wire:
    `prompt_execution_requested` → `execution_requested`)
  - `CommitSummaryComposed` → `CommitSummaryCompleted` (wire:
    `commit_summary_composed` → `commit_summary_completed`)
  - `OpsSummaryComposed` → `OpsSummaryCompleted` (wire: `ops_summary_composed` →
    `ops_summary_completed`)
  - `GreetingComposed` is **retained** — it is a genuine domain fact in a
    two-event sequence (`GreetingComposed` → `GreetingDelivered`), not a
    lifecycle end.
  - **Caveat:** pre-rename events persisted in `~/.foundry/events/*.jsonl`
    carrying the old wire strings will deserialize as `Custom(...)` — this is
    acceptable for historical records and does not affect current workflow
    execution.

### Fixed

- **Benign triage declines no longer inflate the maintenance run status table.**
  `GenerateSummary` now treats a terminal `success: false` whose summary is a
  benign decline ("triage rejected", "no correction warranted", "unknown
  violation") as a `Success` rather than a `Failed` row — these are no-ops, not
  defects. Backed by a shared `is_benign_decline` predicate reused by the triage
  formation. Also corrected a stale comment in `create_plan` that described the
  rejection terminal as `success: false` (it has emitted `success: true` since
  triage rejections became benign no-ops).

## [0.20.0] - 2026-05-29

This release adds the **ops-digest** formation — a periodic summary of business
activity from the MBOS event stream, the operational counterpart to v0.19.0's
commit digest. A new `ops-digest` sentinel fires every three hours as a cheap
heartbeat; a pressure gate in the first block decides whether each tick produces
a digest, so the effective cadence tracks activity rather than the clock.

### Added

- Ops-digest formation — a three-block chain (`ObserveEvents` →
  `SummarizeEvents` → `WriteOpsDigest`) that reads MBOS JSONL event files,
  applies a pressure gate (≥25 new events or any anomaly), asks the agent to
  summarise the operational period, and writes
  `{FOUNDRY_OPS_DIGESTS_DIR}/{YYYY-MM-DD}.md`. Anomaly classification covers P0
  urgency, CI pipeline failures, unresolved maintenance interventions,
  high/critical vulnerability alerts, and maintenance runs with failed repos.
- `ops-digest` sentinel (`0 */3 * * *`) added to the canonical default seed,
  emitting `OpsDigestStarted` every three hours. Existing installs pick it up
  automatically on the next daemon restart via the additive seed-merge.
- Four new event types: `OpsDigestStarted` (span opener), `OpsObserved`,
  `OpsSummaryComposed`, `OpsDigestCompleted`, with matching typed payload
  structs (`OpsEventDigest`, `OpsObservedPayload`, `OpsSummaryComposedPayload`,
  `OpsDigestCompletedPayload`).
- Three new path helpers in `foundry_core::paths`: `ops_digests_dir()`
  (`FOUNDRY_OPS_DIGESTS_DIR`), `ops_events_intake_dir()`
  (`FOUNDRY_OPS_EVENTS_DIR`, defaults to `~/Work/Operations/Events/intake`), and
  `ops_watermark_path()` (`~/.foundry/ops-digest.watermark`). The watermark
  advances atomically after each successful digest write so subsequent runs only
  process newer events.
- Watermark-based incremental ingestion: first-run lookback is 24 hours when no
  watermark exists. Malformed JSONL lines are skipped silently.

## [0.19.0] - 2026-05-28

This release ships the **Sentinel** subsystem and its first two formations: the
nightly maintenance run (internalised from launchd) and the daily commit digest.
Sentinels are declarative, named, scheduled triggers that live inside `foundryd`
and emit configured events when their schedule fires, replacing the per-machine
launchd plist pattern that previously drove proactive workflows.

### Added

- `foundry_core::sentinel` module — `SentinelStore`, `SentinelEntry`, `Schedule`
  (externally-tagged enum so future kinds extend additively), `EmitSpec`
  (strongly-typed `EventType` on the wire), and `SentinelMutationError`.
  `default_seed()` ships the canonical set; `merge_default_seed_into` adds
  missing-by-name entries to existing stores on every daemon start, so new
  Foundry releases that ship more canonical sentinels reach existing installs
  automatically without manual JSON edits.
- In-process scheduler (`foundryd::scheduler`) — a tokio task that watches
  `~/.foundry/sentinels.json`, computes the soonest enabled firing across all
  entries, and emits the configured event when due. `tokio::select!` between the
  deadline and a `Notify` reload signal so mutations take effect immediately.
  Standard 5-field cron expressions are auto-padded to the 6-field shape the
  `cron` crate expects. Catch-up policy is "skip missed firings".
- Two new gRPC RPCs — `SentinelEnable` and `SentinelDisable` — plus the proto
  `Sentinel` message. Both wake the scheduler's reload notify so the next firing
  is recomputed immediately.
- CLI subcommands: `foundry sentinel list | show | enable | disable`, mirroring
  the `foundry registry` shape exactly. `list` and `show` always read the file
  directly; `enable` and `disable` try gRPC and fall back to direct file
  mutation with a warning when the daemon is unreachable.
- `nightly-maintenance` sentinel (`0 2 * * *`) in the default seed,
  internalising the schedule previously held by
  `launchd/com.mojility.foundry-maintenance.plist`.
- `daily-commit-digest` sentinel (`0 17 * * *`) in the default seed, driving a
  new linear formation: `ObserveCommits` (Observer; runs
  `git log --since="24 hours ago" --no-merges --pretty=format:...` per active
  project, captures per-project errors inline so the chain continues),
  `SummarizeCommits` (Observer; agent-invoked Reasoning, ReadOnly access, with
  Canadian English / `##` per project / 7-char SHA / ⚠ Heads-up section /
  no-fabrication prompt rules; short-circuits the agent call on empty days),
  `WriteCommitDigest` (atomic tempfile-then-rename to
  `{FOUNDRY_DIGESTS_DIR}/{YYYY-MM-DD}.md`; dry-run firings run the full chain
  but skip the file write).
- Four new event types: `CommitDigestStarted` (span opener), `CommitsObserved`,
  `CommitSummaryComposed`, `CommitDigestCompleted`, with matching typed payload
  structs (`CommitInfo`, `ProjectCommits`, and the four `*Payload` types).
- New paths: `~/.foundry/sentinels.json` (overridable via
  `FOUNDRY_SENTINELS_PATH`) and `~/.foundry/digests/YYYY-MM-DD.md` (overridable
  via `FOUNDRY_DIGESTS_DIR`).
- mdBook chapters `book/src/guide/sentinels.md` and
  `book/src/guide/commit-digest.md`, plus AGENTS.md and skill updates.

### Changed

- `service::emit()` now delegates to a shared `service::spawn_workflow` helper
  that both the gRPC handler and the in-process scheduler use, so sentinel
  firings appear in `foundry status` and produce traces identically to
  user-triggered runs.

### Removed

- `launchd/com.mojility.foundry-maintenance.plist`. The nightly schedule now
  lives in `~/.foundry/sentinels.json`. Upgrade path is in `launchd/README.md`
  and `book/src/guide/sentinels.md`: unload the legacy plist and `rm` it; the
  canonical seed merge populates `nightly-maintenance` on the next daemon start.

### Dependencies

- Added `cron = "0.16"` to `foundryd` for parsing sentinel schedules.

## [0.18.1] - 2026-05-22

### Fixed

- A triage rejection is now always classified as a successful no-op. Previously
  `create_plan` split rejections by the assessor's severity number: below the
  triage threshold was `success: true`, but at-or-above threshold was reported
  as `success: false`, so a project whose only finding was triaged away as
  busy-work showed up as a failed maintenance run. Severity is already folded
  into the triage agent's accept/reject decision, so re-deriving a failure
  verdict from it in a later block was redundant and wrong. Both rejection paths
  now emit `ProjectIterationCompleted { success: true }`, carrying the triage
  agent's reason in the summary.

## [0.18.0] - 2026-05-21

### Removed

- The `audit_only` throttle level. It executed Mutator blocks for real but
  suppressed delivery of their events, so a chain halted at the first mutation —
  a confusing middle ground whose name implied the opposite of its behaviour.
  `Throttle` is now binary: `full` (run for real) and `dry_run` (simulate
  Mutators). The proto throttle encoding is now `0 = full`, `1 = dry_run`. No
  workflow depended on the halt-at-first-mutation behaviour.

### Changed

- `FanOutMaintenance` now uses the scatter/gather primitive. A system
  maintenance cycle scatters its per-project `ProjectRunStarted` events and
  gathers their `ProjectRunCompleted` terminals; the engine synthesizes the
  cycle's `MaintenanceCycleCompleted` as a genuine fan-in. The service layer no
  longer hand-aggregates per-project completions — it persists per-project
  traces and emits a new `MaintenanceSummaryRequested` command that triggers
  `GenerateSummary`. The `MaintenanceCycleStarted` payload struct
  `MaintenanceCycleCompletedPayload` is renamed
  `MaintenanceSummaryRequestedPayload` (same fields). `foundry run` detects
  cycle completion from `maintenance_summary_requested` rather than
  `maintenance_cycle_completed`.

### Added

- Events now carry an optional `causation_id` — the `id` of the event that
  triggered the block which emitted it. This records the domain causality edge
  directly in the event envelope, independent of the observability span
  structure (`trace_id`/`span_id`/`parent_span_id`). The engine stamps it
  unconditionally on every emitted event ("set if unset"); root events carry no
  `causation_id`. Legacy events without the field continue to parse. Groundwork
  for native fan-out/fan-in coordination.
- Events now carry an optional `gather_id` — the fan-out (scatter/gather) group
  the event belongs to. Unlike `causation_id`, it propagates verbatim to every
  descendant like `trace_id`, so a scattered child workflow's terminal
  `*Completed` event still identifies its group across span-opener boundaries.
  `None` outside any fan-out; legacy events continue to parse. Groundwork for
  the gather/reduce engine primitive.
- Native scatter/gather (map/reduce) coordination. A task block can return a
  `Scatter` in its `TaskBlockResult` (`TaskBlockResult::scattering`) declaring a
  set of child events and a `GatherSpec`. The engine mints a `gather_id`, stamps
  and dispatches the children, counts their completions, and synthesizes a
  reduce event (`GatherCompletedPayload`) once the `GatherPolicy` (`All` or
  `Count(n)`) is satisfied. Gathers nest — a reduce event rejoins its enclosing
  group — and an empty scatter reduces immediately. For now a gather completes
  within a single `process()` traversal; durable, cross-call gathers are a later
  step behind the same interface.

## [0.17.1] - 2026-05-20

### Fixed

- Below-threshold triage rejections are now classified as successful no-ops
  rather than failures. A project that has stabilized — where the iterate triage
  finds no correction warranted — no longer shows as `failed` in maintenance run
  summaries.
- Terminal events now emit with accurate success status on all trace paths.
- `span_context` no longer carries an inherited `TRACEPARENT` when the context
  is absent.
- `retry_execution` replaces an `expect` with a graceful fallback in
  `dry_run_events`.

### Changed

- Internal refactors: event serialization and agent JSON parsing consolidated in
  `blocks`; loose parameters encapsulated into `ProjectSpec`, `ProjectEdits`,
  and `ExecutionContext`.
- Upgraded `rand` 0.8 → 0.10.

## [0.17.0] - 2026-05-15

### Breaking changes

- **Event taxonomy renames** (no aliases — hard cutover):
  - `MaintenanceRunStarted` split into `MaintenanceCycleStarted` (cycle root)
    and `ProjectRunStarted` (per-project).
  - `MaintenanceRunCompleted` split similarly into `MaintenanceCycleCompleted`
    and `ProjectRunCompleted`.
  - `IterationRequested` → `ProjectIterationRequested`.
  - `MaintenanceRequested` → `ProjectMaintenanceRequested`.
  - `GreetRequested` → `GreetingRequested`.
  - Snake_case wire strings change accordingly (e.g. `iteration_requested` →
    `project_iteration_requested`).
- **`trc_*` trace IDs replaced** with OpenTelemetry-compatible 32-char lowercase
  hex. Legacy `trc_*` IDs remain readable (a helper `is_legacy_trace_id`
  discriminates), but newly-emitted events use the new format.
- **`FanOutMaintenance` no longer mints a fresh `trace_id` per project**.
  Per-project events inherit the cycle root's `trace_id` via the engine's
  span-stamping pass, so every event in a nightly batch shares one `trace_id`.

### Migration

Run once after upgrading and **before** restarting `foundryd`:

```bash
scripts/migrate-event-names.sh --dry-run   # review counts
scripts/migrate-event-names.sh             # apply
```

The script rewrites archived event names in `~/.foundry/events/*.jsonl` and
`~/.foundry/traces/**/*.json`, recomputes `Event::id` for renamed events, and
fixes up payload references (`project_trace_ids`, `root_event_id`). `foundryd`
refuses to start (exit code 2) if it detects legacy event names on disk and
prints the remediation command.

### Added

- **OpenTelemetry-shaped nested tracing**: every `Event` now carries `span_id`
  (16-char hex) and `parent_span_id` (16-char hex) alongside `trace_id` (32-char
  hex). The engine stamps these per two rules: emitted events default to the
  trigger's span; events registered as *span openers* (`*Requested` workflow
  events, `*Started` lifecycle events, `RemediationStarted`) get a fresh
  `span_id` parented to the emitting block's span. Block-level spans are
  recorded on `BlockExecution` (not on emitted events), so the call tree can be
  reconstructed without changing event volume on the wire.
- **New `Span` RPC** (`proto/foundry.proto`): retrieve every event and block
  execution within a single span. Phase 2 stub returns `found = false`; Phase 6
  wires it to a real `span_id → trace_id` index in the trace store.
- **`mint_span_id` / `mint_trace_id`** in `foundry_core::event`: both
  hex-encoded random output (16-char and 32-char respectively).
- **`EventType::is_span_opener`**: predicate over the span-opener registry. Used
  by the engine's stamping pass to decide between the default rule and the
  span-opener rule.

### Changed

- **`EventType::IterationRequested` → `ProjectIterationRequested`** and the
  parallel renames listed above. Payload structs renamed in lockstep
  (`IterationRequestedPayload` → `ProjectIterationRequestedPayload`, etc.).
- **`MaintenanceRunStartedPayload`/`MaintenanceRunCompletedPayload`** removed
  and replaced by four new payload structs: `MaintenanceCycleStartedPayload`,
  `ProjectRunStartedPayload`, `MaintenanceCycleCompletedPayload`,
  `ProjectRunCompletedPayload`. The cycle-level completion still carries
  `project_trace_ids` / `skipped_projects` / `total_duration_ms` for
  `GenerateSummary` compatibility; the per-project completion is a leaner shape
  (`success` + optional `root_event_id`).
- **`AGENTS.md` event taxonomy examples** updated to new names.

## [0.16.1] - 2026-05-13

### Fixed

- **`ExecutePlan` short-circuits when `correction_needed=false`**
  (`crates/foundryd/src/blocks/execute_plan.rs`): when `CreatePlan` returns
  `correctionNeeded: false` (the Reasoning agent inspected the codebase and
  concluded the assessment was inaccurate), `ExecutePlan` now emits a synthetic
  `ExecutionCompleted` event immediately and skips the Coding-agent invocation.
  Previously the Coding agent was invoked unconditionally with an imperative
  "the plan MUST be applied" prompt, burning ~48s of Full-access tokens per
  occurrence on a plan that prescribed no changes. The new path preserves
  `correction_reason` in `execution_output` for trace visibility, sets
  `changes_detected: false`, and patterns on `CreatePlan`'s `accepted=false`
  short-circuit.

### Changed

- **`docs` gate added to `.hone-gates.json`**: foundry's own quality-gate suite
  now runs `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` as a
  required gate, matching the CI "Check documentation compiles" step.
  `iterate`/`validate`/maintenance runs previously only checked
  fmt/clippy/test/coverage/deny, so doc-link breakage (e.g. the 0.16.0 private
  intra-doc link) could pass local gates and only fail in CI.

## [0.16.0] - 2026-05-13

### Added

- **gRPC registry mutations** (`proto/foundry.proto`, `foundry-core`,
  `foundryd`, `foundry-cli`): `foundry registry add|remove|edit` now route
  through the daemon's gRPC API (`RegistryAdd`, `RegistryRemove`, `RegistryEdit`
  RPCs) when the daemon is reachable, so the in-memory registry stays consistent
  with `registry.json` across concurrent CLI calls. Pass `--offline` to bypass
  gRPC and mutate the file directly (useful when the daemon is not running). The
  daemon holds `Arc<RwLock<Registry>>` and persists changes atomically via
  `Registry::save` inside the write-lock guard. `foundry-core` gains
  `ProjectSpec`, `ProjectEdits`, `RegistryMutationError`, and `parse_stack` to
  support typed add/edit operations.

### Changed

- **CI maintenance — Node 24 action bumps** (`.github/workflows/`): bumped
  pinned GitHub Actions ahead of the forced Node.js 24 runner cutover
  (2026-06-02) and the Node 20 removal (2026-09-16). `actions/checkout@v4` →
  `@v6`, `actions/upload-artifact@v4` → `@v7`, `actions/download-artifact@v4` →
  `@v8`, `actions/upload-pages-artifact@v3` → `@v5`, `actions/deploy-pages@v4` →
  `@v5`. `Swatinem/rust-cache@v2` already resolves to a Node 24 release;
  `dtolnay/rust-toolchain` and `EmbarkStudios/cargo-deny-action` are
  composite/Docker actions and unaffected. No workflow logic changed.

### Fixed

- **`cargo doc` broke on a private intra-doc link**
  (`crates/foundryd/src/engine.rs`): `Engine::with_event_writer`'s doc comment
  linked `[`EventWriter`]`, which is crate-private, so
  `cargo doc --workspace -D warnings` (a CI gate) failed. Replaced with a plain
  code span.

## [0.15.1] - 2026-05-12

### Fixed

- **Agent session events carried an empty `project`**
  (`crates/foundryd/src/gateway.rs`, `blocks/mod.rs`, `blocks/release.rs`):
  `ClaudeAgentGateway::invoke` hard-coded `project: String::new()` when emitting
  `AgentSessionStarted`/`AgentSessionEnded` events and the started payload, so
  ops-visualizer's `/agents` view showed every session with a blank project.
  `AgentRequest` (and `ReleaseInput`) now carry a `project` field; callers in
  `invoke_agent` and the release adapters thread the registry project name
  through, and the gateway emits it on both events. No proto / gRPC schema
  change — `WatchResponse.project` already carried the event's `project`.

## [0.15.0] - 2026-05-11

### Added

- **`correctionNeeded` flag in `PlanCompleted`** (`foundry-core`, `foundryd`):
  the plan agent now emits a machine-readable fenced JSON block at the end of
  its response declaring `{ "correctionNeeded": true|false, "reason": "..." }`.
  When `false`, a clean working tree after `ExecutePlan` is treated as a
  **legitimate no-op** (success) rather than an agent flake (failure → retry).
  This mirrors hone's busy-work-containment semantics: an agent that correctly
  concludes "nothing to do" is no longer penalised with up to 4 redundant
  retries.
  - `PlanCompletedPayload` gains two new serde-default fields:
    `correction_needed: bool` (defaults to `true`) and
    `correction_reason: String`.
  - `build_agent_execution_result` and `build_execution_outcome` in `mod.rs`
    accept a `correction_needed: bool` parameter; the clean-tree override
    branches on it.
  - `RetryExecution` and `ExecuteMaintain` always pass `true` (correction is
    required when retrying or maintaining).
  - Parse is fail-closed: any output without a valid boolean `correctionNeeded`
    field → `correction_needed = true`.

## [0.14.2] - 2026-05-10

### Fixed

- **Silent no-op guard misfiring on agents that commit their work**
  (`crates/foundryd/src/blocks/mod.rs`, `execute_plan.rs`,
  `execute_maintain.rs`, `retry_execution.rs`): `detect_post_execution_changes`
  previously ran only `git status --porcelain`, which sees uncommitted changes.
  Agents that commit and push their work (Claude Code's default behaviour) left
  a clean working tree, so the detector reported `changes_detected: false` and
  the 0.14.1 silent-no-op guard then incorrectly marked the run as a failure —
  causing 4× redundant retries each producing real, committed work. Reproduced
  in production on the 2026-05-10 maintenance run (10/20 projects falsely
  flagged). Fix: capture HEAD via `git rev-parse HEAD` immediately before agent
  invocation, then detect changes via `git diff --name-only <pre_sha>` which
  captures both committed and uncommitted changes since the snapshot. Falls back
  to porcelain on diff failure or when no pre-sha is available.

## [0.14.1] - 2026-05-09

### Fixed

- **Iterate workflow silent no-op** — closes the failure mode where the iterate
  agent could exit successfully without applying its plan (302 of 382 traces
  over the prior 30-day window). Two complementary changes:
  - **Execute prompt strengthened**
    (`crates/foundryd/src/blocks/execute_plan.rs`): `build_execution_prompt` now
    uses imperative requirements that explicitly invalidate "gates already pass"
    as a stopping condition and require the working tree to contain
    modifications when the plan is complete. Maintain prompt unchanged.
  - **Block-level no-op detection** (`crates/foundryd/src/blocks/mod.rs`,
    `run_verify_gates.rs`): when the iterate workflow's agent exits cleanly but
    `changes_detected == false` (or only auxiliary paths like
    `.claude/worktrees/` were touched), the execution result is overridden to
    `success: false` with summary
    `"agent did not modify any files (silent no-op)"`. The downstream chain
    treats this as a real failure: `RunVerifyGates` short-circuits with a
    synthetic `agent_execution` gate, which routes through retry up to
    `--max-retries`, eventually surfacing
    `ProjectIterationCompleted { success: false }` if retries don't recover.
    Maintain workflow is explicitly unaffected — `changes_detected: false`
    remains a successful maintenance run when deps are already current.

## [0.14.0] - 2026-05-09

### Added

- **Agent session visibility v1** — `foundryd` now emits `AgentSessionStarted`
  and `AgentSessionEnded` events around every Claude agent invocation, capturing
  session lifecycle in the event stream. New `AgentSessionStarted` /
  `AgentSessionEnded` payload structs in `foundry-core`, `agent_sessions_dir()`
  path helper, and `AgentStreamRunner` trait with `ProcessAgentStreamRunner`
  implementation that runs `claude` with `--print --output-format stream-json`
  so per-tool-call lifecycle events flow through the engine in real time. Wires
  `event_tx` and `ProcessAgentStreamRunner` into `ClaudeAgentGateway`.
- **Per-gate timing** — `GateRunResult` now carries `duration_ms`, and
  `RunVerifyGates` / `RunPreflightGates` populate it for every gate executed.
  Unblocks "agent thinking vs build/test time" decomposition in event analyses.
- **Working-tree change detection on execution** — `ExecutionCompletedPayload`
  carries `changes_detected: bool` and `files_changed: Vec<String>`, populated
  via `git status --porcelain` after agent execution. Surfaces silent no-op
  cases (agent claims success while leaving the tree clean) directly in events
  instead of requiring inference.
- **Coverage gate** — foundry's own gate suite now runs
  `cargo tarpaulin --fail-under 61` as a required gate.
- Unit tests for `greet`, `check_pipeline`, and `watch_pipeline` blocks in
  `foundryd`.

### Changed

- Internal refactor: extracted shared agent-remediation and gate-result builder
  helpers across `execute_plan`, `execute_maintain`, and `retry_execution` to
  remove duplication.

## [0.13.0] - 2026-04-21

### Added

- `WorkflowStatus.trace_id` on the `Status` gRPC response. The root event's
  `trace_id` is now tracked on `ActiveWorkflow` and surfaced through `Status`,
  letting dashboards and external tools group the live `Watch` stream events by
  the workflow they belong to. Additive proto field (tag 8), backward-compatible
  for existing clients.

## [0.12.0] - 2026-04-20

### Changed

- **Breaking**: `foundryd` now binds to `127.0.0.1:50051` (IPv4 loopback)
  instead of `[::1]:50051` (IPv6 loopback). The CLI default `--addr` tracks this
  change (`http://127.0.0.1:50051`). Motivation: several client HTTP/2 stacks
  (notably Elixir's `:grpc`/Gun adapter) can't pass the `:inet6` transport
  option needed to dial an IPv6-only listener through their `transport_opts`
  plumbing, so pure-IPv6 binding blocked off legitimate localhost clients. IPv4
  loopback is still localhost-only and works for every stack. No change is
  needed for users who rely on the CLI default; anyone who passed
  `--addr http://[::1]:50051` explicitly must update to
  `http://127.0.0.1:50051`.

## [0.11.2] - 2026-04-20

### Added

- `foundry registry show <project>` now displays an `Installs skill:` line
  summarizing the `installs_skill` field —
  `yes (default -- runs <derived-command>)`, `command: <command>`, or
  `no (explicitly disabled)`. The displayed default command comes from the same
  derivation function `InstallLocally` uses, so what's shown is always what
  would actually run.
- `foundry registry list` adds a narrow `Skill` column showing `auto` / `cmd` /
  `off` / blank per project.

## [0.11.1] - 2026-04-20

### Added

- `foundry init` brought up to the canonical Mojility skill-install spec: new
  `--force` and `--json` flags, version-guard that refuses to overwrite when the
  installed skill version is newer than this binary (override with `--force`),
  version-stamping (`foundry-version: <X>` written into installed file
  frontmatter), and per-file action reporting (`Created` / `Updated` /
  `UpToDate` / `Skipped`). Exit code is non-zero when any file is skipped. With
  this change, `installs_skill: true` can safely be set on foundry's own
  registry entry.

## [0.11.0] - 2026-04-20

### Added

- `installs_skill` registry field — when set, the `InstallLocally` block
  automatically runs a per-tool skill installer after the binary install step of
  a release, so `~/.claude/skills/<name>/SKILL.md` is refreshed without a manual
  `<tool> init`. Accepts `true` (runs `<binary> init --global --force`) or
  `{ command: "..." }` for tools whose skill-install verb differs (e.g.
  `gilt skill-init --global --force`). Absent field preserves current behavior.
  New `LocalSkillInstallCompleted` event is emitted on success or failure;
  skill-install failure is a soft warning and does not fail the parent
  `InstallLocally` block.

## [0.10.0] - 2026-04-10

### Added

- `CleanupBranches` task block — automatically deletes merged local branches and
  removes stale git worktrees during project validation, preventing accumulation
  of leftover branches from hopper jobs and Claude Code agent sessions

## [0.8.0] - 2026-03-29

### Added

- `foundry init` command — installs the bundled Foundry skill for Claude agents
- `foundry init --global` — installs to `~/.claude/skills/foundry/` instead of
  local `.claude/skills/foundry/`
- Skill files embedded in the binary via `include_str!()`, updated on every
  release
- Event naming convention documentation in AGENTS.md

### Changed

- `AutoReleaseTriggered` renamed to `ReleaseRequested` (commands use
  `*Requested` suffix)
- `AutoReleaseCompleted` renamed to `ReleaseCompleted`
- `GatesResolved` renamed to `GateResolutionCompleted` (lifecycle endpoints use
  `*Completed` suffix)
- `ProjectIterateCompleted` renamed to `ProjectIterationCompleted` (noun form
  for compound prefixes)
- `ProjectMaintainCompleted` renamed to `ProjectMaintenanceCompleted` (noun form
  for compound prefixes)
- `CharterCheckCompleted` payload field `passed` renamed to `success` for
  consistency

## [0.3.0] - 2026-03-26

### Added

- Open-source release under MIT license (svetzal/foundry)
- GitHub Pages documentation site via mdBook
- Homebrew tap distribution (`brew tap svetzal/tap && brew install foundry`)
- Summary module for automated maintenance reports
- Orchestrator for automated maintenance workflows
- Exit condition for `foundry watch` stream

### Changed

- Repository transferred from Mojility org to svetzal personal account
- Registry action flags forwarded in validation payload
- CI pipeline now installs `protoc` for proto compilation

## [0.2.0] - 2026-03-22

### Added

- Async emit: `Emit` RPC now returns immediately, processing runs in the
  background
- `--wait` flag on `foundry emit` to block until processing completes and
  display the trace
- Workflow status tracking: `foundry status` shows active in-flight workflows
- `WorkflowTracker` module with RAII `WorkflowGuard` for robust cleanup
- `ShellGateway` trait for I/O abstraction in task blocks (functional core /
  imperative shell)
- `FakeShellGateway` for deterministic, fast unit tests
- Configurable per-project `timeout_secs` in the registry (defaults to 30
  minutes)
- Project charter (`CHARTER.md`)

### Changed

- All task blocks refactored to use `ShellGateway` dependency injection instead
  of hard-wired shell calls
- Workspace lint configuration deduplicated into root `Cargo.toml`
- Block tests now use fakes instead of real shell commands

## [0.1.0] - 2026-03-15

### Added

- Event-driven workflow engine with queue-based event propagation
- Three-crate workspace: `foundry-core`, `foundryd`, `foundry-cli`
- gRPC service with `Emit`, `Status`, `Watch`, and `Trace` RPCs
- Task block library: `ValidateProject`, `ComposeGreeting`, `DeliverGreeting`,
  `ScanDependencies`, `AuditReleaseTag`, `AuditMainBranch`,
  `RemediateVulnerability`, `CommitAndPush`, `CutRelease`, `WatchPipeline`,
  `InstallLocally`, `RouteProjectWorkflow`, `RunHoneIterate`, `RunHoneMaintain`
- Throttle control: `full`, `audit_only`, `dry_run`
- Project registry (`~/.foundry/registry.json`) with per-project configuration
- JSONL event writer for persistent event logging
- Trace store with 1-hour TTL for completed event chains
- `foundry run` command for triggering maintenance workflows with live streaming
- Maintenance orchestrator with per-project concurrency guards
- Configurable retry policies per task block
- Stack-specific audit scanner (Rust, Python, TypeScript, Elixir)
- mdBook documentation
- launchd plist files for daemon and scheduled runs
