# xero-bot

[English](README.md) | [简体中文](README.zh-CN.md)

Org-wide GitHub App bot for the Xero-Team. Written in Rust — a single binary with two deployment modes (Vercel serverless / Docker self-hosted).

Features:
- **bors/triagebot-style comment commands** — `r?`, `?r cc`, label management, assign/claim, `r+` approval on behalf, and more
- **Incremental AI code review** — learns the project first and builds on the previous review round, instead of looking at the diff in isolation
- **Rebase reminders** — when a PR conflicts with its target branch, adds the `needs-rebase` label and a reminder; clears it once resolved
- **CodeQL quality reports** — reads the repo's existing code scanning alerts and maps them to files changed in the PR

## Command reference

Issued in comments (case-insensitive; one comment may contain several commands; content inside code blocks is ignored):

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
| `@xero-review claim` / `unclaim` | Claim/release (assign to self / remove self) |
| `@xero-review r+` | Approve on behalf: the bot verifies the commenter has write access, then submits an APPROVE review in their name |
| `@xero-review r+ as @user` | Approve in @user's name (for forwarding an approval given elsewhere — bors' `r=`) |
| `@xero-review r-` | Withdraw a previous bot APPROVE (dismiss) |

Automatic behavior (no command needed):
- After a PR push/reopen, checks for conflicts → adds `needs-rebase` + a reminder comment; once resolved → removes the label
- Periodic sweep (Vercel Cron daily / self-hosted default 6h) as a fallback check
- Adding the `CODEQL_LABEL` label to a PR (if configured) → auto-generates a CodeQL report

## AI review engine

Selected via `REVIEW_ENGINE`:

| Engine | Mechanism | Incremental capability | Platform |
|---|---|---|---|
| `agent` (default) | tool-calling loop, tools = GitHub API (list/read/search code); explores the project before reviewing | Injects the previous bot review on this PR + the list of newer commits | Vercel + self-hosted |
| `builtin` | single HTTP call (OpenAI chat/responses/Anthropic formats) | Same (context injection) | Vercel + self-hosted |
| `pi` | subprocess `pi -p --session-dir`, read-only toolset | **Session continuity**: per-repo session files remember project understanding | Self-hosted only (preinstalled in Docker) |
| `codex` | subprocess `codex exec --sandbox read-only -o` | Same (`codex exec resume`) | Self-hosted only (preinstalled in Docker) |
| `auto` | probes in order: pi → codex → agent → builtin | - | - |

`agent` automatically falls back to `builtin` on timeout/failure. All engines share the same publishing pipeline: risk-tiered summary table + inline comments on added lines + a publishing fallback chain (with inline → without inline → plain comment).

## Deployment

Both modes run the exact same code — the difference is only in how you operate them:

| | Vercel | Docker |
|---|---|---|
| Maintenance | zero ops | you own the host |
| Time limit per invocation | 300 s (Hobby) / 800 s (Pro) | none |
| `pi` / `codex` engines | unavailable | both preinstalled |
| Scheduled sweep | Vercel Cron (daily, managed) | built-in sweep loop (default 6h) |
| Webhook path | `/api/webhook` | `/webhook` |
| Private key | `PRIVATE_KEY_B64` | `PRIVATE_KEY_PATH` or `PRIVATE_KEY_B64` |

### 0. Create the GitHub App (both modes)

GitHub → Settings → Developer settings → GitHub Apps → **New GitHub App**:

| Setting | Value |
|---|---|
| Webhook URL | `https://<host>/api/webhook` (Vercel) or `https://<host>/webhook` (self-hosted) |
| Webhook secret | any random string — must match `WEBHOOK_SECRET` |
| Subscribed events | **Issue comment** + **Pull request** |
| Permissions | Contents: R · Pull requests: RW · Issues: RW · **Code scanning alerts: R** |

Then: **generate a private key** (downloads a `.pem` file), note the numeric **App ID** and the bot's @-name (for `BOT_NAME`), and install the App on the target org/repos.

### 1. Vercel (recommended, zero ops)

1. **Deploy the project** — push this repo to GitHub and import it into Vercel (*Add New… → Project*); the Vercel Rust runtime builds the `api/*.rs` entrypoints automatically, and `vercel.json` already sets the function timeouts and the cron schedule. The CLI works too:
   ```bash
   npm i -g vercel && vercel link && vercel --prod
   ```
2. **Configure environment variables** — Project → Settings → Environment Variables, or `vercel env add <KEY> production`:

   | Variable | Notes |
   |---|---|
   | `APP_ID` | numeric App ID |
   | `PRIVATE_KEY_B64` | base64 of the `.pem` file — serverless has no filesystem, so `PRIVATE_KEY_PATH` does **not** work here |
   | `WEBHOOK_SECRET` | same value as in the App settings |
   | `BOT_NAME` | e.g. `xero-review` |
   | `AI_BASE_URL` / `AI_API_KEY` / `AI_MODEL` / `API_FORMAT` | LLM provider; see [.env.example](.env.example) |
   | `REVIEW_ENGINE` | `auto` (default) — on Vercel this resolves to `agent`/`builtin` |
   | `CRON_SECRET` | any random string; Vercel Cron sends it as a Bearer token automatically |

   Encode the private key (value only, no line breaks):
   ```bash
   base64 -w0 app.pem        # Linux / Git Bash
   base64 -i app.pem         # macOS (already single-line)
   ```
   **Redeploy after adding or changing env vars** (`vercel --prod` or Deployments → Redeploy) — env changes don't apply to already-running functions.

