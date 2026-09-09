# xero-bot

[English](README.md) | [简体中文](README.zh-CN.md)

Org-wide GitHub App bot for the Xero-Team. Written in Rust — a single self-hosted binary (Docker / VPS).

Features:
- **triagebot-style comment commands** — `r?`, `?r cc`, label management, assign/claim, `r+` approval on behalf, and more
- **Incremental AI code review** — learns the project first and builds on the previous review round, instead of looking at the diff in isolation
- **Rebase reminders** — when a PR conflicts with its target branch, adds the `needs-rebase` label and a reminder; clears it once resolved
- **CodeQL quality reports** — reads the repo's existing code scanning alerts and maps them to files changed in the PR
- **Bilingual replies** — answers in English or Chinese, chosen from the PR's own commit messages; no configuration

## Command reference

Issued in comments (case-insensitive; one comment may contain several commands; content inside code blocks is ignored).

Most commands work in issues as well as pull requests — GitHub serves labels, assignees and
comments from the same API for both. The four that need a PR are `review`, `codeql`, `r+` and
`r-`; used in an issue they say so rather than failing silently. In an issue `r? @user` is an
assignment, since an issue has no reviewers.

### Mention-free sessions

Once a user has run **one** command with a mention (`@xero-review help`) on a PR or issue,
their session on it is open: their later comments can use the mention-free forms below without
`@`. Only an explicit command is recognized — a comment has to *open* with the verb, and prose
is never parsed. The verbs that work without a mention are the unambiguous ones: `review`,
`codeql`, `ready`, `author`, `blocked`, `ping`, `help`, and bare `r+` / `r-`. Argument-taking
verbs (`claim`, `label`, `cc`, `assign`) and the combined forms still need the mention; bare
`r? @user` and `?r` never needed one. A mention-less command from someone who has no session
gets one line of explanation instead of silence.

