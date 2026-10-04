# Repository configuration contract

The bot reads `.github/xero-bot.toml` from the **target repository's default branch**.
It does not read policy from a PR head or fork. This reader is independent of
`IDLE_WORKFLOWS_ENABLED`; that environment switch still controls the idle scheduler.
See the [annotated full example](../example.toml), [complete defaults](../examples/repository-config.toml),
[existing idle configuration](idle-workflows.md), and
[Chinese acceptance record](issue-11-acceptance.md).

## Delivery scope

The comment, creation-event and path consumers share verified default-branch
configuration, disabled vetoes and durable state. Strict parsing gates each
candidate before deduplication; source-ordered sessions have a configurable TTL.
Dynamic help uses the effective snapshot and persisted session evidence. See the
[English trigger/upgrade guide](triggers.md), [中文指南](triggers.zh-CN.md),
and [cross-feature acceptance](issue-18-acceptance.md).

`r= @user`, `r+ as @user` and `r+ @user` normalize to the same on-behalf approval,
with unchanged execution permissions and deployment switch. Missing, invalid or
extra parameters invalidate the candidate; they never degrade to a plain approval.
Sentence punctuation after the target (for example `@bot r= @alice。`) remains
accepted. Bare approval syntax is parsed in a complete command block but its
execution still requires a mention by default, even with existing session evidence.

## Configuration and validation

There are 16 canonical command IDs. TOML accepts known aliases (`take`, `untake`,
`release`, `release-assignment`, `?r`, `reviewer`, `r=`, `relabel`, `commands`) and
normalizes them before applying policy. Configuring both `take` and `claim` is an
error, even if the values match. Modes are `disabled`, `no_mention`, `mention_once`,
and `always_mention`; `auto` is rejected.

| Contract default | Canonical IDs |
| --- | --- |
| `no_mention` | `claim`, `unclaim`, `cc`, `r?`, `ready` |
| `always_mention` | `r+`, `r-` |
| `mention_once` | `review`, `codeql`, `label`, `assign`, `author`, `blocked`, `ping`, `help`, `queue` |

`command_sessions.ttl_days` defaults to 30 and accepts 1–365. No command defaults
to `disabled`. A disabled command is rejected before session lookup or handler
execution, regardless of its alias or existing session. Old comments are not imported as session openers; only valid explicit interactions
recorded by the durable consumer count. Permission checks
in command handlers remain required; the configuration contract grants no authority.

Syntax, type, missing required fields and unknown fields reject the whole document.
After deserialization, semantic failures are retained separately for comments,
events, paths and idle scheduling. One invalid event/path rule disables that rule;
duplicate IDs or invalid shared path settings disable their domain. An invalid domain
never silently receives defaults. Idle fields keep their previous strict validation.

Event rules execute PR-open `review`/`codeql` or static `label.add` on Issue/PR
open (including drafts); references to disabled commands are invalid. Up to 32
rules with nonempty unique IDs are allowed. Identical canonical actions/parameters
are coalesced, preserving all matched IDs. Labels must already exist; the deployed
queue control labels and nonempty `CODEQL_LABEL` are rejected before planning.
The veto uses the same Unicode lowercase normalization as label matching.
For opened events, a verified invalid TOML document/events domain records a
permanent refusal for that thread; later policy repairs cannot backfill it.
Transient configuration reads retain retryable inbox work.
See [opened-event behavior and acceptance](issue-15-acceptance.md). Path rules
run only for PR `opened` and `synchronize` events. They match the complete
base/head file list with a case-sensitive repository-root glob subset (`*`, `?`,
and a standalone `**` path segment), including both names of a rename and
excluding paths matched by `exclude`. Labels are coalesced across matching
rules, checked against the repository label inventory, and added without ever
creating, removing, or touching merge-queue/CodeQL control labels. Missing,
truncated, malformed or changing file lists fail closed and produce no label
write or notification. `cc` accepts up to 32 explicit personal logins per rule,
without `@` or team syntax; logins are validated, lowercased and deduplicated.
The App itself is excluded, and selected accounts are verified as GitHub users.
A rule combining labels and CC must have all its labels present before either
side effect. Label API failures and comment results are recorded independently.
A disabled `label` (including `relabel`) or `cc` invalidates path rules that
reference it; either veto rejects a mixed rule. Invalid comment policy also blocks
these dependent rules. Other manual mention modes do not change automatic
subscriptions. Failures are available through `RepositoryConfig::problems()`,
dynamic help and logs.

`max_cc_users_per_pr` defaults to 10 and accepts 0–10. Zero disables path CC
while labels remain active. Within each installation/repository/PR, all heads and
rules share a lifetime ledger keyed by the normalized login. Different
installations have independent budgets. The ledger excludes already sent or
uncertain recipients, then reserves remaining slots in login order. One aggregate
comment contains only newly reserved mentions, safe
rule/path examples, the checked head SHA, and a count of suppressed users.
Fully suppressed plans write no comment. Manual CC has a separate lifecycle.

Changing/deleting/recreating rules, changing the limit, closing/reopening the PR,
deleting a notification comment, and restarting the process never reset slots.
A lower limit blocks new recipients when existing usage meets or exceeds it.
Unknown comments pause for App identity + stable marker + original body
reconciliation. Missing comments do not prove no send. Initial deployment of
this feature and a lost/recreated database both conservatively block CC on PRs
created at or before the durable ledger epoch; labels continue. Restore a backup
or attest a complete ledger through the offline CLI before enabling those PRs.
See [notification acceptance and recovery](issue-17-acceptance.md).

## Snapshots, failures and diagnostics

The process-wide cache key is `(installation ID, repository ID)`. Revalidation reads
repository metadata, the default branch ref, then file contents at that immutable
commit SHA. Snapshots record the default branch, commit SHA, optional config blob SHA
and verification time. Only a file-path 404 after successful repository/ref reads
means “no config”; repository/ref 404, 403, rate limits, timeouts, malformed responses,
directories, unsupported encodings and invalid UTF-8 all block execution.
Missing/empty files, idle-only files and partial overrides merge built-in defaults.

Snapshots are usable for 60 seconds. Each repository has a single refresh in flight;
other repositories can refresh independently. A verified default-branch push or an
observed default-branch change invalidates immediately, including a refresh already
in flight. ETag/304 reuse is limited to the same repository, branch and immutable
commit URL. Missed events are recovered on expiry.

After expiry/invalidation a failed refresh blocks actions. The previous snapshot is
available only as an explicitly expired reference. Retry backoff is 15 seconds, or
longer when GitHub supplies Retry-After/rate-limit reset. Invalidation never erases a
rate-limit backoff. Ordinary chat causes no config request. Explicit help/ping can
return a short English/Chinese failure diagnosis without executing bundled commands.

Reason codes are typed; diagnostics never interpolate TOML source, workflow inputs
or remote error bodies. Existing entry points suppress duplicate diagnostics for
10 minutes per installation/repository/thread/reason within the process. Durable
write receipts prevent replaying the same diagnostic source comment across restart;
the in-memory rate window resets for new comments. The inbox retains retryable
work, and uncertain writes require reconciliation. Automatic failures are logged
rather than generating per-event comments. See [state operations](trigger-state.md).
