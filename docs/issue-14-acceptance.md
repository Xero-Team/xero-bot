# Issue 14 acceptance

Issue 14 connects the parser and the durable trigger store to the repository
comment policy from Issue 10.

- Each parsed candidate is gated independently by `disabled`, `no_mention`,
  `mention_once`, or `always_mention`. Disabled aliases stop before session
  lookup or command actions; PR-only commands reach the handler on an Issue so
  the user receives an explicit explanation.
- Production webhook deliveries use the source comment ID, GitHub user ID,
  issue/PR ID, installation ID, and GitHub `created_at` timestamp. A genuine
  explicit bot mention for a session-capable mode records a wake only after applicability
  and authorization preflight. Bare commands and always-mention approvals never open
  or renew a session.
- Session lookup is a durable SQLite query scoped to installation, repository,
  thread, and user. It uses the current `ttl_days`, source ordering
  (`created_at` plus comment ID), and the wall clock at execution time. It does
  not scan historical comments, and old session rows without installation
  scope are inert after migration so an upgrade requires a fresh @.
- `always_mention` approvals remain explicit-only authority and do not become a
  session grant; `r+`, `r=`, and `r-` are rechecked by their handlers immediately
  before the privileged review or dismissal write.
- `r+`, `r=`, and `r+ as` retain the existing write/maintain/admin, self-
  approval, credited-user, and feature-switch checks. `r-` now performs the
  same write-level preflight and refuses when GitHub cannot confirm permission;
  no dismissal or queue dequeue is attempted on that path.

Validation:

```text
cargo test --locked --all-targets
```

The test suite covers mode priority, disabled aliases, durable session scope,
source ordering and expiry, restart persistence, configuration failures,
approval authorization, and the `r-` unknown-permission refusal path.