| Command | Description |
|---|---|
| `@xero-review review` | AI code review (incremental: builds on the previous bot review and newer commits) |
| `@xero-review codeql` | CodeQL quality report |
| `@xero-review ping` | Health check |
| `@xero-review help` | Command help |
| `r? @user` | Request review from @user (auto-assigns; `r? user` without the @ also works; can appear anywhere in the comment) |
| `@xero-review cc @u1 @u2` | CC / notify users |
| `?r` or `@xero-review ready` | Mark as waiting for review (adds `waiting-on-review`, removes the other two status labels) |
| `?r cc @user` | ready + cc combo (triagebot shorthand style) |
| `@xero-review author` | Mark as waiting on author (`waiting-on-author`) |
| `@xero-review blocked` | Mark as blocked (`blocked`) |
| `@xero-review label +bug -wip` | Add/remove labels |
| `@xero-review assign @user` | Assign to @user |
| `@xero-review claim` / `unclaim` (aliases: `take` / `untake`) | Claim/release (assign to self / remove self) |
| `@xero-review r+` | Approve on behalf: the bot checks the commenter has write access and did not author the PR, then submits an APPROVE review in their name |
| `@xero-review r+ as @user` | Approve in @user's name (bors' `r=`, for relaying an approval given elsewhere). **Refused unless `R_PLUS_ALLOW_ON_BEHALF=true`** — see [Approvals](#approvals) |
| `@xero-review r-` | Withdraw a previous bot APPROVE (dismiss) |
| `@xero-review queue` | Show the merge queue: batch under test + waiting PRs |

Automatic behavior (no command needed):
- After a PR push/reopen, checks for conflicts → adds `needs-rebase` + a reminder comment; once resolved → removes the label
- Periodic sweep (built-in loop, default 6h) as a fallback check
- Adding the `CODEQL_LABEL` label to a PR (if configured) → auto-generates a CodeQL report

**`?r` notifies the reviewer.** A label alone reaches nobody, so on a PR `?r`/`ready` also
pings the reviewer: whoever is currently listed under Reviewers (whether the bot's `r?` put
them there or they were added by hand in the GitHub UI), or — when nobody is listed — whoever
left the most recent CHANGES_REQUESTED/APPROVED review. With neither, the reply asks for
`?r @user` rather than guessing.

### Approvals

An APPROVE review submitted by the App is a real approval: a branch-protection rule that
requires one counts it, so `r+` is a privileged write and not a comment. Three rules apply.

- **The commenter needs write access or above**, checked against the repo before anything is
  posted.
- **The PR author can never approve their own PR**, not directly and not via `r+ as @someone`.
  GitHub enforces this for human reviews, but the review author here is the App, so the bot
  has to enforce it itself.
- **Relaying an approval to another login is off by default.** With
  `R_PLUS_ALLOW_ON_BEHALF=true`, `r+ as @user` credits the approval to @user — who must also
  have write access. Left enabled, anyone with write access can manufacture an approval in a
  colleague's name and satisfy a required-review rule without that colleague ever seeing the
  PR, which is why it ships off. Plain `r+` is unaffected either way.

A refused `r+` costs no API call beyond the checks above, and the `help` table says which side
of the switch the deployment is on.

### Merge queue

With `MERGE_QUEUE_ENABLED=true`, an approval stops meaning "this looks good" and starts meaning
**"merge it"** — the semantics an automerge queue gives `r+`. The queue tests a *batch* of PRs
together, so main only ever advances to combinations that have actually passed CI:

1. A successful `r+` (or a web Approve by a write+ reviewer) adds the `merge queue: queued` label.
   `r-`, a CHANGES_REQUESTED review, or closing the PR takes it out again.
2. The driver (a poll loop, default every 30s) builds a batch — up to `MERGE_QUEUE_MAX_BATCH` PRs,
   ascending number order — by merging each PR's head into the `staging` branch as a merge
   commit (`xero-bot: merge #n (head …)`). The batch is then labeled `merge queue: testing`.
3. CI runs on the staging pushes. Green → main is advanced via a `staging`→`main` PR (which
   inherits main's branch protection, so required checks are already satisfied by the tested
   tree). Red → the newest member is removed as the likely culprit, staging resets, and the
   remaining prefix re-tests automatically (tail-dropping is bisecting by construction).
4. On success every member gets a 🎉 comment and the staging branch is deleted (recreated
   next batch).

`@xero-review queue` shows the batch under test with its CI state, plus the waiting list.

**Prerequisites** (the queue fails open-ish: a batch that never gets a CI verdict times out
after `MERGE_QUEUE_CI_TIMEOUT_SECS` — default 2h — and returns its PRs to the queue with an
explanation):

- **CI must run on staging pushes.** A workflow with only `on: pull_request` never fires on
  the `staging` branch — this is the single most common misconfiguration:
  ```yaml
  on:
    push:
      branches: [main, staging]
  ```
- **GitHub App settings**: add permission **Contents: read/write** (the queue creates,
  resets and deletes the staging branch and creates the advance PR) and subscribe to the
  **Pull request review** event (a web Approve must reach the bot). Also subscribe to the
  **Push** event if you can: it is what notices "someone else's PR merged and dirtied an
  open PR" within seconds instead of at the next sweep. Everything else stays as before.
- **Branch protection**: leave `staging` unprotected — the bot force-updates it constantly.
  Keep `main` protected as today; the advance PR satisfies required checks on its own
  (its head is the tested tree). If main also requires human reviews, a write+ user
  approving the advance PR approves the whole batch — the bot says so and retries.
- **Only PRs targeting the repo's default branch** are accepted (`MERGE_QUEUE_ADVANCE_METHOD=pr`
  default; `ref` does a bare fast-forward and needs the App exempted from push
  restrictions — advanced setups only).

The queue keeps all state in GitHub — labels plus the staging merge-commit chain — so a
restart mid-batch resumes exactly where it left off, with no database.

### Reply language

The bot answers in English or Chinese — including the prose of an AI review — and picks
which from the PR's own commit subjects: mostly English gets English, mostly Chinese gets
Chinese. Each commit casts one vote, so a single long message can't decide for the rest, and
only the subject line is read, so English trailers (`Signed-off-by`, `Co-authored-by`) don't
skew a Chinese PR. When the commits say nothing either way (`bump deps`, `v2 -> v3`) the
triggering comment is consulted, and failing that the reply is English. There is nothing to
configure, and no other languages are modelled — Japanese written in kanji is indistinguishable
from Chinese here and will be answered in Chinese.

## AI review engine

Selected via `REVIEW_ENGINE`:

| Engine | Mechanism | Incremental capability |
|---|---|---|
| `agent` (default) | tool-calling loop, tools = GitHub API (list/read/search code); explores the project before reviewing | Injects the previous bot review on this PR + the list of newer commits |
| `builtin` | single HTTP call (OpenAI chat/responses/Anthropic formats) | Same (context injection) |
| `pi` | subprocess `pi -p --session-dir`, read-only toolset | **Session continuity**: per-repo session files remember project understanding |
| `codex` | subprocess `codex exec --sandbox read-only -o` | Same (`codex exec resume`) |
| `auto` | probes in order: pi → codex → agent → builtin | - |

`agent` automatically falls back to `builtin` on timeout/failure. All engines share the same publishing pipeline: risk-tiered summary table + inline comments on added lines + a publishing fallback chain (with inline → without inline → plain comment).