3. **Cron** — `vercel.json` already declares a daily job (`0 3 * * *`, the Hobby plan's minimum granularity) hitting `/api/cron`. Vercel attaches `Authorization: Bearer $CRON_SECRET` automatically as long as `CRON_SECRET` is configured — no extra auth setup needed.
4. **Verify**:
   ```bash
   curl https://<your-app>.vercel.app/api/health
   # {"status":"ok","configured":true,...}
   ```
   and check *Recent Deliveries* on the App settings page — webhook pings should show a green check.

**Vercel limits (verified):** function max duration is 300 s on Hobby / 800 s on Pro (`vercel.json` sets 300; raise it on Pro if reviews get cut off). The `pi`/`codex` subprocess engines are unavailable on Vercel — `REVIEW_ENGINE=auto` falls back to `agent`. Cold starts add a few seconds to the first request after inactivity.

### 2. Docker (self-hosted, full feature set)

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
     Works in Docker and on Vercel alike; nothing to mount. (Alternative: mount the file — add `- ./xero-review-bot.pem:/keys/bot.pem:ro` to the compose `volumes` and set `PRIVATE_KEY_PATH=/keys/bot.pem`.)
   - **`WEBHOOK_SECRET` must be byte-identical** to the secret saved in the App's settings — a mismatch makes GitHub reject every delivery with 401.
2. **Subprocess engines need their own AI key.** The container preinstalls both `pi` and `codex`; they authenticate via `OPENAI_API_KEY` (separate from the bot's `AI_API_KEY`). Just add it to `.env` — compose's `env_file` injects the whole file into the container. Skip it and `REVIEW_ENGINE=auto` falls back to the `agent` engine; the bot keeps working either way.
3. **Start**:
   ```bash
   docker compose up -d --build
   docker compose logs -f     # watch startup; config validation errors exit fast
   ```
4. **Webhook URL**: `https://<your-host>/webhook` — must be reachable from the internet (GitHub delivers events to it; for a home server use a reverse proxy or tunnel).

What you get in the container:
- A `/data` named volume (`xero-data`) caches repo checkouts and `pi` sessions — this is the bot's **incremental memory**; wiping it loses review context. Leave it alone or back it up.
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
- `PRIVATE_KEY_PATH` (self-hosted) or `PRIVATE_KEY_B64` (Vercel) — one of the two
- Real environment variables always win over `.env` values (so Vercel dashboard config takes precedence)
- Labels are configurable (`LABEL_*`); defaults: `needs-rebase` / `waiting-on-review` / `waiting-on-author` / `blocked`
- A non-empty `CODEQL_LABEL` makes that label trigger a CodeQL report; empty (default) = command-only
- CodeQL reports require code scanning to be enabled on the repo (CodeQL default setup or a `codeql.yml` workflow); private repos need GitHub Advanced Security

## Local development

```bash
cargo test                    # 45 unit + 7 integration tests (wiremock mocks the GitHub API)
cargo run                     # self-hosted mode on :8080
cargo run --example send_webhook -- issue-comment "@xero-review ping"
cargo run --example send_webhook -- issue-comment "r? @octocat"
cargo run --example send_webhook -- pr-synchronize
```

`send_webhook` signs the payload with `WEBHOOK_SECRET` (default `dev-secret`) and POSTs it to the local server, simulating the GitHub side.

## Architecture

```
src/
├── config.rs          env config (.env loading; real env vars win)
├── webhook.rs         HMAC-SHA256 signature verification + event classification
├── commands.rs        command parser (multi-command / code-block skipping / r? anywhere / ? shorthands)
├── handlers.rs        command execution (permission checks, reply rendering)
├── github.rs          octocrab wrapper (the only GitHub API egress)
├── review.rs          builtin engine + shared publishing pipeline (diff parsing / verdict parsing / rendering / fallback chain)
├── agent.rs           native review agent (tool-calling loop, tools = GitHub API)
├── engines_subproc.rs pi/codex subprocess engines + git checkout cache (self-hosted only)
├── codeql.rs          code scanning alerts → PR changed-file mapping → report
├── rebase.rs          mergeable detection + needs-rebase label + sweep
├── dispatch.rs        event → background work routing (shared by both entrypoints)
└── main.rs            self-hosted axum server

api/                   Vercel entrypoints (webhook/cron/health, AppState::wait_until background work)
```

State persistence: everything lives in GitHub (labels = workflow state, PR reviews = previous-round review memory) — the bot itself has no database and no external storage.

Build note: `vercel_runtime` is vendored in `vendor/vercel_runtime` with a one-line fix for a unix-only compile issue (see `[patch.crates-io]` in `Cargo.toml`) — both the Docker build and the Vercel build depend on the `vendor/` directory being present.
