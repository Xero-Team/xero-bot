# xero-review-bot

An AI code-review bot for the **Xero-Team** GitHub organization. Install it once as an organization GitHub App, then mention `@<bot-name> review` on **any** pull request in **any** repo under the org to get an on-demand, risk-classified review. No per-repo setup.

- **Trigger:** `@bot review` in a PR comment (on demand — only when you ask).
- **AI provider:** your own endpoint / API key / model, configured on the server (not in GitHub). Supports three API formats: OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages.
- **Output:** a summary review comment with a risk-tier table (🔴 critical / 🟠 high / 🟡 medium / 🔵 low / ⚪ info) plus inline comments on the specific added lines.
- **Where it runs:** anywhere you can run Python and expose a public HTTPS endpoint. This repo provides only the code — deployment (Tailscale Funnel, Cloudflare Tunnel, reverse proxy, VPS, …) and process management (systemd, Docker, tmux, …) are up to you.

---

## How it works

```
PR comment "@xero-review review"
  → GitHub sends an issue_comment webhook
  → this server verifies the HMAC signature, fetches an installation token (JWT),
    pulls the PR diff, calls your AI endpoint, parses the JSON verdict,
    and posts a review (summary + inline comments on added lines).
```

Because the App is installed at the organization level (all repositories), the webhook fires for every repo — that's why one bot covers the whole org with zero per-repo config.

---

## Files

| File | Purpose |
|---|---|
| `bot.py` | HTTP webhook server (signature verification, event dispatch, fires review in a thread) |
| `review.py` | Review engine: fetch diff → parse added lines → call AI (3 formats) → robust JSON parse → post review |
| `app_auth.py` | GitHub App JWT signing + installation access token exchange (PyJWT + stdlib) |
| `.env.example` | Configuration template — copy to `.env` and fill in |
| `requirements.txt` | Runtime deps: `pyjwt`, `cryptography` |

---

## Setup

### 1. Create the organization GitHub App

Go to **`https://github.com/organizations/Xero-Team/settings/apps`** → **New GitHub App**.

| Field | Value |
|---|---|
| GitHub App name | `xero-review` (your choice; set `BOT_NAME` to match) |
| Webhook URL | `https://<your-public-endpoint>/webhook` — fill this in after step 2 |
| Webhook secret | any random string — keep it; it goes in `.env` as `WEBHOOK_SECRET` |
| Repository → Contents | **Read-only** |
| Repository → Pull requests | **Read & write** |
| Repository → Issues | **Read & write** |
| Subscribe to events | **`Issue comment`** |

After creating: note the **App ID** (General settings), and **Generate a private key** (`*.pem`) — download it. Put the `.pem` on your server and set `PRIVATE_KEY_PATH` to its path (`chmod 600`).

### 2. Expose the webhook endpoint

GitHub sends webhooks from the public internet, so the server's `PORT` must be reachable at a public HTTPS URL. Use whatever you already have:

- **Tailscale Funnel** — `tailscale funnel 8080` (needs Funnel enabled on your tailnet).
- **Cloudflare Tunnel** — `cloudflared tunnel --url http://localhost:8080`.
- A reverse proxy (nginx/Caddy) terminating TLS in front of `localhost:8080`.
- A public VPS running the bot directly.

Take the resulting `https://...` URL and set it as the App's **Webhook URL** (append `/webhook`).

### 3. Configure and run

```bash
git clone https://github.com/Xero-Team/xero-review-bot.git
cd xero-review-bot
cp .env.example .env
# edit .env: APP_ID, PRIVATE_KEY_PATH, WEBHOOK_SECRET, BOT_NAME,
#           AI_BASE_URL, AI_API_KEY, AI_MODEL, API_FORMAT
# place the .pem file at PRIVATE_KEY_PATH (chmod 600)
pip install -r requirements.txt   # or: uv run --with pyjwt --with cryptography python bot.py
python bot.py
```

