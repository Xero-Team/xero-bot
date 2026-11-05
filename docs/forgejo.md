# Forgejo 接入

Forgejo 是与 GitHub App 分开的 provider。两者可以同时启用，但不会共享 token、webhook secret 或身份回退。

## 部署配置

在 `.env` 中成组设置：

```dotenv
FORGEJO_BASE_URL=https://git.example.com
FORGEJO_TOKEN=<dedicated-bot-access-token>
FORGEJO_WEBHOOK_SECRET=<random-webhook-secret>
FORGEJO_BOT_NAME=zero-bot
```

`FORGEJO_BOT_NAME` 只影响 Forgejo 的指令解析和 bot 自身识别；留空时继承
`BOT_NAME`。这样 GitHub App 可以继续使用 `@xero-team-bot`，Forgejo 使用
`@zero-bot`，两边互不影响。

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

- 使用专用 bot 账号，在其 Applications 设置创建 access token。评论指令需要 `read:user`（读取 bot 身份）、`write:issue`（评论、标签、assignee）和 `write:repository`（仓库配置、PR、reviewer、review）；组织标签还需要 `read:organization`。镜像同步本身需要 `write:repository`。
- token scope 不会提升账号本身的仓库权限。账号必须能读取默认分支配置，并拥有对应仓库功能的写权限；审批指令还要能查询 collaborator permission，部署时确认该接口对 bot 可用（通常需要仓库管理员权限）。不需要站点级 `admin` scope。
- 参考 [Forgejo token scope 文档](https://forgejo.org/docs/v15.0/user/authentication/token-scope/)。该版本的 Specific repositories token 不允许 `read:user` / `read:organization`，不能用于当前完整评论指令流程；仅镜像同步可按部署版本选择这种限制。访问私有仓库时不要选择 Public only。
- token 只放部署环境，不写入仓库的 `.github/xero-bot.toml`。
- API 请求使用 `Authorization: token <FORGEJO_TOKEN>`。
- 连接超时 5 秒，整体请求超时 15 秒，响应上限 2 MiB。
- 禁止跟随 HTTP 重定向和自动重试（包括协议级重试）；超时后的实际状态可能是未知的，交由人工或上层幂等流程处理。
- 写请求的传输失败或无法解析的响应返回 `UnknownWriteOutcome`，明确要求先核对远端状态再考虑重试；读取失败仍返回 `Transport` / `InvalidResponse`。HTTP 拒绝保留 `Api` 状态码，5xx 也不能据此推断写操作没有生效。
- 错误只返回状态和经过脱敏、最多 512 字符的 provider message，不返回 token。

当前支持独立的 Forgejo webhook、评论指令、持久化恢复，以及显式启用的 GitHub → Forgejo 单向镜像同步和 Forgejo ↔ GitHub Issue / PR 消息桥。

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
Forgejo 上会明确回复“不支持”，不会静默成功。Forgejo Actions CI 使用独立的
`.forgejo/workflows/ci.yml`；AI review 适配仍属于后续独立工作。

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
GitHub → Forgejo。

### Forgejo Actions CI

Forgejo 侧工作流位于 `.forgejo/workflows/ci.yml`，与 GitHub CI 执行相同的格式、
Clippy 和测试命令。它是独立文件，因为 Forgejo 不读取 GitHub 的 workflow 目录；
修改 CI 命令时需要同步更新两份工作流。

镜像同步把 GitHub 的 Push 落到 Forgejo 后，Forgejo Actions 会在该 Push 上再运行一次
同一套检查。仓库必须启用 Actions 单元，并且至少有一个在线的 Forgejo Runner：

- Runner 标签：`ubuntu-latest`、`ubuntu-22.04`、`docker`
- `ubuntu-latest` 后端镜像：`xero-bot-ci:rust-1.99-node20`
- Runner 使用部署环境的 Docker socket 启动隔离的任务容器

镜像定义在 `.forgejo/runner/Dockerfile`，同时包含 Rust 1.99、Clippy、rustfmt
以及 `actions/checkout` 需要的 Node 20。部署 Runner 前构建：

```sh
docker build -f .forgejo/runner/Dockerfile -t xero-bot-ci:rust-1.99-node20 .
```

Forgejo 测试命令会串行运行 Cargo 测试（`--test-threads=1`）。仓库中的状态机测试
包含亚秒级并发期限；共享 Runner 在并行构建或测试较多时可能因 CPU 抢占偶发超时。

CI 不依赖 GitHub App、AI key 或 webhook secret。

### 触发与对账

- 每个 GitHub Push（分支、tag、删除）都会在 rebase 检查后读取默认分支策略并触发同步。
- 未启用、provider 未配置或目标缺失时不发请求。
- 同一 `FORGEJO_BASE_URL + destination` 的并发同步会串行化；重复触发是安全的
  mirror-sync 请求，不会向 GitHub 写 ref。
- `GET /cron` 在已有 sweep 后追加 `forgejo_sync` 摘要。每个进程最多每 15 分钟执行一轮对账，整轮期间拒绝重叠扫描；频繁 cron 不会重复读取配置或请求 mirror-sync。重启后可立即对账。
- 不自动重放 Forgejo 写请求。401/403/404/409/超时只记录稳定状态，等待下一次
  Push 或 cron 对账，不猜测目标、不降级为任意 URL 推送。
- `sync-requested` / `requested` 表示 Forgejo 接受了同步请求，不代表后台镜像拉取已完成。最终结果请查看 Forgejo 的镜像状态。
- 日志只包含源仓库、目标状态和 HTTP 状态，不包含 `FORGEJO_TOKEN`。

### 升级顺序

先升级所有读取本仓库策略的 xero-bot 实例，再往默认分支的配置文件增加 `[forgejo.mirror]`。
旧版采用严格未知字段校验，即使 `enabled = false` 也会拒绝该新表，暂停仓库动作。
因此本仓库的在线 `.github/xero-bot.toml` 及其副本 `examples/repository-config.toml`
只保留注释示例；完整配置在 `example.toml`。回滚到旧版本前，应先从在线配置移除该表。

## Forgejo ↔ GitHub Issue / PR 消息桥

消息桥同步协作内容，代码继续由现有 GitHub → Forgejo pull mirror 复制。
它与 `[forgejo.mirror]` 分别启用；默认关闭。目标仓库必须已存在，且 Forgejo
仓库的 `original_url` 是本 GitHub 仓库的 HTTPS 地址，例如
`https://github.com/xero-team/xero-bot.git`。暂不接受 SSH、带凭据的 origin
或 GitHub Enterprise。GitHub App 必须安装在该 GitHub 仓库上。

### 启用

先部署支持消息桥的 bot，再在 **GitHub 默认分支** 的 `.github/xero-bot.toml` 加入：

```toml
[forgejo.bridge]
enabled = true
destination = "xero-team/xero-bot" # Forgejo 的 owner/repository
issues = true
pull_requests = true
comments = true
```

三个内容开关默认都为 `false`，启用时至少选一个。仅开启 `comments` 时只能为
已有持久映射的对象转发评论，不会创建 Issue / PR。`destination` 始终填写
Forgejo 仓库名，即使 GitHub 仓库名与它不同也不改变含义。

等待 pull mirror 把这份默认分支配置同步到 Forgejo，再启用双方 webhook。
两个方向都会检查双方默认分支策略：配置错误、读取失败、目标不匹配时不写入。
Forgejo → GitHub 的目标从 mirror origin 和 GitHub App 安装清单解析，并核对
实际仓库 ID；不会使用 webhook 中声明的目标地址。

- GitHub App 仓库权限：Metadata / Contents 读，Issues / Pull requests 写。
- GitHub webhook：`Issues`、`Pull request`、`Issue comment`。
- Forgejo token：`read:user`、`write:issue`、`write:repository`，账号本身需具备仓库相应权限。
- Forgejo webhook：Issue、Pull request、Issue comment、Pull request comment，
  以及 Issue / Pull request 的标签事件。兼容 `issue_label` / `pull_request_label`
  事件名和 `label_updated` / `label_cleared` 动作。
- Forgejo 仓库需启用 Issues / Pull Requests 单元；双方分支必须可读取。

Forgejo webhook 的配置和签名方式见前文，也可参考
[Forgejo webhook 文档](https://forgejo.org/docs/latest/user/repository/webhooks/)。
GitHub 的 PR 标签通过 Issues API 管理，权限说明见
[GitHub Labels API](https://docs.github.com/en/rest/issues/labels)。

### 同步范围

| 内容 | 行为 |
| --- | --- |
| 新 Issue / PR | `opened` 事件创建对端对象，双方编号可以不同 |
| 标题、正文 | 双向更新；执行前读取源端当前状态，避免旧 webhook 覆盖新内容 |
| 开放 / 关闭 | 双向关闭和重新打开；合并事件只同步关闭状态，不执行对端合并 |
| 标签 | 按名称同步成员关系、移除已取消的标签；缺少的标签使用源端颜色和描述创建 |
| 普通评论 | 转发新建评论，显示源平台和评论者；不转发评论编辑、删除、行内 review |
| PR 分支 | 仅同仓库、双方已存在的 head/base；支持 base 变更，不传输 Git ref |

已有标签的颜色和描述保留各平台设置。`LABEL_MERGE_QUEUE_QUEUED` /
`LABEL_MERGE_QUEUE_TESTING` 指定的两个合并队列标签不会跨平台导入，也不会被
对端标签集合清除。CI、Checks、review approval、assignee、reviewer、附件迁移、
表情反应和合并队列操作均不在桥接范围内。当前 bot 账号自己的写入会被过滤，
包括它产生的命令回复；其他普通评论正常转发。

PR 来自 fork、分支尚未镜像到对端、仓库未开放 PR 功能时，桥接不会复制代码或
猜测分支。Fork PR 跳过；API 拒绝（例如缺少分支）保留失败事件，修复后可恢复。
已有 Issue / PR 不会按标题或相同编号自动配对，也不做历史数据回填；后续评论
只会发给已建立映射的对端。并发编辑按单消费者处理顺序收敛，不做字段级冲突合并。

### 去重与恢复

验证签名后，webhook 将桥接任务提交到
`XERO_DATA_DIR/command-triggers.sqlite` 才返回成功。独立消费者每两秒检查任务，
服务重启后恢复未完成的事件。命令执行仍使用原来的命令收件箱，两者独立。
普通讨论即使没有 bot 指令，也会进入桥接流程。
反向代理应保留 `X-GitHub-Delivery` / `X-Forgejo-Delivery`（兼容
`X-Gitea-Delivery`），用于区分同一秒内内容相同的不同编辑事件。

映射范围包含 Forgejo API 地址、双方仓库 ID、GitHub installation ID 和仓库名。
同一桥接范围的双向操作串行执行。正文中的 HMAC 隐藏标记用于识别来源；
**复制或删除标记不会重新绑定对象**，SQLite 映射是唯一依据。
请备份整个持久目录及 WAL；数据库丢失后不能靠标题、编号或标记重建映射。

创建对象和评论前先保存写入占位。超时、响应损坏或进程中断后，如果没有可确认的
远端编号，保留 `unknown` 占位，不盲目重复 POST。创建后立刻保存双向映射；
后续元数据步骤失败，可沿已有映射恢复，不再新建对象。已完成任务清除存储的正文，
保留去重键和对象映射。SQLite 与外部 API 不共享事务，不承诺 exactly-once。

失败事件进入 `blocked`，暂停同一来源 Issue / PR 的后续桥接事件，其他对象继续。
不会在后台不断重试写请求。停止服务后可以使用：

```sh
trigger-state /data bridge-deliveries # 待处理/阻塞事件的 KEY、lane、原因
trigger-state /data bridge-status     # 不确定的创建/评论及其 SCOPE、来源编号

# 若没有不确定 POST（例如策略读取失败），修复配置/权限后恢复：
trigger-state /data bridge-retry KEY

# 如果远端实际已创建：PEER 是对端 Issue/PR 编号或评论 ID。
# SOURCE 是 bridge-status 中的来源编号；SCOPE 需按原值引用为单个参数。
trigger-state /data bridge-resolve 'SCOPE' issue forgejo SOURCE PEER '已核对远端对象及其来源'
# kind 可为 issue、pull、comment；side 可为 github、forgejo。

# 仅在核实远端确实没有创建时，用 0 解除占位：
trigger-state /data bridge-resolve 'SCOPE' comment github SOURCE 0 '已确认远端没有该评论'
trigger-state /data bridge-retry KEY
# 然后重启服务。仍会重新检查当前策略、映射和占位。
```

`bridge-retry` 本身不会清除不确定写入占位。对于已完成但未记录结果的创建/评论，
必须先提供远端编号；不能仅因为 5xx 或超时就认定没有生效。

### 上线与回滚

仓库中的示例表保持注释，不会随代码发布自动开启桥接。先升级所有读取该仓库策略的
实例，再添加 `[forgejo.bridge]`，并确认镜像中的配置已经更新。回滚到不认识该表的
旧版本前，先移除该表并完成镜像同步；只设 `enabled = false` 仍会被旧版严格解析器拒绝。
