# Repository configuration contract (#11)

The bot reads `.github/xero-bot.toml` from the **target repository's default branch**.
It does not read policy from a PR head or fork. This reader is independent of
`IDLE_WORKFLOWS_ENABLED`; that environment switch still controls the idle scheduler.
See the [complete defaults](../examples/repository-config.toml),
[existing idle configuration](idle-workflows.md), and
[Chinese acceptance record](issue-11-acceptance.md).

## Delivery scope

This change provides a shared configuration contract and reader, plus configuration
failure and `disabled` vetoes in existing comment/CodeQL-label entry points. It does
not add automatic actions. Strict bare-command parsing and complete mention-mode
routing/session lifecycle are implemented by #12–#14. Until then, enabled commands
continue using the existing parser and history-based session handling: the new
`ttl_days` field is validated but does not yet expire those legacy sessions.
The contract's mention defaults are not a claim that the #14 routing migration has shipped.

An explicit `@bot r= @user` normalizes to an on-behalf approval, with the same
execution permissions and deployment switch as `r+ as @user`. Its target is mandatory;
missing, invalid or extra parameters never degrade to a plain approval. Bare `r=`
and the broader approval grammar/source migration remain part of #12.

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

Event rules only declare PR-open `review`/`codeql` or static `label.add` on Issue/PR
open; references to disabled commands are invalid. All declared event/path rules
currently return `Unsupported` until #15–#17 implement them. Failures are available
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
