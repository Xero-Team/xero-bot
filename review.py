"""AI code-review engine.

Given an installation token, repo, and PR number:
  1. fetch the unified diff
  2. parse out per-file added line numbers (for inline comments)
  3. call an AI endpoint (OpenAI chat / OpenAI responses / Anthropic)
  4. robustly parse the JSON verdict
  5. post a summary review + inline comments on added lines

Only the Python standard library is used for HTTP, so no extra deps beyond PyJWT
(which lives in app_auth). The AI provider is selected by ``api_format``.
"""
from __future__ import annotations

import json
import re
import urllib.error
import urllib.request
from dataclasses import dataclass

API_BASE = "https://api.github.com"
SEVERITIES = ["critical", "high", "medium", "low", "info"]
SEV_META = {
    "critical": ("🔴", "严重"),
    "high":     ("🟠", "高"),
    "medium":   ("🟡", "中"),
    "low":      ("🔵", "低"),
    "info":     ("⚪", "信息"),
}


@dataclass
class AiConfig:
    base_url: str
    api_key: str
    model: str
    api_format: str        # chat | responses | anthropic
    max_diff_chars: int


# --------------------------------------------------------------------------- #
# GitHub API helpers
# --------------------------------------------------------------------------- #
def _gh_request(url: str, token: str, method: str = "GET",
                 accept: str = "application/vnd.github+json",
                 body: dict | None = None, extra_headers: dict | None = None) -> tuple[int, bytes, dict]:
    headers = {
        "Authorization": f"token {token}",
        "Accept": accept,
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "xero-review-bot",
    }
    if extra_headers:
        headers.update(extra_headers)
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return resp.status, resp.read(), dict(resp.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), dict(e.headers)


def fetch_pr_diff(token: str, repo: str, pr_number: int) -> tuple[str, dict]:
    """Return (diff_text, pr_meta). pr_meta has title/body/changed_files/..."""
    url = f"{API_BASE}/repos/{repo}/pulls/{pr_number}"
    status, raw, _ = _gh_request(url, token, accept="application/vnd.github.v3.diff")
    if status != 200:
        raise RuntimeError(f"fetch diff failed ({status}): {raw[:300]!r}")
    diff = raw.decode("utf-8", errors="replace")
    # separate call for metadata (JSON)
    _, mraw, _ = _gh_request(url, token, accept="application/vnd.github+json")
    meta = {}
    try:
        meta = json.loads(mraw.decode("utf-8", errors="replace"))
    except json.JSONDecodeError:
        pass
    return diff, meta


# --------------------------------------------------------------------------- #
# Diff parsing -> per-file added line numbers (the "new" side)
# --------------------------------------------------------------------------- #
def parse_added_lines(diff: str) -> dict[str, set[int]]:
    """For each file in the diff, collect line numbers on the RIGHT (new) side
    that were added (lines starting with '+'). These are the only lines GitHub
    lets an inline review comment attach to."""
    added: dict[str, set[int]] = {}
    current_file = None
    new_line = 0
    for line in diff.splitlines():
        # file header: +++ b/path  (and also /dev/null for deletions)
        m = re.match(r"^\+\+\+ b/(.+)$", line)
        if m:
            current_file = m.group(1)
            added.setdefault(current_file, set())
            continue
        if line.startswith("+++ "):
            current_file = None
            continue
        if line.startswith("@@"):
            mm = re.search(r"\+(\d+)(?:,(\d+))?", line)
            if mm:
                new_line = int(mm.group(1)) - 1
            continue
        if current_file is None:
            continue
        if line.startswith("+") and not line.startswith("+++"):
            new_line += 1
            added[current_file].add(new_line)
        elif line.startswith("-") and not line.startswith("---"):
            pass  # removed line, new line number unchanged
        else:
            new_line += 1
    return added


def truncate(diff: str, max_chars: int) -> tuple[str, bool]:
    if len(diff) <= max_chars:
        return diff, False
    return diff[:max_chars], True


