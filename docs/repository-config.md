# Repository configuration contract (#11)

The bot reads `.github/xero-bot.toml` from the **target repository's default branch**.
It does not read policy from a PR head or fork. This reader is independent of
`IDLE_WORKFLOWS_ENABLED`; that environment switch still controls the idle scheduler.
See the [complete defaults](../examples/repository-config.toml),
[existing idle configuration](idle-workflows.md), and
[Chinese acceptance record](issue-11-acceptance.md).

## Delivery scope

The shared configuration reader provides configuration failure and `disabled`
vetoes in comment/CodeQL-label entry points. #12 adds strict command-block parsing,
source-bearing candidates and per-candidate mention gates before deduplication or
status resolution. It does not add automatic actions or persistent sessions.
Session evidence uses the durable wake ledger for the same user's explicit, enabled
commands. `ttl_days`, source ordering and authorization preflight are enforced at runtime by #13/#14.
See the [#12 acceptance record](issue-12-acceptance.md).

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
execution, regardless of its alias or existing session. Existing historical commands
that are currently disabled cannot qualify as session openers. Permission checks
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
still return `Unsupported` until #16–#17 implement them. Failures are available
through `RepositoryConfig::problems()`, logged on load and shown by help/ping.
Path `labels`/`cc` are independent static actions, not aliases for comment commands.
The path CC budget accepts 0–10; only PR opened/synchronize event names are accepted.

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
10 minutes per repository/thread/reason within the process. **Durable suppression,
retry inboxes and restart-safe delivery are #13**, not guarantees of this in-memory
adapter. Automatic failures are logged rather than generating per-event comments.
