# Trigger configuration and upgrade guide

[English](triggers.md) | [简体中文](triggers.zh-CN.md)

Policy comes only from the **target repository's default branch** at
`.github/xero-bot.toml`, never a fork or PR head. It coexists with `idle_workflows`
and is read even when the idle scheduler is off. The checked-in
[complete defaults](../.github/xero-bot.toml) enable no creation/path actions;
[opt-in examples](../examples/triggers-opt-in.toml) are separate and **do enable
those actions when copied**. The [annotated example.toml](../example.toml) covers all implemented configuration domains,
keeps automatic rules empty and idle scheduling disabled, and includes complete
CI monitors (current/related repositories), workflow tasks, branches, retry limits,
equivalent run events and typed dispatch inputs. Adapt these values before enabling
idle; creation/path opt-in blocks remain separate. Tests validate the reference both
as shipped and with idle enabled, including its inputs against a workflow definition.

## Manual modes and upgrade changes

| Mode | Meaning |
| --- | --- |
| `disabled` | Permanent veto on every entry point, alias, combination and automatic action referencing that command |
| `no_mention` | A complete bare command can run without a session |
| `mention_once` | Explicit bot mention, or an earlier valid session for this user/thread |
| `always_mention` | An explicit bot mention on every invocation, even with a session |

Defaults: `claim`, `unclaim`, `cc`, `r?`, `ready` use `no_mention`;
`r+` and `r-` use **`always_mention`**; `review`, `codeql`, `label`, `assign`,
`author`, `blocked`, `ping`, `help`, `queue` use `mention_once`. No command is
disabled by default. Missing, empty, idle-only and partial configurations merge
these defaults. `event_triggers` and `path_triggers.rules` default to empty.
`mode = "auto"` is invalid: automatic subscriptions are separate.

Aliases share policy: `take` → `claim`; `untake` / `release` /
`release-assignment` → `unclaim`; `?r` / `reviewer` → `ready`;
`relabel` → `label`; `commands` → `help`. `r= @user`, `r+ as @user` and
`r+ @user` are the same credited approval, behind `R_PLUS_ALLOW_ON_BEHALF=true`.
Configuring both an alias and its canonical name is invalid. Missing or extra
approval arguments never fall back to a plain approval.

**Compatibility changes:** bare words now require the entire comment to be a
complete command block. `claim`, `take; cc @alice`, and newline-separated complete
commands work. `claim 是什么意思？`, `cc @alice about this`, `review 一下` and
blocks mixed with prose do not. Use `review` or an explicit `@bot review 一下`.
Code, quotes and bot replies are not command sources. Explicit mentions and the
existing `r?`/`?r` shorthand keep their own positional rules. Every candidate is
gated before combination/deduplication: a permitted `ready` never authorizes a
blocked `cc` or approval next to it.

`review`, `codeql`, `r+`, `r-` are PR-only. On an Issue, `r? @alice` assigns Alice;
on a PR it requests review. Mention modes never grant repository rights.
`r+` **and `r-`** check current write/maintain/admin permission; credited approvals
also check the credited user. Self-approval is refused. Revoked or unknown
permissions cannot be replaced with old session evidence.

## Sessions and dynamic help

