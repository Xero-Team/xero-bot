# Durable trigger state

The server always opens `XERO_DATA_DIR/command-triggers.sqlite`, even with
`IDLE_WORKFLOWS_ENABLED=false`. Use one deployment process and a persistent local
volume. A second owner of the same database fails at startup. Different volumes
are **not coordinated**; active replicas are unsupported. Docker Compose already
mounts `/data`. Keep the database and its WAL together; stop the server before a
filesystem backup or offline administration. Existing databases migrate transactionally through version 4, retaining intents,
receipts, sessions and recipient reservations; notification scope migration is
described below. See the [English](triggers.md) / [Chinese](triggers.zh-CN.md)
upgrade guide for trigger policy and deployment changes.

## Ingress and execution

After signature verification, the server commits a recovery envelope before
acknowledging actionable `issue_comment.created`, `issues.opened`, and
`pull_request.opened/synchronize`. It keeps delivery, repository, installation,
thread/comment/user IDs, GitHub source time, applicable head/base SHA and the
comment text required for parsing. It does not store authentication headers,
installation tokens or arbitrary webhook fields. Missing required metadata is a
400; persistence failure is a retryable 503, with no background command execution.
Pure routing filters ordinary prose and self replies before storing comment text;
syntax diagnostics and executable command candidates still enter the inbox.

Comment work uses up to eight concurrent durable workers. Free slots are refilled
while other work runs, including for events arriving during a slow review. Each
delivery keeps its own timeout and recovery ownership. A local bookkeeping,
claim, or periodic cleanup error stops new claims and maintenance. The pump keeps
polling already claimed workers until they finish or reach their own deadlines,
then returns the first error. It does not cancel healthy in-flight writes because
another delivery failed to save its state. Opened envelopes execute the explicit
whitelist from `event_triggers`; PR path envelopes execute the verified label
and explicit notification actions from `path_triggers`. Labels and notifications
use the same durable inbox and independent operation receipts.
Rebase, CodeQL label,
native review/merge queue, and idle workflow routing keep their existing switches
and are not dispatched a second time by this worker.

The business key for a manual command is its repository ID, comment ID and
canonical command/arguments, independent of delivery ID. Aliases and normalized
login lists share a key. Opened actions have a repository/thread/stable rule ID
key; notification recipients have an installation/repository/PR/lowercase-login
key. Configuration and head SHAs are audit/snapshot data, never part of a lifetime
recipient key. Recipient uniqueness, budget queries, completeness proofs, path
subscriptions, snapshots, operation keys and per-PR locks all include installation.
Each installation/repository/PR has a separate lifetime budget. Repeated events,
configuration edits and restarts within that scope never reset it.

The first successfully loaded opened-event plan is frozen in `opened_plans`,
including an empty plan. It survives inbox cleanup and prevents configuration
changes or redelivery from backfilling old threads. Equivalent actions share an
operation keyed by the first sorted stable rule ID, with all matched IDs retained
in its intent. Before each action/recovery, current default-branch policy must
still contain at least one original rule with the same canonical action and
parameters. Removed/changed/disabled plans are superseded. Independent actions
can finish even when a sibling is paused.

A verified invalid TOML document or invalid events domain (including duplicate
IDs and the 32-rule limit) is a definite refusal. It logs its reason and freezes
an empty plan, allowing the envelope to finish and age out normally. Repairing
that configuration cannot backfill the refused thread. Existing frozen plans are
revoked under an invalid policy. Transport, permission, rate-limit and incomplete
response errors remain retryable because they do not establish repository intent.
Revocation and cancellation of unsent children commit together **before** remote
reconciliation; failed lookups cannot erase that veto. Sent unknown children keep
their evidence, and a subsequently proven-unsent child is superseded too.

Policy is checked before starting or recovering an action. Disabling a rule does
not cancel a computation already running under its verified snapshot. For an
emergency stop, stop the service/worker and inspect the resulting unknown writes
before resuming; the stopped computation is not automatically repeated.

Automatic review/report computation is marked started before invoking its engine.
After interruption it can adopt a confirmed result receipt, but never recomputes
AI output automatically. A started computation with no confirmable output stays
unknown for operator inspection. Progress and engine-fallback announcements use
a separate API and are omitted for automatic work. Final automatic comment
intents carry an explicit report flag; legacy unclassified comment receipts do
not prove completion. Any pending child also prevents completion. Labels remain an
idempotent ensure: reconcile first, then retry only under compatible current policy.
Report bodies include event, matched rule IDs, configuration commit and actual PR
head; API review diffs use immutable base/head comparisons, and COMMENT reviews
include the actual commit ID. Subprocess checkouts must match that snapshot.