# --------------------------------------------------------------------------- #
# AI calls (three formats)
# --------------------------------------------------------------------------- #
def _ai_post(url: str, headers: dict, body: dict) -> dict:
    data = json.dumps(body).encode("utf-8")
    req = urllib.request.Request(url, data=data, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            return json.loads(resp.read().decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"AI request failed ({e.code}) to {url}: {e.read()[:400]!r}")


def call_ai(cfg: AiConfig, system_prompt: str, user_prompt: str) -> str:
    base = cfg.base_url.rstrip("/")
    fmt = cfg.api_format.lower()

    if fmt == "chat":
        url = f"{base}/chat/completions"
        headers = {"Authorization": f"Bearer {cfg.api_key}", "Content-Type": "application/json"}
        body = {
            "model": cfg.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "temperature": 0.2,
            "response_format": {"type": "json_object"},
        }
        out = _ai_post(url, headers, body)
        return out["choices"][0]["message"]["content"]

    if fmt == "responses":
        url = f"{base}/responses"
        headers = {"Authorization": f"Bearer {cfg.api_key}", "Content-Type": "application/json"}
        body = {
            "model": cfg.model,
            "input": user_prompt,
            "instructions": system_prompt,
            "text": {"format": {"type": "json_object"}},
        }
        out = _ai_post(url, headers, body)
        if isinstance(out.get("output_text"), str):
            return out["output_text"]
        # fallback: walk output array
        for item in reversed(out.get("output", [])):
            for c in item.get("content", []):
                if c.get("type") == "output_text" and c.get("text"):
                    return c["text"]
        raise RuntimeError(f"responses API: no text in {out!r}")

    if fmt == "anthropic":
        url = f"{base}/v1/messages"
        headers = {
            "x-api-key": cfg.api_key,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
        }
        body = {
            "model": cfg.model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": [{"role": "user", "content": user_prompt}],
        }
        out = _ai_post(url, headers, body)
        for block in out.get("content", []):
            if block.get("type") == "text" and block.get("text"):
                return block["text"]
        raise RuntimeError(f"anthropic API: no text in {out!r}")

    raise ValueError(f"unknown api_format: {cfg.api_format}")


# --------------------------------------------------------------------------- #
# JSON verdict parsing (robust)
# --------------------------------------------------------------------------- #
def parse_verdict(text: str) -> dict | None:
    if not text:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        pass
    # strip ```json ... ``` fences
    m = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(1))
        except json.JSONDecodeError:
            pass
    # last resort: greediest {...}
    m = re.search(r"\{.*\}", text, re.DOTALL)
    if m:
        try:
            return json.loads(m.group(0))
        except json.JSONDecodeError:
            pass
    return None


# --------------------------------------------------------------------------- #
# Prompt
# --------------------------------------------------------------------------- #
SYSTEM_PROMPT = (
    "你是一名资深、严谨的安全与代码质量审查员。审查 pull request 的代码改动,"
    "按风险分级输出问题。只报告真实问题,不要为了凑数编造。"
    "输出必须是严格的 JSON(不要加任何解释性文字、不要 markdown 围栏)。"
    "JSON schema: "
    '{"summary": "一句话总体评价", '
    '"findings": [{"severity": "critical|high|medium|low|info", '
    '"title": "简短标题", "file": "改动中的文件路径", '
    '"line": 整数行号(改动新增行之一,若不适用填1), '
    '"description": "问题描述与潜在影响", '
    '"suggestion": "具体修复建议"}]}。'
    "severity 标准: critical=安全漏洞(注入/RCE/鉴权绕过/数据丢失); "
    "high=逻辑bug/资源泄漏/竞态/核心功能损坏; "
    "medium=边界条件/错误处理缺失; low=风格/可维护性; info=建议/疑问/nit。"
    "用中文输出 description 和 suggestion。若无问题, findings 为空数组。"
)


def build_user_prompt(diff: str, pr_meta: dict, truncated: bool) -> str:
    title = pr_meta.get("title", "")
    body = (pr_meta.get("body") or "")[:2000]
    note = "\n\n[注意: diff 已截断,仅展示前部分改动]\n" if truncated else ""
    return (
        f"PR 标题: {title}\nPR 描述: {body}\n\n"
        f"以下是 PR 的 unified diff(只关注新增的代码):\n{diff}{note}\n\n"
        f"请审查上述改动并按指定 JSON schema 输出。"
    )