Post a new valid `@bot help` (using the deployment's bot name) after upgrading:
**old comments are not imported**. A genuine explicit mention must pass syntax,
applicability and authorization checks before it records a session. Explicit
commands in `always_mention` mode do not open reusable sessions. Bare commands,
automatic events, bot messages and failed checks do not open or renew sessions.

Sessions are scoped to installation + repository + Issue/PR thread + GitHub user.
`[command_sessions] ttl_days = 30` is the default (1–365 days), measured from the
GitHub source comment time. Only valid explicit mentions renew it. Edits do not
wake, deletion does not revoke recorded evidence, and closing/reopening does not
refresh expiry. Persistence survives a restart. TTL is checked against both source
order and execution time; a later mention cannot authorize an older comment or a
bare command in the same comment. Expired sessions require a fresh mention.

Help shows all 16 effective modes, aliases, PR restrictions, current session scope,
remaining seconds and UTC expiry (or inactive/unavailable), plus separate creation
and path rule summaries. Rule counts are configured counts, **not remaining
notification slots or successful deliveries**. CC recipients are not mentioned
by help. Displayed paths/IDs are escaped and bounded; consult the configuration
file for the full lists. A direct handler without a verified snapshot only offers
a defaults reference, not effective repository policy.

## Cache and failure handling

Verified snapshots are cached for **60 seconds**, keyed by installation/repository.
Concurrent refreshes for one repository are coalesced. A verified default-branch
Push or observed default-branch change invalidates the snapshot immediately;
missed events are recovered at expiry. Configuration is fetched at an immutable
commit, not from the PR. Only a file 404 after successful repository/default-ref
reads means absent; 403/429/5xx, transport or malformed responses are failures.

After expiry/invalidation, a failed refresh blocks related actions; an older
snapshot is **reference only**, never executable. Unknown fields/types or invalid
TOML reject the whole document. Semantic errors disable the affected domain or
rule; unaffected domains remain usable. Automatic rules referencing disabled
commands are invalid, including path `labels`/`cc`; a mixed rule with either
command disabled is rejected as a whole. Other manual mention modes do not grant
or remove automatic subscriptions. Repairs do not backfill refused/frozen events.

Explicit `@bot help` / `@bot ping` can give a minimal diagnosis: stable reason and
repair location on the default branch, without TOML values, workflow inputs,
credentials or private API error bodies. Bundled actions do not execute on a
configuration outage. Diagnostics are limited to one per installation/repository/
thread/reason per **10 minutes within the process**; durable write receipts also
prevent replaying the same comment across restart. Restart resets the in-memory
rate window for new source comments. Automatic failures appear in logs and the
state CLI, not repeated notification comments. A storage ingress failure returns
HTTP 503 without running actions; the server must have writable persistent storage.

## Automatic creation and path actions

Creation subscriptions accept `issues.opened` and `pull_request.opened`, including
draft PRs. Only PR `review`/`codeql` and Issue/PR static `label.add` are supported.
No automatic approvals, assignments or generic command evaluator exist. AI review
publishes **COMMENT only**, even when the model recommends approval; it cannot
APPROVE or enter the merge queue. Labels must already exist. Reserved queued/testing
labels and nonempty `CODEQL_LABEL` are forbidden even when those features are off.

Path rules run on PR `opened`/`synchronize` only. Ordinary Issues have no diff and
never inherit paths from prose or a linked PR. Matching uses the **complete current
base/head diff**, not just the last push. Repository-root `/` paths are case-sensitive:
`*`/`?` match within a segment, and standalone `**` matches zero or more segments.
Any include plus no exclude matches a path; either name of a rename can match
(an excluded name does not exclude the other name). Deletions use the old filename.
Generated and binary files participate unless explicitly excluded. Absolute paths,
backslashes, `.`/`..`, braces, character classes and negation are unsupported.

The API file list is paginated and checked against `changed_files`, unique names,
statuses and stable base/head. Missing/truncated/malformed lists, more than GitHub's
**3000-file limit**, or an unstable snapshot produce **no partial path actions**.
Matching unions existing labels; it does not remove or create labels. A mixed
label/CC rule with a missing configured label is skipped as a whole. Label and
notification API results are recorded independently.

CC is an explicit personal-login list (max 32 per rule, no `@`, teams or inferred
owners). Selected accounts are verified; the App itself is excluded. Case-normalized
logins are deduplicated across all rules and selected in lexical order. One aggregate
comment mentions only newly reserved recipients. Excess names are **only counted**
in the public body; fully suppressed plans post nothing. IDs/path samples are bounded
and escaped, including `@`, to avoid extra mentions.

`max_cc_users_per_pr` is a **lifetime** budget of 0–10 distinct people (default 10)
per installation/repository/PR, shared across rules and pushes; 0 disables CC only.
Manual CC is separate. Duplicate pushes, configuration/rule changes, deleted comments,
closing/reopening and restarts do not reset it. Lowering the cap blocks new users
when existing usage exhausts it. SQLite atomically reserves recipients and slots;
unknown sends keep slots. Confirmed no-send can release unsent reservations.
A missing/non-personal/renamed account fails the whole unsent aggregate; fix the
configuration for a later supported snapshot. Transport failures before sending
remain retryable. Acceptance of a comment does not prove notification delivery
under each person's GitHub settings.

## Deployment and recovery

Subscribe **Issue comment** for `created` commands; **Issues** for opened Issue
rules; **Pull request** for opened/synchronize paths and creation rules plus existing
rebase/label/close handling; **Push** for prompt default-branch configuration
invalidation and rebase/queue updates. Add **Pull request review** for native-review
merge queue integration. Existing rebase, CodeQL-label, merge queue and idle
features keep their own switches and routes; this feature adds no duplicate route.
App-created promotion PRs are filtered by verified App provenance.

| Enabled feature | GitHub App repository permissions |
| --- | --- |
| Read default-branch policy and code | Contents: read; Metadata: read (implicit) |
| Issue commands / labels / assignments / CC | Issues: write |
| PR review requests, review/report publication, path PR reads and rebase checks | Pull requests: write; Issues: write for labels/comments |
| CodeQL reports | Code scanning alerts: read, plus report publication permissions |
| Merge queue | Contents: write, Pull requests: write, Issues: write, Checks: read; keep existing branch protection/bypass policy |
| Idle workflow dispatch | Actions: write, Contents: read, Pull requests: read; monitored external repositories need Actions: read |

Grant permissions for the enabled features and approve changes for installations.
No new organization-member, team or CODEOWNERS permissions are required.

Use a persistent `XERO_DATA_DIR` (Compose mounts `/data`) and **one process**. An
exclusive database lock rejects a second owner; separate volumes do not coordinate.
Stop the server before copying the database **and WAL** / persistent volume. Restore
the complete backup, not just a partial recipient table. Data loss loses sessions,
receipts, deduplication and unknown-write evidence; deleting SQLite is not recovery.
The first feature startup records a ledger epoch. PRs created at/before it block
automatic CC until a complete ledger is restored; labels can continue. This covers
both first upgrade and lost/recreated volumes. Import evidence with
`restore-notification-ledger` only after a complete audit, including possibly sent
or deleted comments. Visible comments alone are insufficient.

Unknown non-idempotent writes are reconciled by marker, verified App identity and
original body; they are never blindly resent. Stop the server, then use:

```sh
trigger-state /data list
trigger-state /data inbox
trigger-state /data show SHA256_FROM_MARKER
trigger-state /data confirm-success SHA256_FROM_MARKER 'Verified remote result and marker'
trigger-state /data confirm-not-sent SHA256_FROM_MARKER 'Evidence no request reached GitHub'
trigger-state /data retry SHA256_FROM_MARKER 'Definite failed request corrected'
```

`retry` accepts only `failed`; unknown requires evidence-backed confirmation.
Inspect child writes before the parent. Decisions are audited and never reset the
lifetime budget. Label ensures can reconcile and retry idempotently under current
policy. Policy changes do not cancel already-running computations; emergency stops
require stopping the service and inspecting uncertain writes. **GitHub and SQLite
do not share a transaction; this is not exactly-once delivery.** See the full
[operational procedure](trigger-state.md) and [#18 acceptance matrix](issue-18-acceptance.md).
