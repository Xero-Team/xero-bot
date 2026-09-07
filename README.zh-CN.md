# xero-bot

[English](README.md) | [简体中文](README.zh-CN.md)

Xero-Team 的组织级 GitHub App 机器人。Rust 实现,单二进制,自托管部署(Docker / VPS)。

功能:
- **bors/triagebot 风格评论命令** — `r?`、`?r cc`、label 管理、assign/claim、`r+` 代审批等
- **增量 AI 代码审查** — 先了解项目、结合上一轮审查意见,而非孤立地看 diff
- **rebase 提醒** — PR 与目标分支冲突时自动打 `needs-rebase` 标签并提醒,解决后自动清除
- **CodeQL 质量报告** — 读取仓库存量 code scanning 告警,映射到 PR 变更文件
- **中英双语回复** — 依 PR 自身的 commit 信息决定用中文还是英文,无需配置

## 命令参考

评论中发出(大小写不敏感,一条评论可含多条命令,代码块内的内容会被忽略)。

大部分命令在 issue 上同样可用 —— GitHub 的标签、指派、评论对 issue 与 PR 是同一套 API。
只有 `review`、`codeql`、`r+`、`r-` 这四条需要 PR,在 issue 上使用会明确回复说明而非静默失败。
在 issue 上 `r? @用户` 只是指派,因为 issue 没有 reviewer。

### 免 @ 会话

同一用户在同一 PR/issue 上**带 @ 执行过一次命令**(如 `@xero-review help`)后,会话即已开启:
之后的评论可以用下述免 @ 形式直接下指令。只有**明确的指令**才会被解析 —— 评论必须以动词
**开头**,散文永远不会被当成命令。可免 @ 的动词是无歧义的那几个:`review`、`codeql`、`ready`、
`author`、`blocked`、`ping`、`help`,以及裸 `r+` / `r-`。带参数的动词(`claim`、`label`、
`cc`、`assign`)和组合形式仍需带 @;裸 `r? @user` 与 `?r` 本来就无需 @。没有会话的用户发免 @
指令会收到一行说明,而不是石沉大海。