### Finding IDs, evidence, and the adversarial re-check

Every published finding carries a stable ID (`XRV-…`), a type, and a description that must
cite the code it is about. The ID is what makes a finding addressable across rounds: when a
PR is reviewed again, the prompt requires the model to audit the previous round's findings
one by one and state, in the summary, whether each is now **fixed**, **still present**, or
**rejected as a false positive** — an audit trail instead of a fresh list every time.

### Learning from author pushback

The PR's author sometimes disagrees with a finding — a thread reply ("this is the Python
3.14 syntax", as in AstrBot #5) or a 👎 on the inline comment. Both are read back before
the next review and injected into the prompt as a binding `Author feedback` section: a
rebutted finding is not repeated at the same place unless the current diff contains
verifiable new evidence that answers the rebuttal, and if the model believes the author is
wrong it must argue that in the summary rather than silently re-report. Attributions are
kept verbatim (`@user: "quote"`), so teammates' views are not laundered into the author's.

### CI as the ground truth about "does it build"

The bot has no execution environment, so "does it compile" is not its question — CI's
answer is read from the head commit's check runs and commit statuses and stated to the
model as fact: green CI means compilation and imports were *executed and passed*, and a
prompt section forbids `invalid syntax` / `does not compile` / `cannot be imported`
findings outright, naming the newer-grammar hypothesis (Python 3.14's paren-less
multi-exception except, which AstrBot #5 and #64 both misjudged as critical). Failed
checks are named instead of re-reported; a commit with no CI renders no section — silence
is never presented as success. Requires the App to have `Checks: read`; without it the
section is simply absent and the scope rule in the review brief still applies.

With `REVIEW_VERIFY=true`, each critical/high/medium finding additionally goes through a
blind second pass: a separate AI call that sees the diff and the claim only — never the
first verdict — and is asked to *refute* it. A finding the checker confirms is marked
`[re-checked]`; one it refutes is demoted a level and marked `[not confirmed on re-check]`
rather than deleted, because a disagreement between two passes is itself information. The
re-check costs one AI call per significant finding, so it ships off.

## Deployment

Self-hosted (Docker or a VPS):

1. **Prepare the config**:
   ```bash
   cp .env.example .env
   ```
   Open `.env` and fill in every field — each has a detailed comment saying where its value comes from (App ID, webhook secret, AI provider, …). The two things people trip over most:
   - **Private key — recommended: `PRIVATE_KEY_B64`.** Convert the `.pem` downloaded from the App settings page and paste the single-line output as the value:
     ```bash
     base64 -w0 xero-review-bot.private-key.pem   # Linux / Git Bash
     base64 -i xero-review-bot.private-key.pem    # macOS
     ```
     Works in Docker and on bare metal alike; nothing to mount. (Alternative: mount the file — add `- ./xero-review-bot.pem:/keys/bot.pem:ro` to the compose `volumes` and set `PRIVATE_KEY_PATH=/keys/bot.pem`.)
   - **`WEBHOOK_SECRET` must be byte-identical** to the secret saved in the App's settings — a mismatch makes GitHub reject every delivery with 401.
2. **Subprocess engines need their own AI key.** The container preinstalls both `pi` and `codex`; they authenticate via `OPENAI_API_KEY` (separate from the bot's `AI_API_KEY`). Just add it to `.env` — compose's `env_file` injects the whole file into the container. Skip it and `REVIEW_ENGINE=auto` falls back to the `agent` engine; the bot keeps working either way.
3. **Start**:
   ```bash
   docker compose up -d --build
   docker compose logs -f     # watch startup; config validation errors exit fast
   ```
4. **Webhook URL**: `https://<your-host>/webhook` — must be reachable from the internet (GitHub delivers events to it; for a home server use a reverse proxy or tunnel).

### 0. Create the GitHub App

GitHub → Settings → Developer settings → GitHub Apps → **New GitHub App**:

| Setting | Value |
|---|---|
| Webhook URL | `https://<host>/webhook` |
| Webhook secret | any random string — must match `WEBHOOK_SECRET` |
| Subscribed events | **Issue comment** + **Pull request** (+ **Pull request review** for the merge queue; **Push** recommended — seconds-level notice when the base moves) |
| Permissions | Contents: R (RW for the merge queue) · Pull requests: RW · Issues: RW · **Checks: R** · **Code scanning alerts: R** |

Then: **generate a private key** (downloads a `.pem` file), note the numeric **App ID** and the bot's @-name (for `BOT_NAME`), and install the App on the target org/repos.

What you get in the container:
- A `/data` named volume (`xero-data`) caches repo checkouts and `pi` sessions — this is the bot's **incremental memory**; wiping it loses review context. Leave it alone or back it up. The layout:

  | Path | Contents | Safe to delete? |
  |---|---|---|
  | `repos/{owner}__{repo}/pr-{n}` | One shallow checkout per PR (depth `CHECKOUT_DEPTH`, default 100) | Yes — a merged PR's directory can go |
  | `sessions/{owner}__{repo}` | `pi` sessions, **shared per repository** = the incremental project understanding | No |
  | `codex/{owner}__{repo}-pr{n}-{sha}.md` | One `codex` run's output, deleted once read | Nothing to manage |

  Per PR rather than per repository is required, not tidiness: the tree sits at a PR's head, so one shared directory meant two concurrent reviews could each be reading the other's code. Disk use is therefore roughly *concurrently active PRs × shallow clone size*. A duplicate `@bot review` on the same PR is turned away with a note rather than paying for the model twice.
- Both `pi` and `codex` CLIs are preinstalled, so all five engines work out of the box (`REVIEW_ENGINE=auto` probes pi → codex → agent → builtin). If an npm install fails during the image build, that engine is skipped gracefully and selection falls through.
- A built-in rebase sweep loop (`REBASE_SWEEP_ENABLED=true`, every `REBASE_SWEEP_INTERVAL_SECS` = 6h by default) — no external cron required. Optionally, belt-and-braces via host crontab:
  ```bash
  curl -H "Authorization: Bearer $CRON_SECRET" http://localhost:8080/cron
  ```

Endpoints: `POST /webhook` (GitHub), `GET /health`, `GET /cron` (protected by `CRON_SECRET`).

<details>
<summary><b>Docker quick-check — from zero to a working bot</b></summary>

```bash
git clone https://github.com/Xero-Team/xero-bot.git && cd xero-bot
cp .env.example .env && edit .env        # APP_ID, PRIVATE_KEY_B64, WEBHOOK_SECRET, BOT_NAME, AI_*, OPENAI_API_KEY
docker compose up -d --build
curl http://localhost:8080/health        # {"status":"ok",...}
# then set the App's Webhook URL to https://<your-host>/webhook and install the App on your org
```
</details>

## Configuration

All environment variables are documented in [.env.example](.env.example). Highlights:
- `PRIVATE_KEY_PATH` or `PRIVATE_KEY_B64` — one of the two
- Real environment variables always win over `.env` values
- Labels are configurable (`LABEL_*`); defaults: `needs-rebase` / `waiting-on-review` / `waiting-on-author` / `blocked`
- A non-empty `CODEQL_LABEL` makes that label trigger a CodeQL report; empty (default) = command-only
- CodeQL reports require code scanning to be enabled on the repo (CodeQL default setup or a `codeql.yml` workflow); private repos need GitHub Advanced Security

## Local development

```bash
cargo test                    # unit + integration tests (wiremock mocks the GitHub API)
cargo run                     # self-hosted mode on :8080
cargo run --example send_webhook -- issue-comment "@xero-review ping"
cargo run --example send_webhook -- issue-comment "r? @octocat"
cargo run --example send_webhook -- pr-synchronize
cargo run --example send_webhook -- pr-review-approved
```

`send_webhook` signs the payload with `WEBHOOK_SECRET` (default `dev-secret`) and POSTs it to the local server, simulating the GitHub side.

## Architecture

```
src/
├── config.rs          env config (.env loading; real env vars win)
├── webhook.rs         HMAC-SHA256 signature verification + event classification
├── commands.rs        command parser (multi-command / code-block skipping / r? anywhere / ? shorthands / mention-free sessions)
├── handlers.rs        command execution (permission checks, reply rendering, ?r reviewer notification)
├── github.rs          octocrab wrapper (the only GitHub API egress)
├── review.rs          builtin engine + shared publishing pipeline (diff parsing / verdict parsing / rendering / fallback chain)
├── verify.rs          adversarial re-check of findings (blind second pass, stable finding IDs)
├── agent.rs           native review agent (tool-calling loop, tools = GitHub API)
├── engines_subproc.rs pi/codex subprocess engines + git checkout cache
├── codeql.rs          code scanning alerts → PR changed-file mapping → report
├── rebase.rs          mergeable detection + needs-rebase label + sweep
├── merge_queue.rs     merge queue (staging batches, CI gate, main advance; state = labels + staging chain)
├── dispatch.rs        event → background work routing (incl. the mention-free session check)
└── main.rs            self-hosted axum server
```

State persistence: everything lives in GitHub (labels = workflow state, PR reviews = previous-round review memory, the staging merge-commit chain = merge queue) — the bot itself has no database and no external storage.
