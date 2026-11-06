# Rollup queue

[中文](rollup.zh-CN.md)

Each GitHub repository is one project with its own queue, configuration, candidate
branches and build history. Its bound Forgejo repository submits to that same
queue. GitHub is the only writer that advances the integration branch; the bridge
then copies refs to Forgejo.

The website is deliberately small: `/` lists public projects,
`/queue/OWNER/REPO` shows one project's queue and builds, and `/commands` generates
the command reference from the running bot's command registry. Pages default to
English; the English/中文 links preserve the project (`?lang=en` / `?lang=zh`).
The queue refreshes every ten seconds. There are no web mutation endpoints.
Private repositories never appear in the public API, even if `public=true`.
A project appears after its first queue command, including `queue`.
Commands on non-public projects report status without advertising a public queue link.

## Commands

Mention the bot on a PR on either provider:

| Command | Effect |
| --- | --- |
| `r+` | Approve the current source head; test alone unless rollup was enabled for this entry. |
| `r+ rollup` | Approve the current head and allow batching. |
| `rollup` / `rollup=never` | Change batching for an approved, non-running entry. |
| `r-` | Withdraw approval and cancel its active candidate. |
| `try` | Test the current head alone, without merging. |
| `retry` | Retry an unchanged, still-approved failed revision. |
| `queue` | Show status and link to this project's page. |

Approvals require live write permission and cannot approve the author's own PR.
Forgejo identities must be explicitly mapped on both repositories. A mirrored
comment, label, AI review or web Approve does not enqueue a PR. A new source
commit invalidates approval; a delayed mirrored head waits for synchronization.
Requested changes from a writer after approval require another `r+`.

## Candidate and CI

After the collection window, the bot freezes members and approved heads, merges
them locally onto the latest default branch and pushes one immutable
`automation/xero/rollup/N` (or `automation/xero/try/N`) branch. An audit PR records
the approval manifest and candidate SHA. Later approvals wait for another batch.
Closing the audit PR cancels that batch's approvals.

Generated merge commits use the authenticated GitHub App bot's user ID and
canonical noreply email, not a configured mention alias. Register that email as
a verified additional email on the Forgejo bot to attribute the same SHA on both
providers. Source commit authors and existing history remain unchanged.

GitHub Actions must pass every configured job on the candidate's **push** run.
If matrix jobs or separate workflows share a configured job name, every matching
job in the latest workflow executions must pass; one success cannot hide a failure.
If GitHub fails, its API is unavailable, or its pending time exceeds
`fallback_after_seconds`, the bot pushes the **same commit** to Forgejo. Every job
of every configured Forgejo workflow must succeed. Either complete provider is
enough; partial successes from different providers are never combined. Skipped,
neutral, cancelled, missing and wrong-SHA results cannot authorize merging.

Before merging, the bot rechecks source heads, permissions and the target branch.
It reports `xero/rollup` success and fast-forwards the default branch to the tested
commit without force. If the base moved, it rebuilds and retests. If both providers
fail, a batch splits into two smaller groups; each is retested on the then-current
base. A singleton failure waits for a fix/reapproval or `retry`. API failures back
off, and a timed-out run waits for `retry`. Finished candidate refs are cleaned up;
audit PRs and the durable build history remain. Once the bridge has copied the tested
commit into Forgejo’s target branch, the bot records the paired PRs as manually
merged through Forgejo’s native API, preserving the same commit.

## Enable a project

1. Deploy this version, preserve `XERO_DATA_DIR`, and set
   `PUBLIC_URL=https://bot.example.com`. Rollup service support is enabled by
   default. Each project controls activation through `[rollup].enabled` in its
   `.github/xero-bot.toml`; an omitted or false value keeps the legacy queue.
   `ROLLUP_ENABLED=false` disables Rollup service support across the deployment;
   remove an old false override when migrating an existing deployment.
2. Configure the ordinary-repository bridge on **both** default branches with
   pinned repository IDs and `sync_refs=true`. Drain the legacy queue before
   switching; an existing testing label blocks the new queue.
3. Adapt [examples/rollup.toml](../examples/rollup.toml) on both repositories.
   Reviewer mappings and Forgejo workflow names must agree. Names and limits are
   per project; never reuse another project's repository IDs.
4. Allow Actions `push` events on `automation/xero/**` in both CI files. The App
   needs contents, pull requests and commit statuses write, and Actions read;
   Forgejo's bot needs repository admin permission to query other reviewers' live
   permissions (Forgejo 17) and Actions access. Enable Forgejo's “Allow marking
   pull requests as manually merged” repository setting (`allow_manual_merge`)
   so the bot can record PR completion after the tested commit is synchronized.
5. Align branch protection with the policy: require the App's `xero/rollup` gate.
   Keeping GitHub-only CI as an additional required gate will also require it to
   pass, so it prevents the requested Forgejo fallback merge. The bot does not
   bypass or edit branch protection. Keep unrelated protection rules intact.
6. Use `@bot queue` to register the project, then `@bot try` and `@bot r+ rollup`
   on disposable PRs. Verify the same SHA on both sides and the exact tested SHA
   on the default branch.

Snapshots, approvals, fallback progress and retry deadlines live in the existing
SQLite store under `XERO_DATA_DIR`; a restart does not reapprove a new head. Do not
start two bot instances against the same state directory. Disable the project
policy and drain active candidates before rolling back to a binary that does not
understand `[rollup]`, then remove the new configuration table.
