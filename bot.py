"""Xero-Team AI Review Bot — webhook server.

A tiny stdlib-only HTTP server that receives GitHub ``issue_comment`` webhooks,
verifies the HMAC signature, and triggers an AI code review when a PR comment
mentions ``@<BOT_NAME> review``.

Config is loaded from a ``.env`` file (or real environment variables). No third
party web framework — just :mod:`http.server`. Run with:

    python bot.py

Expose ``PORT`` to the public internet over HTTPS however you like.
"""
from __future__ import annotations

import hmac
import hashlib
import json
import os
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import review
import app_auth


# --------------------------------------------------------------------------- #
# Config
# --------------------------------------------------------------------------- #
def _load_dotenv(path: str = ".env") -> None:
    p = Path(path)
    if not p.is_file():
        return
    for line in p.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        k, v = k.strip(), v.strip().strip('"').strip("'")
        if k and k not in os.environ:
            os.environ[k] = v


def _cfg(key: str, default: str = "") -> str:
    return os.environ.get(key, default)


def _int_cfg(key: str, default: int) -> int:
    try:
        return int(os.environ.get(key, str(default)))
    except ValueError:
        return default


def build_ai_config() -> review.AiConfig:
    return review.AiConfig(
        base_url=_cfg("AI_BASE_URL"),
        api_key=_cfg("AI_API_KEY"),
        model=_cfg("AI_MODEL"),
        api_format=_cfg("API_FORMAT", "chat"),
        max_diff_chars=_int_cfg("MAX_DIFF_CHARS", 60000),
    )


def verify_signature(secret: str, body: bytes, signature_header: str | None) -> bool:
    if not signature_header:
        return False
    expected = "sha256=" + hmac.new(secret.encode("utf-8"), body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature_header)


# --------------------------------------------------------------------------- #
# Webhook handling
# --------------------------------------------------------------------------- #
def _handle_review(payload: dict[str, Any]) -> None:
    """Runs in a worker thread. Never raises."""
    try:
        installation_id = payload["installation"]["id"]
        issue = payload.get("issue", {})
        repo = payload.get("repository", {}).get("full_name")
        pr_url = issue.get("pull_request", {}).get("url")
        if not repo or not pr_url:
            return  # not a PR comment
        pr_number = issue.get("number")
        if pr_number is None:
            return
        token = app_auth.installation_token(
            _cfg("APP_ID"), installation_id, _cfg("PRIVATE_KEY_PATH")
        )
        bot_login = payload.get("installation", {}).get("app_slug") or _cfg("BOT_NAME")
        review.review_pr(token, repo, pr_number, build_ai_config(), bot_login)
    except Exception as e:  # noqa: BLE001
        sys.stderr.write(f"[review thread] error: {e}\n")


class WebhookHandler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):  # quieter logging
        sys.stderr.write("[webhook] " + (fmt % args) + "\n")

    def _send(self, code: int, body: str = "") -> None:
        data = body.encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if data:
            self.wfile.write(data)

    def do_GET(self):
        if self.path == "/" or self.path == "/health":
            self._send(200, json.dumps({"status": "ok"}))
        else:
            self._send(404, json.dumps({"error": "not found"}))

    def do_POST(self):
        if self.path != "/webhook":
            self._send(404, json.dumps({"error": "not found"}))
            return
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length) if length else b""

        if not verify_signature(_cfg("WEBHOOK_SECRET"), body,
                                self.headers.get("X-Hub-Signature-256")):
            self._send(401, json.dumps({"error": "invalid signature"}))
            return

        event = self.headers.get("X-GitHub-Event", "")
        if event == "ping":
            self._send(200, json.dumps({"ok": "pong"}))
            return
        if event != "issue_comment":
            self._send(200, json.dumps({"ignored": event}))
            return

        try:
            payload = json.loads(body.decode("utf-8"))
        except json.JSONDecodeError:
            self._send(400, json.dumps({"error": "bad json"}))
            return

        if payload.get("action") != "created":
            self._send(200, json.dumps({"ignored": "action != created"}))
            return

        body_text = payload.get("issue", {}).get("body", "") or ""
        bot_name = _cfg("BOT_NAME")
        pattern = re.compile(rf"^@{re.escape(bot_name)}\s+review\b", re.IGNORECASE)
        if not pattern.search(body_text.strip()):
            self._send(200, json.dumps({"ignored": "no review command"}))
            return

        # don't react to the bot's own comments
        actor = payload.get("comment", {}).get("user", {}).get("login", "")
        if actor and actor.lower() == (payload.get("installation", {}).get("app_slug") or bot_name).lower():
            self._send(200, json.dumps({"ignored": "self comment"}))
            return

        # fire and forget — respond 200 immediately so GitHub doesn't time out
        threading.Thread(target=_handle_review, args=(payload,), daemon=True).start()
        self._send(200, json.dumps({"accepted": True, "reviewing": True}))


def main() -> None:
    _load_dotenv()
    for required in ("APP_ID", "PRIVATE_KEY_PATH", "WEBHOOK_SECRET", "BOT_NAME",
                     "AI_BASE_URL", "AI_API_KEY", "AI_MODEL"):
        if not _cfg(required):
            sys.stderr.write(f"ERROR: missing required env var {required}\n")
            sys.exit(2)
    port = _int_cfg("PORT", 8080)
    server = ThreadingHTTPServer(("0.0.0.0", port), WebhookHandler)
    sys.stderr.write(f"xero-review-bot listening on 0.0.0.0:{port} (POST /webhook)\n")
    sys.stderr.write(f"  bot name: @{_cfg('BOT_NAME')} | api_format: {_cfg('API_FORMAT','chat')}\n")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        sys.stderr.write("\nshutting down\n")
        server.shutdown()


if __name__ == "__main__":
    main()
