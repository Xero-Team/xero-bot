# Forgejo 接入

Forgejo 是与 GitHub App 分开的 provider。两者可以同时启用，但不会共享 token、webhook secret 或身份回退。

## 部署配置

在 `.env` 中成组设置：

```dotenv
FORGEJO_BASE_URL=https://git.example.com
FORGEJO_TOKEN=<dedicated-bot-access-token>
FORGEJO_WEBHOOK_SECRET=<random-webhook-secret>
```

三项全部留空时 Forgejo 路由返回 `503`；只配置一部分时进程拒绝启动；三项齐全时启用 `/webhook/forgejo`。

`FORGEJO_BASE_URL` 可带部署子路径，例如 `https://example.com/forgejo`。客户端会自动追加 `/api/v1`，也接受已经以 `/api/v1` 结尾的地址。

## Forgejo Webhook

在目标仓库或组织的 webhook 设置中：

- Payload URL：`https://<xero-bot-host>/webhook/forgejo`
- Content type：`application/json`
- Secret：与 `FORGEJO_WEBHOOK_SECRET` 完全一致
- 首期事件：`Issue comment`、`Push`；后续 Issue/PR 管理会继续使用同一入口

服务读取 `X-Forgejo-Event`（兼容 `X-Gitea-Event`），并校验
`X-Forgejo-Signature` / `X-Gitea-Signature` 的 HMAC-SHA256。支持 Forgejo 的裸
hex 格式以及兼容代理常见的 `sha256=` 前缀；空 secret、缺失签名或错误签名均返回 `401`。

## Token 与失败语义

- token 只放部署环境，不写入仓库的 `.github/xero-bot.toml`。
- API 请求使用 `Authorization: token <FORGEJO_TOKEN>`。
- 连接超时 5 秒，整体请求超时 15 秒，响应上限 2 MiB。
- 写请求不自动重试；超时后的实际状态可能是未知的，交由人工或上层幂等流程处理。
- 错误只返回状态和经过长度限制的 provider message，不返回 token。

当前变更只建立 provider、验签、事件解析和 HTTP 客户端边界。评论指令执行由 #32 接入，仓库镜像同步由 #33/#34 接入。

## 评论指令

Forgejo webhook 的 `issue_comment` 会复用现有 lexer/parser 和 `.github/xero-bot.toml`
中的 `command_triggers` / `command_sessions`。当前支持：

- `ping`、`help`
- `claim` / `take`、`unclaim` / `untake`
- `label +label -label`、`assign @user`、`cc @user`
- `ready`、`author`、`blocked`
- `r? @user`：Issue 上指派，PR 上请求 reviewer
- `r+`、`r-`：实时检查 write+ 权限；PR 作者不能自我审批；代审批仍受
  `R_PLUS_ALLOW_ON_BEHALF` 控制

评论策略无效、默认分支不可读或权限无法确认时 fail closed。显式 @ 产生的会话写入现有
`XERO_DATA_DIR` SQLite，使用 Forgejo 实例 URL 派生的独立命名空间，不会与 GitHub
installation 的会话混用。

`review`、`codeql`、`queue`、GitHub Checks 和 GitHub 合并队列是 GitHub 专属能力；
Forgejo 上会明确回复“不支持”，不会静默成功。Forgejo Actions 的 CI 结果与 AI review
适配属于后续独立工作。

## 故障排查

- `401 invalid signature`：检查 Forgejo webhook Secret 与 `FORGEJO_WEBHOOK_SECRET`。
- `503 Forgejo provider is not configured`：三项环境变量未成组配置。
- 评论无响应：确认 webhook 订阅了 `Issue comment`，并检查 token 是否具有仓库读取和
  Issue/PR 写权限。
- `permission-error`：确认 token 可以读取 collaborator permission，且评论者是仓库协作者。

## GitHub → Forgejo 单向同步配置

在 **GitHub 仓库默认分支** 的 `.github/xero-bot.toml` 增加：

```toml
[forgejo.mirror]
enabled = true
destination = "xero-team/xero-bot"
```

约束：

- `destination` 必须是严格的 `owner/repository`，不能是 URL、凭据、query、fragment 或额外路径。
- `enabled = true` 时必须提供目标；默认 `false`，缺失该表完全不触发同步。
- 目标 Forgejo 仓库必须已存在，并已配置为从该 GitHub 仓库拉取的 **pull mirror**。
- bot 不创建目标仓库，也不保存/更新 Forgejo 侧的 GitHub 拉取凭据。
- 配置解析失败属于仓库策略错误，同步 fail closed，不会猜测目标。

同步执行只调用 Forgejo `POST /api/v1/repos/{owner}/{repo}/mirror-sync`，方向固定为
GitHub → Forgejo。

### 触发与对账

- 每个 GitHub Push（分支、tag、删除）都会在 rebase 检查后读取默认分支策略并触发同步。
- 未启用、provider 未配置或目标缺失时不发请求。
- 同一 `FORGEJO_BASE_URL + destination` 的并发同步会串行化；重复触发是安全的
  mirror-sync 请求，不会向 GitHub 写 ref。
- `GET /cron` 在已有 sweep 后追加 `forgejo_sync` 摘要，用低频对账补偿丢失的 webhook。
- 不自动重放 Forgejo 写请求。401/403/404/409/超时只记录稳定状态，等待下一次
  Push 或 cron 对账，不猜测目标、不降级为任意 URL 推送。
- 日志只包含源仓库、目标状态和 HTTP 状态，不包含 `FORGEJO_TOKEN`。