| 命令 | 说明 |
|---|---|
| `@xero-review review` | AI 代码审查(增量:结合上一轮审查与新提交) |
| `@xero-review codeql` | CodeQL 质量报告 |
| `@xero-review ping` | 健康检查 |
| `@xero-review help` | 命令帮助 |
| `r? @user` | 请求 @user 审查(自动指派;`r? user` 不带 @ 也可以;可在评论任意位置) |
| `@xero-review cc @u1 @u2` | 抄送/通知用户 |
| `?r` 或 `@xero-review ready` | 标记等待审查(打 `waiting-on-review`,摘掉另外两个状态标签) |
| `?r cc @user` | ready + cc 组合(triagebot 快捷风格) |
| `@xero-review author` | 标记等待作者(`waiting-on-author`) |
| `@xero-review blocked` | 标记受阻(`blocked`) |
| `@xero-review label +bug -wip` | 添加/移除标签 |
| `@xero-review assign @user` | 指派给 @user |
| `@xero-review claim` / `unclaim` | 认领/释放(指派给自己/移除自己) |
| `@xero-review r+` | 代审批:bot 校验评论者有 write 权限、且不是本 PR 作者后,以其名义提交 APPROVE review |
| `@xero-review r+ as @user` | 以 @user 名义代审批(即 bors 的 `r=`,用于转发在其他渠道给出的批准)。**未设 `R_PLUS_ALLOW_ON_BEHALF=true` 时一律拒绝** —— 见[审批](#审批) |
| `@xero-review r-` | 撤回 bot 之前的 APPROVE(dismiss) |

自动行为(无需命令):
- PR push/reopen 后检测冲突 → 打 `needs-rebase` + 提醒评论;冲突解决 → 摘标签
- 周期 sweep(内置循环,默认 6h)兜底检测
- 给 PR 打 `codeql` 标签(若配置了 `CODEQL_LABEL`)→ 自动生成 CodeQL 报告

**`?r` 会通知审阅者。** 标签本身不会通知任何人,所以在 PR 上 `?r`/`ready` 还会主动 ping 审阅者:
优先 re-request 当前 Reviewers 列表中的人(无论是 `r?` 加的还是手动在 GitHub UI 上加的);
列表为空时,退而 ping 最近一次 CHANGES_REQUESTED/APPROVED 审查的提交者;两者都没有时,
回复会提示用 `?r @用户` 指定,而不是凭空猜测。

### 审批

由 App 提交的 APPROVE review 是一次**真实**批准:要求 1 个批准的分支保护规则会把它计入。
所以 `r+` 是一次特权写入,不是一条评论。三条规则:

- **评论者必须具备 write 及以上权限**,在提交任何内容之前先向仓库核实。
- **PR 作者永远不能批准自己的 PR**,直接 `r+` 与 `r+ as @其他人` 一并拒绝。GitHub 会为人类
  审查强制这一点,但这里的 review 作者是 App,只能由 bot 自己把关。
- **代他人归功默认关闭。** 设置 `R_PLUS_ALLOW_ON_BEHALF=true` 后,`r+ as @user` 会把批准
  归功于 @user —— 该用户同样需要 write 权限。若默认开启,任何 write 权限持有者都能以同事的
  名义凭空制造一个批准、满足必需审查规则,而该同事根本没看过这个 PR,因此发行版默认关闭。
  普通 `r+` 无论开关如何都不受影响。

被拒绝的 `r+` 除上述校验外不会产生额外 API 调用;`help` 表会说明本部署处于开关的哪一侧。

### 回复语言

bot 用中文还是英文回复(AI 审查的正文同样如此)由 PR 自身的 commit 标题决定:英文居多用英文,
中文居多用中文。每条 commit 一票,所以一条长信息不会替其余 commit 做决定;只读标题行,
因此 `Signed-off-by`、`Co-authored-by` 这类英文 trailer 不会把中文 PR 带偏。commit 什么都
看不出来时(`bump deps`、`v2 -> v3`)退而参考触发评论,仍无法判断则回退英文。无需任何配置;
只支持这两种语言 —— 以汉字书写的日文在这里与中文无法区分,会被当作中文回复。

## AI 审查引擎

`REVIEW_ENGINE` 选择:

| 引擎 | 机制 | 增量能力 |
|---|---|---|
| `agent`(默认) | tool-calling 循环,工具=GitHub API(列目录/读文件/搜代码),先探索项目再审查 | 注入本 PR 上一轮 bot 审查 + 其后的新提交列表 |
| `builtin` | 单次 HTTP 调用(OpenAI chat/responses/Anthropic 三种格式) | 同上(上下文注入) |
| `pi` | 子进程 `pi -p --session-dir`,只读工具集 | **会话延续**:per-repo 会话文件记住项目理解 |
| `codex` | 子进程 `codex exec --sandbox read-only -o` | 同上(可 `codex exec resume`) |
| `auto` | 依次探测:pi → codex → agent → builtin | - |

agent 超时/失败自动回退 builtin。所有引擎共用同一发布管线:风险分级表 + 新增行内联评论 + 发布降级链(带内联 → 去内联 → 普通评论)。

### Finding ID、证据与二次证伪(复检)

每条发布的 finding 都带稳定 ID(`XRV-…`)、类型标注,且 description 必须引用其针对的代码。
ID 使 finding 可以跨轮对账:再次审查同一 PR 时,prompt 要求模型逐条核对上一轮的 findings,
并在总结中逐条给出结论 —— **已修复 / 仍存在 / 被判误报** —— 而不是每次都输出一份全新的清单。

### 从作者反馈中学习

PR 作者可能不同意某条发现 —— 在内联评论下回复反驳(如 AstrBot #5 中的 "It's the python
3.14 syntax"),或直接给该评论点 👎。下一轮审查前,bot 会把这些回应读回来,作为**必须遵守**的
`Author feedback` 段注入 prompt:被反驳的发现不再在同一位置重复,除非当前 diff 中存在足以
回应反驳的可验证新证据;若模型认为作者是错的,必须在总结中给出论证,而不是默默重报。
归因保留原文(`@用户: "引用"`),队友的意见不会被混写成作者的立场。

设置 `REVIEW_VERIFY=true` 后,critical/high/medium 级别的每条发现还会经过一次**盲态二次证伪**:
独立的第二次 AI 调用,只拿到 diff 和该条断言(看不到第一轮结论),任务是设法**推翻**它。
复核通过的发现标注 `[已复核]`;被驳回的发现降一级并标注 `[复核未确认]`,而不是删除 ——
两次审查的分歧本身就是信息。复检每条显著发现多花一次模型调用,默认关闭。

## 部署

自托管(Docker 或 VPS):

1. **准备配置**:
   ```bash
   cp .env.example .env
   ```
   打开 `.env` 逐项填写 — 模板里每一项都写了详细注释说明值从哪来(App ID 在哪、webhook secret 怎么生成、AI 怎么配……)。最容易踩坑的两处:
   - **私钥 — 推荐 `PRIVATE_KEY_B64`。** 把 App 设置页下载的 `.pem` 转成单行 base64 粘贴进去:
     ```bash
     base64 -w0 xero-review-bot.private-key.pem   # Linux / Git Bash
     base64 -i xero-review-bot.private-key.pem    # macOS
     ```
     Docker 和裸机通用,无需挂载文件。(备选:挂载文件 — 在 compose 的 `volumes` 加一行
     `- ./xero-review-bot.pem:/keys/bot.pem:ro`,并设 `PRIVATE_KEY_PATH=/keys/bot.pem`。)
   - **`WEBHOOK_SECRET` 必须与 App 设置里存的完全一致** — 不一致的话 GitHub 每次推送都会被 401 拒绝。
2. **子进程引擎要有自己的 AI key。** 容器已预装 `pi` 和 `codex`,它们用 `OPENAI_API_KEY` 认证(与 bot 的 `AI_API_KEY` 是两回事)。直接写进 `.env` 即可 — compose 的 `env_file` 会把整个文件注入容器。不填也不会坏 — `REVIEW_ENGINE=auto` 会回退到 `agent` 引擎,bot 照常工作。
3. **启动**:
   ```bash
   docker compose up -d --build
   docker compose logs -f     # 观察启动;配置校验失败会立刻退出
   ```
4. **Webhook URL**:`https://<your-host>/webhook` — 必须能被公网访问(GitHub 要向它推送事件;家用服务器需反代或内网穿透)。

### 0. 创建 GitHub App

GitHub → Settings → Developer settings → GitHub Apps → **New GitHub App**:

| 项 | 值 |
|---|---|
| Webhook URL | `https://<host>/webhook` |
| Webhook secret | 任意随机字符串 — 必须与 `WEBHOOK_SECRET` 一致 |
| 订阅事件 | **Issue comment** + **Pull request** |
| 权限 | Contents: R · Pull requests: RW · Issues: RW · **Code scanning alerts: R** |

然后:**生成私钥**(会下载 `.pem` 文件),记下数字 **App ID** 与 bot 的 @-名(填 `BOT_NAME`),并把 App 安装到目标组织/仓库。

容器内的既有能力:
- `/data` 具名卷(`xero-data`)缓存仓库 checkout 与 `pi` 会话 — 这是 bot 的**增量记忆**,删掉就丢审查上下文,不要轻易清理。布局:

  | 路径 | 内容 | 可否清理 |
  |---|---|---|
  | `repos/{owner}__{repo}/pr-{编号}` | 每个 PR 一份浅 checkout(深度 `CHECKOUT_DEPTH`,默认 100) | 可以 — 已合并的 PR 目录可安全删除 |
  | `sessions/{owner}__{repo}` | `pi` 会话,**按仓库共享** = 项目理解的增量记忆 | 不要删 |
  | `codex/{owner}__{repo}-pr{编号}-{sha}.md` | `codex` 单轮输出,读完即删 | 无需管理 |

  checkout 按 PR 而非按仓库分开是必须的:工作树停在某个 PR 的 head 上,共用一份会让并发的两轮审查读到对方的代码。磁盘占用因此约为「同时活跃的 PR 数 × 浅克隆大小」。同一个 PR 的重复 `@bot review` 会被直接回绝(回一条"已有一轮审查正在进行"),不会重复花模型钱。
- `pi` 和 `codex` 两个 CLI 都已预装,五个引擎开箱即用(`REVIEW_ENGINE=auto` 依次探测 pi → codex → agent → builtin)。若镜像构建时某个 npm 安装失败,对应引擎会被优雅跳过,探测链继续往下走。
- 内置 rebase sweep 循环(`REBASE_SWEEP_ENABLED=true`,默认每 `REBASE_SWEEP_INTERVAL_SECS`=6h 一轮),无需外部 cron。也可在宿主机 crontab 里再加一道兜底:
  ```bash
  curl -H "Authorization: Bearer $CRON_SECRET" http://localhost:8080/cron
  ```

端点:`POST /webhook`(GitHub)、`GET /health`、`GET /cron`(受 `CRON_SECRET` 保护)。

<details>
<summary><b>Docker 快速上手 — 从零到跑通</b></summary>

```bash
git clone https://github.com/Xero-Team/xero-bot.git && cd xero-bot
cp .env.example .env && edit .env        # 填 APP_ID、PRIVATE_KEY_B64、WEBHOOK_SECRET、BOT_NAME、AI_*、OPENAI_API_KEY
docker compose up -d --build
curl http://localhost:8080/health        # {"status":"ok",...}
# 然后把 App 的 Webhook URL 设为 https://<your-host>/webhook,并把 App 安装到你的组织
```
</details>

## 配置

全部环境变量见 [.env.example](.env.example)。要点:
- `PRIVATE_KEY_PATH` 或 `PRIVATE_KEY_B64` 二选一
- 真实环境变量永远优先于 `.env` 值
- 标签名可配(`LABEL_*`),默认 `needs-rebase` / `waiting-on-review` / `waiting-on-author` / `blocked`
- `CODEQL_LABEL` 非空时,打该标签自动触发 CodeQL 报告;默认空=仅命令触发
- CodeQL 报告要求仓库已启用 code scanning(CodeQL default setup 或 codeql.yml workflow);私有仓库需 GitHub Advanced Security

## 本地开发

```bash
cargo test                    # 单元 + 集成测试(wiremock mock GitHub API)
cargo run                     # 自托管模式跑在 :8080
cargo run --example send_webhook -- issue-comment "@xero-review ping"
cargo run --example send_webhook -- issue-comment "r? @octocat"
cargo run --example send_webhook -- pr-synchronize
```

`send_webhook` 用 `WEBHOOK_SECRET`(默认 `dev-secret`)对 payload 签名后 POST 到本地服务器,模拟 GitHub 侧。

## 架构

```
src/
├── config.rs          env 配置(.env 加载,真实环境变量优先)
├── webhook.rs         HMAC-SHA256 验签 + 事件分类
├── commands.rs        命令解析器(多命令/代码块忽略/r? 任意位置/?短命令/免 @ 会话)
├── handlers.rs        命令执行(权限校验、回复渲染、?r 审阅者通知)
├── github.rs          octocrab 封装(唯一 GitHub API 出口)
├── review.rs          builtin 引擎 + 共享发布管线(diff 解析/verdict 解析/渲染/降级链)
├── verify.rs          finding 二次证伪(盲态二审 + 稳定 finding ID)
├── agent.rs           原生 review agent(tool-calling 循环,工具=GitHub API)
├── engines_subproc.rs pi/codex 子进程引擎 + git checkout 缓存
├── codeql.rs          Code Scanning 告警 → PR 变更文件映射 → 报告
├── rebase.rs          mergeable 检测 + needs-rebase 标签 + sweep
├── dispatch.rs        事件 → 后台工作 路由(含免 @ 会话检查)
└── main.rs            自托管 axum 服务器
```

状态持久化:全部存 GitHub(标签 = 工作流状态,PR review = 上一轮审查记忆)——bot 本身无数据库、无外部存储。