Run it however your environment likes (systemd, Docker, tmux, nohup, a PaaS, …). The server only needs the `PORT` exposed over HTTPS; it does not care how TLS is terminated.

### 4. Install the App into the organization

On the App's settings page → **Install App** → choose **Xero-Team** → **All repositories** (or pick specific repos).

### 5. Test

Open any pull request under Xero-Team and comment:

```
@xero-review review
```

Within a few seconds you should see a `🔄 正在审查…` comment, then the full review (summary table + inline comments on added lines).

---

## Configuration reference (`.env`)

| Variable | Required | Meaning |
|---|---|---|
| `APP_ID` | yes | Numeric App ID |
| `PRIVATE_KEY_PATH` | yes | Path to the App private key `.pem` |
| `WEBHOOK_SECRET` | yes | The webhook secret set on the App |
| `BOT_NAME` | yes | The App name (matches `@bot review`) |
| `AI_BASE_URL` | yes | AI endpoint base URL, **no trailing slash** |
| `AI_API_KEY` | yes | Your API key |
| `AI_MODEL` | yes | Model id |
| `API_FORMAT` | yes | `chat` \| `responses` \| `anthropic` |
| `PORT` | no | HTTP listen port (default `8080`) |
| `MAX_DIFF_CHARS` | no | Truncate diffs above this many chars (default `60000`) |

### `API_FORMAT` and URL conventions

The code appends the path to `AI_BASE_URL`:

| Format | Request | Auth header |
|---|---|---|
| `chat` | `POST {AI_BASE_URL}/chat/completions` | `Authorization: Bearer {key}` |
| `responses` | `POST {AI_BASE_URL}/responses` | `Authorization: Bearer {key}` |
| `anthropic` | `POST {AI_BASE_URL}/v1/messages` | `x-api-key: {key}` + `anthropic-version: 2023-06-01` |

So for OpenAI set `AI_BASE_URL=https://api.openai.com/v1`; for Anthropic set `AI_BASE_URL=https://api.anthropic.com`.

---

## Risk classification

| Level | Meaning |
|---|---|
| 🔴 Critical | Security: injection, RCE, auth bypass, data loss |
| 🟠 High | Logic bug, resource leak, race, core-functionality breakage |
| 🟡 Medium | Edge-case / error-handling gaps |
| 🔵 Low | Style / maintainability |
| ⚪ Info | Suggestion / question / nit |

---

## Behavior notes

- **On-demand only:** reviews run only when someone posts `@bot review`. No automatic review on PR open or push.
- **Inline comments** are attached only to lines that were *added* in the PR (the diff's `+` lines). Findings whose reported line isn't an added line still appear in the summary.
- **Large PRs** are truncated to `MAX_DIFF_CHARS`; the review notes the truncation.
- **Malformed AI output:** if the model's JSON can't be parsed, the raw text is posted with a warning instead of failing silently.
- **Never blocks a PR:** any internal error is reported as a comment; the bot never crashes the webhook flow.

---

## Security

- The `.pem` private key and `.env` are in `.gitignore` — keep them off the repo.
- Webhook requests are HMAC-verified with `WEBHOOK_SECRET`; forged requests get a 401.
- Installation tokens (1h TTL) are fetched per review; nothing is cached on disk.
- The bot needs only the minimum App permissions listed above (contents read, PRs + issues read/write).

---

## Troubleshooting

- **No webhook arrives:** confirm the App's Webhook URL is correct and your endpoint is publicly reachable over HTTPS (check with `curl`).
- **401 from GitHub when fetching the diff:** the App isn't installed on that repo, or the installation token expired (shouldn't happen — fetched fresh each time).
- **"missing required env var":** a variable in `.env` is empty; the server refuses to start.
- **Review says "未能解析 JSON":** the model didn't return clean JSON. Try a model with stronger JSON mode support, or check that `API_FORMAT` matches your endpoint.
