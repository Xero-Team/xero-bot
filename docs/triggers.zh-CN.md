# 触发配置与升级指南

[English](triggers.md) | [简体中文](triggers.zh-CN.md)

策略只来自**目标仓库默认分支**的 `.github/xero-bot.toml`，不会读取 fork 或 PR head。
它与 `idle_workflows` 共存，关闭 idle scheduler 仍会读取。仓库内的
[完整默认示例](../.github/xero-bot.toml) 不启用创建/路径动作；
[显式启用示例](../examples/triggers-opt-in.toml) 单独存放，**复制后会启用自动动作**。
两份示例均作为 fixture 进行 TOML 解析和语义校验。

## 手动模式与升级变化

| 模式 | 含义 |
| --- | --- |
| `disabled` | 所有入口、别名、组合和引用该指令的自动动作永久拒绝 |
| `no_mention` | 完整裸指令无需会话即可调用 |
| `mention_once` | 显式 @ 机器人，或该用户/线程已有更早且有效的会话 |
| `always_mention` | 每次调用均须显式 @，已有会话也不例外 |

默认 `claim`、`unclaim`、`cc`、`r?`、`ready` 使用 `no_mention`；
`r+`、`r-` 使用 **`always_mention`**；`review`、`codeql`、`label`、`assign`、
`author`、`blocked`、`ping`、`help`、`queue` 使用 `mention_once`。默认不禁用任何指令。
文件不存在、空文件、仅 idle 配置、部分覆盖均合并这些默认值。
`event_triggers` 和 `path_triggers.rules` 默认为空。`mode = "auto"` 无效，自动订阅独立配置。

别名共用策略：`take` → `claim`；`untake` / `release` / `release-assignment` →
`unclaim`；`?r` / `reviewer` → `ready`；`relabel` → `label`；`commands` → `help`。
`r= @user`、`r+ as @user`、`r+ @user` 是相同的代他人审批形式，须开启
`R_PLUS_ALLOW_ON_BEHALF=true`。同时配置规范名和别名无效；审批参数缺失或多余不会降级成普通批准。

**兼容性变化：**裸单词要求整条评论是完整指令块。`claim`、`take; cc @alice` 和换行分隔的
完整指令有效；`claim 是什么意思？`、`cc @alice about this`、`review 一下` 和混入散文的
指令块无效。改用 `review` 或显式 `@bot review 一下`。代码、引用和 bot 输出不作为指令来源。
显式 @ 及既有 `r?`/`?r` 快捷入口保留独立的位置规则。组合/去重前逐候选检查：被允许的
`ready` 不会放行旁边被阻止的 `cc` 或审批。

`review`、`codeql`、`r+`、`r-` 仅适用于 PR。Issue 上的 `r? @alice` 是指派；PR 上是请求审阅。
提及模式不授予仓库权限。`r+` **和 `r-`** 都实时检查 write/maintain/admin 权限；代他人审批还
检查被归功用户的权限。拒绝自我审批。权限被撤销或无法确认时，旧会话不能替代授权。

## 会话与动态帮助

升级后请使用部署的机器人名，新发有效的 `@bot help`：**不会导入旧评论中的唤醒**。
真实显式 @ 必须通过语法、适用性和授权预检才记录会话。`always_mention` 模式的显式指令不建立
可复用会话。裸指令、自动事件、bot 消息和未通过检查的指令不建立或续期会话。

会话范围是 installation + 仓库 + Issue/PR 线程 + GitHub 用户。
`[command_sessions] ttl_days = 30` 为默认值（允许 1–365 天），按 GitHub 源评论时间计算。
仅有效显式 @ 续期。编辑不唤醒，删除不撤销已记录证据，关闭/重开不刷新到期时间，重启不丢失。
TTL 同时按源顺序与执行时间检查；后发 @ 不能放行更早评论或同一评论中的裸指令。
过期后须重新 @。

