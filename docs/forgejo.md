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
- 事件：`Issue comment`、`Pull request comment`、`Push`

服务读取 `X-Forgejo-Event`（兼容 `X-Gitea-Event`），并校验
`X-Forgejo-Signature` / `X-Gitea-Signature` 的 HMAC-SHA256。支持 Forgejo 的裸
hex 格式以及兼容代理常见的 `sha256=` 前缀；空 secret、缺失签名或错误签名均返回 `401`。

## Token 与失败语义

- token 只放部署环境，不写入仓库的 `.github/xero-bot.toml`。
- API 请求使用 `Authorization: token <FORGEJO_TOKEN>`。
- 连接超时 5 秒，整体请求超时 15 秒，响应上限 2 MiB。
- 禁止跟随 HTTP 重定向和自动重试（包括协议级重试）；超时后的实际状态可能是未知的，交由人工或上层幂等流程处理。
- 错误只返回状态和经过脱敏、最多 512 字符的 provider message，不返回 token。

当前支持独立的 Forgejo webhook、评论指令和持久化恢复。仓库镜像同步由 #33/#34 接入。

## 评论指令

Forgejo webhook 的 `issue_comment` / `pull_request_comment` 会复用现有 lexer/parser 和 `.github/xero-bot.toml`
中的 `command_triggers` / `command_sessions`。当前支持：

- `ping`、`help`
- `claim` / `take`、`unclaim` / `untake`
- `label +label -label`、`assign @user`、`cc @user`
- `ready`、`author`、`blocked`：仅切换状态标签；`ready` 不自动选取或通知审查者，请使用 `r? @user` 请求审查
- `r? @user`：Issue 上指派，PR 上请求 reviewer
- `r+`、`r-`：实时检查 write+ 权限；PR 作者不能自我审批；代审批仍受
  `R_PLUS_ALLOW_ON_BEHALF` 控制

策略读取固定到默认分支的一个 commit；配置文件 404 只有在同一 commit 的父目录列表证明缺失后才使用默认策略。评论策略无效、默认分支不可读或权限无法确认时 fail closed。显式 @ 产生的会话写入现有
`XERO_DATA_DIR` SQLite，使用 Forgejo 实例 URL 派生的独立命名空间，不会与 GitHub
installation 的会话混用。

`review`、`codeql`、`queue`、GitHub Checks 和 GitHub 合并队列是 GitHub 专属能力；
Forgejo 上会明确回复“不支持”，不会静默成功。Forgejo Actions 的 CI 结果与 AI review
适配属于后续独立工作。

## 持久化与恢复

`202` 表示评论已写入 `XERO_DATA_DIR/forgejo-comments.sqlite`。同一实例、仓库和评论 ID 只入队一次；
重投不会重复审批或回复。单消费者按源时间、评论 ID 排序处理已入队评论，避免分配列表和会话检查相互竞争。
晚到的旧状态命令不会覆盖较新的状态命令。缺少会话的裸指令可等待晚到的唤醒，最多检查 5 次。

没有发出写请求的任务在重启后恢复；已经发出写请求但无法确认结果的任务保留为 `unknown`，
暂停该 Issue/PR 的后续任务。不要盲目重投写请求。先停止服务，再检查：

```sh
trigger-state /data forgejo-inbox
trigger-state /data forgejo-resolve KEY done '已在 Forgejo 核实操作结果'
# 仅在确认该任务的所有写请求均未生效时：
trigger-state /data forgejo-resolve KEY not-sent '未发出或未生效的外部证据'
```

`done` 保留去重记录并解除阻塞；`not-sent` 重新排队，重启后会重新检查当前仓库策略和权限。
这些命令与服务共享排他数据库锁，不支持多个进程同时处理同一数据目录。

## 故障排查

- `401 invalid signature`：检查 Forgejo webhook Secret 与 `FORGEJO_WEBHOOK_SECRET`。
- `503 Forgejo provider is not configured`：三项环境变量未成组配置。
- 评论无响应：确认 webhook 订阅了 `Issue comment` / `Pull request comment`，并检查 token 是否具有仓库读取和
  Issue/PR 写权限。
- `permission-error`：确认 token 可以读取 collaborator permission，且评论者是仓库协作者。

## GitHub → Forgejo 单向同步配置

在 **GitHub 仓库默认分支** 的 `.github/xero-bot.toml` 增加：

```toml
[forgejo.mirror]
enabled = true
destination = "xero-team/xero-bot"
```

GitHub 与 Forgejo 使用同一严格解析器。Forgejo 的 `repository_config` 客户端接口在默认分支的固定 commit 上读取并返回完整配置，包括镜像设置和各配置域的校验错误；评论指令只取其中的评论策略。读取 Forgejo 侧配置不会触发镜像同步。

约束：

- `destination` 必须是严格的 `owner/repository`，不能是 URL、凭据、query、fragment 或额外路径。
- `enabled = true` 时必须提供目标；默认 `false`，缺失该表完全不触发同步。
- 目标 Forgejo 仓库必须已存在，并已配置为从该 GitHub 仓库拉取的 **pull mirror**。
- bot 不创建目标仓库，也不保存/更新 Forgejo 侧的 GitHub 拉取凭据。
- 配置解析失败属于仓库策略错误，同步 fail closed，不会猜测目标。

同步执行只调用 Forgejo `POST /api/v1/repos/{owner}/{repo}/mirror-sync`，方向固定为
GitHub → Forgejo。GitHub Push 驱动、超时/状态分类和 `/cron` 对账由 #34 接入。

### 升级顺序

先升级所有读取本仓库策略的 xero-bot 实例，再往默认分支的配置文件增加 `[forgejo.mirror]`。
旧版采用严格未知字段校验，即使 `enabled = false` 也会拒绝该新表，暂停仓库动作。
因此本仓库的在线 `.github/xero-bot.toml` 及其副本 `examples/repository-config.toml`
只保留注释示例；完整配置在 `example.toml`。回滚到旧版本前，应先从在线配置移除该表。
