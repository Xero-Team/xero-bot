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

- token 只放部署环境，不写入仓库的 `.github/xero-bot.toml`。
- API 请求使用 `Authorization: token <FORGEJO_TOKEN>`。
- 连接超时 5 秒，整体请求超时 15 秒，响应上限 2 MiB。
- 禁止跟随 HTTP 重定向和自动重试（包括协议级重试）；超时后的实际状态可能是未知的，交由人工或上层幂等流程处理。
- 错误只返回状态和经过脱敏、最多 512 字符的 provider message，不返回 token。

当前变更只建立 provider、验签、事件解析和 HTTP 客户端边界。评论指令执行由 #32 接入，仓库镜像同步由 #33/#34 接入。
