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

token 和 webhook secret 至少需要 8 字节，以保证凭据始终被脱敏。远程 Forgejo 必须使用 HTTPS；HTTP 仅允许 localhost / loopback IP，URL 中不得夹带用户名密码。

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

- 使用专用 bot 账号，在其 Applications 设置创建 access token。评论指令需要 `read:user`（读取 bot 身份）、`write:issue`（评论、标签、assignee）和 `write:repository`（仓库配置、PR、reviewer、review）；组织标签还需要 `read:organization`。镜像同步本身需要 `write:repository`。
- token scope 不会提升账号本身的仓库权限。账号必须能读取默认分支配置，并拥有对应仓库功能的写权限；审批指令还要能查询 collaborator permission，部署时确认该接口对 bot 可用（通常需要仓库管理员权限）。不需要站点级 `admin` scope。
- 参考 [Forgejo token scope 文档](https://forgejo.org/docs/v15.0/user/authentication/token-scope/)。该版本的 Specific repositories token 不允许 `read:user` / `read:organization`，不能用于当前完整评论指令流程；仅镜像同步可按部署版本选择这种限制。访问私有仓库时不要选择 Public only。
- token 只放部署环境，不写入仓库的 `.github/xero-bot.toml`。
- API 请求使用 `Authorization: token <FORGEJO_TOKEN>`。
- 连接超时 5 秒，整体请求超时 15 秒，响应上限 2 MiB。
- 禁止跟随 HTTP 重定向和自动重试（包括协议级重试）；超时后的实际状态可能是未知的，交由人工或上层幂等流程处理。
- 写请求的传输失败或无法解析的响应返回 `UnknownWriteOutcome`，明确要求先核对远端状态再考虑重试；读取失败仍返回 `Transport` / `InvalidResponse`。HTTP 拒绝保留 `Api` 状态码，5xx 也不能据此推断写操作没有生效。
- 错误只返回状态和经过脱敏、最多 512 字符的 provider message，不返回 token。

当前变更只建立 provider、验签、事件解析和 HTTP 客户端边界。评论指令执行由 #32 接入，仓库镜像同步由 #33/#34 接入。