Each command and each of its external writes have separate rows. An immediate
SQLite transaction claims a unique key before a GitHub call. Attempt numbers
fence late completions and stale reconciliation results. The immutable original
delivery remains audit data; `lease_delivery` identifies the current attempt owner
when a business operation is reclaimed by another delivery. Successful sibling commands and external writes reuse
receipts rather than sending another request. The worker replans through current
repository configuration, applicability and existing command permission checks;
it does not send stored requests directly from the inbox. A changed PR head/base
supersedes unsent work. Issue 14 consumes the session primitives and adds the
additional `r-` permission gate.

## Uncertain writes

States are `pending`, `running`, `succeeded`, `failed`, `unknown`, and `superseded`.
On startup or after a cancelled/timed-out worker, abandoned `running` operations
first become `unknown`. Each worker has a 15 minute execution limit. An interrupted
delivery only fences its own attempts, after its request futures have been dropped;
healthy concurrent deliveries are unaffected. Retryable
planning/configuration failures use exponential backoff, capped at one hour.

A send-intent record is committed before the HTTP request. If it was never
recorded, recovery can prove the request was not sent. If it was recorded,
process failure, timeout, or 5xx is ambiguous. There is no automatic POST retry.

Comments and reviews carry a stable `<!-- xero-trigger:SHA256 -->` marker.
Reconciliation checks the complete paginated collection and requires the marker,
matching `performed_via_github_app.id`, Bot author type and the resolved App bot
login. A human copying a marker, another App, or a missing author identity cannot
confirm success. An absent/deleted marker or failed lookup leaves the write
`unknown`. ERROR logs include its operation and delivery; offline queries show
its status and diagnostic. It is not resent automatically.

“Ensure these labels exist” checks current labels first. A confirmed set succeeds;
a missing set can be retried with backoff after live preflight. This idempotent
exception does not extend to comments, reviews, assignments or arbitrary writes.
Known HTTP rejections are recorded separately and need an operator retry when
appropriate. Bodyless DELETE 204 responses are recorded as success without JSON
parsing; DELETEs that return assignment data still retain that response. Removing
an absent label also records a successful no-op. Review fallback after a definite
rejection keeps separate receipts.

**SQLite and GitHub do not share a transaction. This is not exactly-once delivery.**
Pausing ambiguous writes trades automatic compensation for fewer duplicates. A
request may be omitted until an operator resolves it; a deleted remote result
cannot safely be distinguished from a write that never happened. Operator evidence
is part of the trust boundary, not a new automatic guarantee.

## Notification and session primitives

`Store::claim_notification` reserves the action and available recipient slots in
one transaction, across all rules and pushes. It validates personal logins,
normalizes case, and returns **only the newly reserved recipients**. The consumer
must build its aggregate comment from that list and skip sending when it is empty.
The total budget is 0–10. Success commits reservations; unknown keeps them;
confirmed no-send releases them. A sent operation cannot simply be superseded to
reclaim slots. Recipient records and successful/unknown actions have no ordinary
expiry or seven-day scheduler cleanup. Closing/reopening a PR, configuration
changes and deleting comments do not reset them. Path matching/aggregation is #16/#17.

A missing account (404), a verified non-personal account, or a renamed login
permanently fails the snapshot's notification operation and releases its unsent
reservations. The error records the login and rule IDs, while the processed inbox
can finish. This aggregate is not partially sent; correct the configuration for a
later supported snapshot. Transport, permission/rate-limit/server errors and
incomplete identity responses remain retryable before any send.

`request.suppressed` records excess
canonical recipients without reserving slots; public comments expose only their
count. Per-PR processing is serialized, with reservation and operation claim in
one SQLite transaction. Rule/path display is escaped and bounded, so it cannot
introduce extra mentions.