help 展示 16 条指令的有效模式、别名、PR 限制，当前会话范围、剩余秒数和 UTC 到期时间
（或无效/不可用状态），以及独立的创建和路径规则部分。人数是配置数量，**不代表剩余通知预算
或成功投递**。help 不会 @ 配置中的 CC 用户。规则 ID/路径会转义并限长，完整名单请查看配置文件。
没有已验证快照的直接 handler 调用只给默认值参考，不冒充仓库有效策略。

## 缓存与故障处理

已验证快照按 installation/仓库缓存 **60 秒**，同仓库并发刷新合并处理。已验签的默认分支
Push 或观察到默认分支变化会立即失效；漏掉事件时在到期后重验证。配置按不可变 commit 读取，
不从 PR 获取。只有仓库和默认分支 ref 成功读取后的文件 404 才代表缺省；403/429/5xx、
网络错误和畸形响应均是故障。

过期/失效后刷新失败会阻断相关动作；旧快照**仅作参考**，不能执行。未知字段/类型或非法 TOML
拒绝整份文档；语义错误使对应域或规则失效，不影响独立有效域。引用禁用指令的自动规则无效，
包括路径 `labels`/`cc`；组合规则的任一指令禁用会拒绝整条规则。其他手动提及模式不授权或移除
自动订阅。配置修复不会回填已拒绝/冻结的旧事件。

显式 `@bot help` / `@bot ping` 可返回最小诊断：稳定原因和默认分支修复位置，不回显 TOML 值、
workflow inputs、凭据或私有 API 错误正文。配置故障时，同条评论的组合动作不执行。
同 installation/仓库/线程/原因的诊断在**进程内每 10 分钟最多一次**；持久写入回执也防止同一
源评论在重启后重复投递。重启会重置新源评论的内存限频窗口。自动故障写入日志和状态 CLI，
不重复发送通知评论。入口存储故障返回 HTTP 503，不执行动作；服务必须有可写持久卷。

## 自动创建与路径动作

创建订阅接受 `issues.opened` 和 `pull_request.opened`，包括草稿 PR。仅支持 PR 的
`review`/`codeql` 和 Issue/PR 的静态 `label.add`，没有自动审批、自动指派或通用命令执行器。
即使模型建议批准，AI 审查也**只发布 COMMENT**，不能 APPROVE 或入队。标签须已存在；禁止
queued/testing 控制标签和非空 `CODEQL_LABEL`，相应功能关闭时也不例外。

路径规则只在 PR `opened`/`synchronize` 执行。普通 Issue 没有 diff，不从散文或关联 PR 推导路径。
匹配使用**当前完整 base/head diff**，不是最近一次 push 的增量。以仓库根为基准的 `/` 路径区分
大小写：`*`/`?` 匹配单个路径段内字符，独立的 `**` 匹配零个或多个路径段。
某个路径命中任一 include 且没有 exclude 命中即有效；改名的新旧路径都参与判断
（排除其中一个名字不会排除另一个名字）。删除使用旧文件名；生成和二进制文件默认参与，可显式
排除。不支持绝对路径、反斜杠、`.`/`..`、brace expansion、字符类和否定语法。

API 文件列表完整分页后核对 `changed_files`、文件名唯一性、状态及稳定的 base/head。
缺失、截断、畸形列表、超过 GitHub **3000 文件上限**或持续变化的快照都**不执行部分路径动作**。
匹配标签取已有标签并集，不移除或创建标签。组合标签/CC 规则中任何配置标签不存在时整条跳过；
标签 API 与通知 API 的结果分别记录。

CC 是显式个人 login 名单（每条规则最多 32 人，不带 `@`、团队或推导的所有者）。核验被选中的
账号，排除本 App。login 大小写规范化后跨规则去重，按字典序选择，一条聚合评论仅提及本次新预占
的人。超额名字在公开正文中**只计数**；全部被抑制时不发评论。ID/路径样例限长并转义，包含 `@`，
避免额外提及。

