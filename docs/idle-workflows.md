# Idle workflow scheduling

[English](idle-workflows.md) | [简体中文](idle-workflows-cn.md)

Each repository opts in through **`.github/xero-bot.toml` on its default branch**.
The bot waits for development inactivity and CI completion before dispatching
workflows, recovering missing runs and retrying failures. Build steps, image
tags and publishing remain in the existing GitHub Actions workflow.

## Enable

1. Grant the App **Contents: read**, **Pull requests: read**, and **Actions: write**
   on repositories it will schedule. Related repositories used only for monitoring
   need **Actions: read**. Approve updated installation permissions in GitHub.
2. Subscribe to **Push** and **Pull request** webhooks. **Merge group** is recommended
   for GitHub's native queue; xero-bot staging queues are covered by push events.
   Workflow run webhooks are not required.
3. Set `IDLE_WORKFLOWS_ENABLED=true` in the bot deployment and retain its
   `XERO_DATA_DIR` volume. `IDLE_WORKFLOWS_POLL_INTERVAL_SECS` defaults to `60`
   (minimum `15`). Docker Compose already persists `/data`.
4. Adapt [the example](../examples/idle-workflows.toml) and merge it as
   `.github/xero-bot.toml` on the default branch. The target workflow must be active
   and support `workflow_dispatch` on both the default and target branches.

Rules are refreshed each reconciliation. Missing config or `enabled = false`
disables scheduling. Invalid TOML, incorrect workflow inputs, missing workflows,
inaccessible monitors or insufficient permissions pause scheduling and produce
specific warnings in the logs. Changed rules start a fresh idle period.
`/cron` also runs reconciliation and returns an `idle_workflows` summary;
it shares the internal loop's writer lock.

Repository rules use TOML. GitHub workflow definitions remain YAML; the bot reads
them to validate `workflow_dispatch` and inputs, without modifying them.

## What to wait for and what to run

| Setting | Meaning |
| --- | --- |
| `idle_workflows.enabled` | Explicit opt-in; default `false`. |
| `idle_workflows.idle_minutes` | Development inactivity required; default `30`. |
| `monitors[].repository` | Related `owner/repository`; omitted means the current repository. The App must have access. |
| `monitors[].workflows` | CI to wait for: workflow filenames, `.github/workflows/...` paths, or quoted numeric IDs. Empty means all. |
| `tasks[].workflow` | Workflow to dispatch/retry: filename, workflow path, or quoted numeric ID. |
| `tasks[].branch` | Target branch; names containing `/` are supported. |
| `tasks[].inputs` | Declared dispatch inputs as TOML strings, numbers or booleans. |
| `tasks[].retry_interval_minutes` | Minimum delay after failure or a previous request; default `15`. |
| `tasks[].max_retries` | Retries beyond the initial attempt; default `2` (three attempts total). `0` disables retries. |
| `tasks[].run_events` | Trigger events representing equivalent task executions; default `["workflow_dispatch"]`, which must remain included. |

Omitting `monitors` waits for all Actions in the current repository. Related
repositories extend the scope; the current repository is always included. Add a
monitor without `repository` to narrow its CI selection. The filter covers all
executions of the selected workflows, including PR, push, staging, merge queue
and manual runs. Empty `workflows` means all CI. Both development activity and
selected CI in related repositories participate in the idle gate.

For AstrBot, a monitor could list `coverage_test.yml`, `dashboard_ci.yml`,
`unit_tests.yml`, `linux-development.yml` and other relevant development checks.
The task could name `publish-nightly.yml` with `branch = "master"`. These names
are configuration, not built-in assumptions. Its cron remains an independent
trigger: remove or adjust it in AstrBot if **all** automatic builds should obey
idle scheduling. Its scheduled mode selects yesterday's commit, so leave
`run_events = ["workflow_dispatch"]` for this workflow.

## Scheduling behavior

- Branch pushes (including force pushes), PR creation, code updates, reopening,
  merging and native merge-group creation reset the timer. Ordinary comments and
  reviews do not. An unchanged open PR does not prevent dispatch.
- Polling compares branch tips and open PR heads to recover missed state changes.
  It does not use comment-sensitive `updated_at` timestamps. A restart, changed
  configuration or interruption in activity observation starts a full idle
  window, since inactivity during an unobserved gap cannot be established.
- After the idle period, selected CI must have no queued, running, requested,
  pending or waiting runs. Waiting for approval also blocks. Finished failed CI
  counts as finished. Failed/incomplete API reads never imply idleness.
- Only the latest branch SHA is scheduled; intermediate updates coalesce. Success
  persists across restarts. Existing queued/running executions for the same
  workflow/branch/SHA prevent another. Identity uses the resolved workflow ID.
- Failures, timeouts and startup failures rerun the original run, preserving its
  SHA and inputs. Both idle and retry conditions apply. New branch tips stop new
  retries for old SHAs. Changed inputs apply to new dispatches; reruns retain their
  original inputs. Input changes alone do not rebuild a successful SHA.
- All cancellations suppress automatic retries for that SHA. A successful manual
  run can satisfy it later. Skipped, neutral and action-required outcomes need
  manual attention. Historical/manual runs in `run_events` and their attempts
  count toward the same SHA's attempt budget.
- `run_events` defines which completed runs are equivalent. A PR check or cron
  building a different commit must not mark an image build complete. Include
  other events only when they process `run.head_sha` with equivalent inputs.
  The workflow must make `success` mean the intended work actually completed.
- Background builds already started finish normally when development resumes.
  This version reduces contention at dispatch time; it does not preempt builds
  or reserve immediate capacity for future PRs.

## Persistence and uncertain requests

Intent and attempt counts are committed to `XERO_DATA_DIR/workflow-scheduler.sqlite`
before calling GitHub. The dispatch response's run ID is saved when available.
After a missing response, timeout or restart, the bot reconciles that ID or recent
bot-authored dispatches first. If the branch moved during dispatch, it credits
the SHA GitHub actually ran.

An unconfirmed request waits at least five minutes for Actions visibility.
After a successful history query finds no corresponding execution, it can be
attempted again through the ordinary idle, interval and attempt-limit gates.
Ambiguous requests reserve an attempt; definite HTTP rejections do not. GitHub
has no client idempotency key for dispatch: arbitrarily delayed visibility or a
simultaneous external dispatch cannot provide absolute exactly-once execution.
Workflows requiring it should also check whether the SHA's artifact already
exists before publishing.

Keep the SQLite file and WAL in the persistent volume. Deleting them loses
retry/cancellation/success records GitHub may no longer retain. One scheduler
process owns the database; a second process using the same volume fails to open
it. Independent copies of the volume are not coordinated. No external database
service is required.

## Validation

Tests use an explicit clock, temporary SQLite databases and a mock GitHub API.
They cover idle timing, workflow filters, related repositories, pagination,
failed reads, cancellation, retry limits, uncertain dispatches, changing branch
tips, config reloads and restart recovery, without triggering real workflows.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```