`path_notification_epoch` is initialized once when the feature first opens a
state database. A PR created at or before that timestamp cannot receive automatic
CC until its complete ledger is restored. This intentionally applies to upgrades
as well as lost volumes: an empty database cannot prove that old comments never
existed. Restoring the original complete database preserves its epoch and budget.
If that is impossible, stop the server and use the evidence-backed command below
(arguments are installation ID, repository ID, then PR database ID, not the
PR's displayed number):

```sh
trigger-state /data restore-notification-ledger 67890 12345 98765 '["alice","bob"]' 'Complete ledger verified from backup and audit records, including deleted or uncertain comments'
```

Import is additive within the specified installation and records an audit entry.
The installation ID is required; the earlier ambiguous CLI syntax is rejected.
Include everyone ever notified or possibly notified in that scope; an empty list requires proof of no historical recipients.
The CLI cannot prove completeness for you. Visible comments alone are insufficient
when deletion or unknown requests are possible. If evidence is incomplete, leave
that PR blocked and investigate manually. No network call is made by the CLI.
New PRs created after the persisted epoch can use automatic CC normally. See
[issue 17 acceptance](issue-17-acceptance.md) for the full operational boundary.

Database version 4 migrates notification ownership in one transaction. Existing
recipient rows with a positive installation in their operation context retain
that installation, receipt state and reserved slots. Legacy operation keys/bodies
remain intact: the matching installation reuses their original markers during
reconciliation. New operations use installation-bearing keys. Legacy path plans
are adopted only when records for the matching snapshot or exact source event
identify one installation. Other events or manual commands on the PR cannot
establish ownership.

Unowned recipients, unscoped completeness proofs and ambiguous plans remain as
legacy evidence and block automatic CC for that repository/PR until the operator
restores a complete ledger for the intended installation. The proof only unblocks
that installation; it is never lent to another. `legacy_recipients` and
`legacy_path_notification_ledgers` preserve the old records for investigation.
A migration error rolls back the scope changes and version so startup can be
retried after the fault is fixed. Version-3 binaries cannot open version-4 state.

`record_wake` stores installation/repository/issue-or-PR number/GitHub user/source comment IDs and GitHub
source time. `session_before` selects a wake strictly before the calling comment,
using comment ID to order equal timestamps, with the caller's current TTL. The
same source comment cannot renew itself on edit. The Issue 14 consumer records
only a genuine explicit mention that passes applicability and authorization;
this module does not infer wake-ups or scan history.

## Inbox retention

Only successful inbox envelopes older than 30 days are eligible for cleanup, in
batches of at most 500. Envelopes linked to pending, running, unknown or failed
operations are retained, including links through the current lease owner. Cleanup
runs before pumping and periodically during sustained work. It never deletes
operation receipts, sessions, recipient deduplication or notification budgets.
The retention window is independent of session TTL and idle scheduler cleanup.

## Offline operations

Stop the server first. The admin command takes the same exclusive lock, so it
cannot modify records concurrently with an active process. It prints JSON and
never performs network writes.

```sh
trigger-state /data list
trigger-state /data inbox
trigger-state /data inbox LAST_DELIVERY_FROM_PREVIOUS_PAGE
trigger-state /data show SHA256_FROM_MARKER
trigger-state /data confirm-success SHA256_FROM_MARKER 'Verified App comment/review URL and marker'
trigger-state /data confirm-not-sent SHA256_FROM_MARKER 'Verified evidence that no request reached GitHub'
trigger-state /data retry SHA256_FROM_MARKER 'Definite rejection corrected; retry authorized'
```

`inbox` returns at most 100 rows in delivery-ID order. Use the last delivery ID
as the next cursor; an empty page ends the listing.

`retry` accepts a `failed` row only. An `unknown` row requires `confirm-success`
or `confirm-not-sent`; a missing marker alone does not justify the latter.
Every decision requires an evidence note and is saved in `admin_audit`.
A verified response receipt can be supplied as a final JSON argument. Assignment
success requires its actual assignee list so resumed handlers cannot falsely
report that GitHub ignored the assignment:

```sh
trigger-state /data confirm-success SHA256_FROM_MARKER 'Verified issue assignees via GitHub API' '{"assignees":[{"login":"alice"}]}'
```

Inspect child writes before the parent command. The relevant inbox is made pending
again; the server rechecks live policy and snapshots before resuming. Previously
successful suboperations are not sent again. No confirmation resets lifetime
notification deduplication or committed budget.

For the Compose deployment, the image includes both binaries:

```sh
docker compose stop xero-bot
docker compose run --rm --no-deps --entrypoint trigger-state xero-bot /data list
# Run the appropriate explicit decision with the same --entrypoint and volume.
docker compose up -d
```

Deleting the database is not a recovery procedure: it loses deduplication,
notification budgets, sessions and unknown-result evidence.