`max_cc_users_per_pr` 是每个 installation/仓库/PR **终身**累计不同接收人预算，允许 0–10
（默认 10），跨规则和 push 共享，0 只关闭 CC。手动 CC 独立。重复 push、配置/规则变化、删评论、
关闭/重开和重启不重置预算。调低上限后，已用额度达到上限就不再通知新人。SQLite 事务原子预占人和
名额，未知发送结果保留名额；确认未发送才能释放未发送预占。账号不存在、非个人或已改名会使整条
未发送聚合通知失败；修正配置后由后续支持的快照触发。发送前网络故障仍可重试。GitHub 接受评论
不代表个人通知设置下的最终送达。

## 部署与恢复

订阅 **Issue comment** 接收 created 评论指令；**Issues** 接收创建 Issue 规则；
**Pull request** 接收 opened/synchronize 路径和创建规则，以及既有 rebase/标签/关单处理；
**Push** 用于及时失效默认分支配置及 rebase/队列更新。原生审查驱动的合并队列还需订阅
**Pull request review**。现有 rebase、CodeQL 标签、合并队列和 idle 保留各自开关和路由，
本功能不重复路由。本 App 创建的推进 PR 按已核验的 App 来源过滤。

| 启用的功能 | GitHub App 仓库权限 |
| --- | --- |
| 读取默认分支策略及代码 | Contents: read；Metadata: read（隐含） |
| Issue 指令 / 标签 / 指派 / CC | Issues: write |
| PR 请求审阅、发布审查/报告、路径 PR 读取及 rebase 检查 | Pull requests: write；标签/评论还需 Issues: write |
| CodeQL 报告 | Code scanning alerts: read，加报告发布权限 |
| 合并队列 | Contents: write、Pull requests: write、Issues: write、Checks: read；保留既有分支保护/bypass 策略 |
| idle workflow 调度 | Actions: write、Contents: read、Pull requests: read；外部被监控仓库需 Actions: read |

按启用的功能授权，并在 installation 上批准权限变化。本期不增加组织成员、团队或 CODEOWNERS 权限。

使用持久化 `XERO_DATA_DIR`（Compose 挂载 `/data`）和**单进程**。排他锁拒绝第二个数据库所有者，
不同卷之间没有协调。停止服务后备份数据库**及 WAL** / 完整持久卷；恢复完整备份，不只恢复部分
接收人表。丢数据会丢失会话、回执、去重和未知写入证据，删除 SQLite 不是恢复手段。
首次启用功能时记录账本起始时间；不晚于该时间创建的 PR 会阻断自动 CC，直到恢复完整账本，标签
仍可执行。这同时覆盖首次升级和丢卷重建。只有在完整审计（包含可能发送和已删除评论）后，才可用
`restore-notification-ledger` 导入证据；只看可见评论不够。

未知的非幂等写入通过 marker、已核验 App 身份及原正文核对，不盲目重发。停止服务后使用：

```sh
trigger-state /data list
trigger-state /data inbox
trigger-state /data show SHA256_FROM_MARKER
trigger-state /data confirm-success SHA256_FROM_MARKER '已核验远端结果与 marker'
trigger-state /data confirm-not-sent SHA256_FROM_MARKER '证明没有请求到达 GitHub 的证据'
trigger-state /data retry SHA256_FROM_MARKER '已修正确定失败的请求'
```

`retry` 仅接受 `failed`；unknown 必须有证据确认。先核对子写入再处理父动作。人工决定会被审计，
不重置终身预算。标签 ensure 可在当前策略下核对后幂等重试。策略变化不取消已经开始的计算，
紧急停止须停服务并检查不确定写入。**GitHub 与 SQLite 不共享事务，不承诺 exactly-once。**
完整操作参见[状态运维说明](trigger-state.md)和 [#18 验收矩阵](issue-18-acceptance.md)。