# --------------------------------------------------------------------------- #
# Rendering & posting
# --------------------------------------------------------------------------- #
def render_summary(verdict: dict) -> str:
    findings = verdict.get("findings", []) or []
    summary = verdict.get("summary", "(无总结)")
    counts = {s: 0 for s in SEVERITIES}
    for f in findings:
        sev = (f.get("severity") or "info").lower()
        if sev in counts:
            counts[sev] += 1
        else:
            counts["info"] += 1
    table = "| 等级 | 数量 |\n|---|---|\n"
    for s in SEVERITIES:
        icon, label = SEV_META[s]
        table += f"| {icon} {label} | {counts[s]} |\n"
    lines = [
        "## 🤖 AI Code Review",
        "",
        f"**{summary}**",
        "",
        "### 风险分级",
        "",
        table,
    ]
    if not findings:
        lines += ["未发现问题 🎉"]
        return "\n".join(lines)
    for s in SEVERITIES:
        items = [f for f in findings if (f.get("severity") or "info").lower() == s]
        if not items:
            continue
        icon, label = SEV_META[s]
        lines += [f"\n### {icon} {label} ({len(items)})", ""]
        for f in items:
            file = f.get("file", "?")
            line = f.get("line", "?")
            title = f.get("title", "(无标题)")
            desc = f.get("description", "")
            sug = f.get("suggestion", "")
            lines += [f"- **`{file}:{line}` — {title}**", f"  {desc}"]
            if sug:
                lines += [f"  💡 {sug}"]
    return "\n".join(lines)


def post_review(token: str, repo: str, pr_number: int, verdict: dict,
                added_lines: dict[str, set[int]]) -> None:
    """Post a PR review with summary + inline comments where line is a real added line.
    Findings whose line isn't an added line are already in the summary, so no loss."""
    inline = []
    for f in verdict.get("findings", []) or []:
        file = f.get("file")
        line = f.get("line")
        if not isinstance(file, str) or not isinstance(line, int):
            continue
        if line in added_lines.get(file, set()):
            icon, _ = SEV_META.get((f.get("severity") or "info").lower(), ("•", ""))
            inline.append({
                "path": file,
                "line": line,
                "side": "RIGHT",
                "body": f"{icon} **{f.get('title','')}**\n\n{f.get('description','')}\n\n💡 {f.get('suggestion','')}".strip(),
            })

    summary_body = render_summary(verdict)
    url = f"{API_BASE}/repos/{repo}/pulls/{pr_number}/reviews"
    body = {
        "body": summary_body,
        "event": "COMMENT",
        "comments": inline,
    }
    status, raw, _ = _gh_request(url, token, method="POST", body=body)
    if status in (200, 201):
        return
    # Some setups reject inline comments on lines slightly off; retry without them.
    if inline:
        body["comments"] = []
        status2, raw2, _ = _gh_request(url, token, method="POST", body=body)
        if status2 in (200, 201):
            return
        # last resort: plain issue comment
        _post_issue_comment(token, repo, pr_number, summary_body)
        return
    # fallback: plain comment
    _post_issue_comment(token, repo, pr_number, summary_body + f"\n\n_(review post returned {status})_")


def _post_issue_comment(token: str, repo: str, pr_number: int, body: str) -> None:
    url = f"{API_BASE}/repos/{repo}/issues/{pr_number}/comments"
    _gh_request(url, token, method="POST", body={"body": body})


def _post_processing_comment(token: str, repo: str, pr_number: int, text: str) -> None:
    _post_issue_comment(token, repo, pr_number, text)


# --------------------------------------------------------------------------- #
# Entry point
# --------------------------------------------------------------------------- #
def review_pr(token: str, repo: str, pr_number: int, cfg: AiConfig,
              bot_login: str | None = None) -> str:
    """Run a full review. Returns a short status string. Never raises —
    failures are reported back to the PR as comments."""
    try:
        _post_processing_comment(token, repo, pr_number, "🔄 正在审查,稍候…")
    except Exception:
        pass
    try:
        diff, meta = fetch_pr_diff(token, repo, pr_number)
        diff, truncated = truncate(diff, cfg.max_diff_chars)
        added = parse_added_lines(diff)
        user_prompt = build_user_prompt(diff, meta, truncated)
        raw = call_ai(cfg, SYSTEM_PROMPT, user_prompt)
        verdict = parse_verdict(raw)
        if verdict is None:
            body = ("## 🤖 AI Code Review\n\n⚠️ 未能解析模型返回的 JSON,"
                    "以下为原始输出:\n\n```\n" + raw + "\n```")
            _post_issue_comment(token, repo, pr_number, body)
            return "parse-failed"
        post_review(token, repo, pr_number, verdict, added)
        return "ok"
    except Exception as e:  # noqa: BLE001 - report, never crash the webhook thread
        try:
            _post_issue_comment(token, repo, pr_number,
                                f"## 🤖 AI Code Review\n\n❌ 审查出错: `{e}`")
        except Exception:
            pass
        return f"error: {e}"
