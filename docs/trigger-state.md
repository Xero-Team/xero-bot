# Durable trigger state

The server always opens `XERO_DATA_DIR/command-triggers.sqlite`, even with
`IDLE_WORKFLOWS_ENABLED=false`. Use one deployment process and a persistent local
volume. A second owner of the same database fails at startup. Different volumes
are **not coordinated**; active replicas are unsupported. Docker Compose already
mounts `/data`. Keep the database and its WAL together; stop the server before a
filesystem backup or offline administration. Version 1 databases migrate transactionally
to version 2, retaining intents, receipts, sessions, and recipient reservations.

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
whitelist from `event_triggers`; path envelopes remain inputs only for #16–#17.
Rebase, CodeQL label,
native review/merge queue, and idle workflow routing keep their existing switches
and are not dispatched a second time by this worker.

The business key for a manual command is its repository ID, comment ID and
canonical command/arguments, independent of delivery ID. Aliases and normalized
login lists share a key. Opened actions have a repository/thread/stable rule ID
key; notification recipients have a repository/PR/lowercase-login key. Configuration
and head SHAs are audit/snapshot data, never part of a lifetime recipient key.

The first successfully loaded opened-event plan is frozen in `opened_plans`,
including an empty plan. It survives inbox cleanup and prevents configuration
changes or redelivery from backfilling old threads. Equivalent actions share an
operation keyed by the first sorted stable rule ID, with all matched IDs retained
in its intent. Before each action/recovery, current default-branch policy must
still contain at least one original rule with the same canonical action and
parameters. Removed/changed/disabled plans are superseded. Independent actions
can finish even when a sibling is paused.

Automatic review/report computation is marked started before invoking its engine.
After interruption it can adopt a confirmed result receipt, but never recomputes
AI output automatically. A started computation with no confirmable output stays
unknown for operator inspection. Courtesy progress comments are omitted for these
actions so they cannot be mistaken for completed reports. Labels remain an
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
