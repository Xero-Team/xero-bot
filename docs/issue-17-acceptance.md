# Issue #17 实现与验收

实现父 issue #10 的路径显式名单通知。复用 #16 的默认分支配置、完整 base/head diff、匹配结果及其快照校验；复用 #13 的 SQLite 操作领取、恢复和永久接收人账本。

## 使用方式

在默认分支 `.github/xero-bot.toml` 配置：

```toml
[path_triggers]
events = ["pull_request.opened", "pull_request.synchronize"]
max_cc_users_per_pr = 10

[[path_triggers.rules]]
id = "rust"
include = ["src/**/*.rs"]
exclude = ["src/generated/**"]
labels = ["area/rust"] # 必须已存在，也可以省略 labels 仅通知
cc = ["alice", "bob"]
```

每条规则最多 32 个个人 login，不带 `@`；统一校验、转小写、跨规则去重、按字典序选人。拒绝团队语法、非法用户名及任意文本，排除本 App，并对实际选中的接收人查询 GitHub 个人账号。不会读取 CODEOWNERS、git blame、组织或团队，也不会指派用户或请求 reviewer。

上限默认 10，只能配置 0–10。它限制 PR 生命周期累计不同接收人，所有规则共用；0 只关闭通知。手动 cc 不使用这个预算。规则重建/改 ID、配置或 head 变化、关闭重开、删除通知评论和进程重启均不重置账本。降低上限不会撤回评论，只会阻止超过新上限的新增通知。

## 发布与恢复

- 同 PR 路径计划串行处理。事务同时领取操作、排除已通知及未知结果的接收人、预占剩余额度，并保存超额的 `request.suppressed` 名单。
- 每个 base/head 快照最多一条聚合评论，正文仅 @ 本次新预占的人，显示命中规则、匹配路径示例和检查过的 head SHA。展示内容转义并限长，路径中的 `@` 转为全角字符；截断仅显示人数，不 @ 被抑制的人。全被抑制时只记录状态/日志。
- 发送前再次校验 head/base、当前规则、账号和上限。确认尚未发送的旧计划可作废并释放额度；标签与通知的成功/失败分别保存。组合标签/通知规则在任一配置标签不存在时整体跳过；标签 API 发帖失败不妨碍独立的通知结果。
- 发出请求前持久化 `sent` 边界。GitHub 接受后提交名额；明确拒绝才释放。超时、5xx、崩溃或远端成功但本地提交失败保留额度，核对稳定 marker、本 App 作者身份及原始正文；仍不明则暂停该计划。换 head 或 marker 无法再次选中它占用的接收人。
- “已通知”仅表示 GitHub 接受包含 mention 的评论，不保证个人订阅设置最终投递。账号错误、读取权限和写入失败会如实记录。
- GitHub 最后一次读取与写入间仍可能发生 push；本实现不承诺跨系统原子快照或 exactly-once。

## 状态丢失与旧 PR

首次启用本功能时，数据库记录永久的通知账本起始时间。PR 创建时间不晚于该时间时，自动 CC 保守阻断，标签继续。此规则同时覆盖首次升级、状态卷丢失和重新建库，避免把未知历史当作“没人收到过”。正常重启不改变起始时间。

优先恢复完整持久卷备份。如无法恢复，停止服务后，凭完整备份/审计记录核对该 PR 所有已通知或可能已通知的人，然后导入：

```sh
trigger-state /data restore-notification-ledger REPOSITORY_ID PR_DATABASE_ID '["alice","bob"]' '完整性依据，包括已删除评论和未知发送结果'
```

`PR_DATABASE_ID` 是 GitHub PR 的数据库 `id`，不是页面上的 `number`。导入只增加保护，不重置现有名单和名额；导入的历史接收人不能被旧的未发送计划恢复过程释放。空名单也必须有完整性依据。CLI 记录审计证据，不会自动判断证据可靠性；只查看当前可见评论无法排除历史评论删除。无法证明完整性时保持阻断并人工处理。单个不确定操作的核对命令见 [触发状态运维](trigger-state.md)。

## 验收证据

`src/trigger_state/path_notification_tests.rs` 通过生产路径处理器、真实 SQLite 事务和 Wiremock GitHub 接口验证：

| 验收项 | 覆盖 |
| --- | --- |
| 聚合与安全正文 | 多规则大小写重复、稳定排序、本 App 排除、恶意路径/规则 ID、抑制人数且不额外 @ |
| PR 生命周期 | 同 delivery、不同 delivery/head、配置修改与规则重建、关闭/重开、删评论、重启 |
| 人数预算 | 0/1/10、超额、上限降低、两次并发 push 共享剩余一个名额；未知结果不释放 |
| 崩溃与失联 | 预占后取消、请求超时、远端成功但 SQLite 提交失败、重启恢复、不盲目补发 |
| 标记核对 | 人类或其他 App 伪造 marker、本 App 不同正文、原始正文和正确 App 的确认 |
| 路径契约 | 删除、跨目录改名、生成文件、重复文件、缺少改名前路径、未知文件状态、发送前旧 head 作废 |
| 独立结果 | 标签 403/503 与通知成功分别记录、缺失配置标签规则无副作用、非法账号/其他 Bot/评论 403 不占已发送名额 |
| 持久状态丢失 | 旧 PR 阻断、标签继续、完整账本恢复、重复导入不删除历史、其他 PR 不继承证明 |

配置测试验证 login 校验、32/33 人边界、大小写去重和非法上限。#16 的完整分页、3000 文件上限、数量不符、API 错误、反复 base/head 变化及生成/二进制文件用例同时回归；通知复用同一次 `list_pr_files_complete` 和匹配输出，没有宽松匹配入口。#13 的原子事务回滚、排他进程所有权及通知预算恢复用例同时回归。

2026-10-04 本地验收：全量 497 项测试通过，格式、Clippy（warnings-as-errors）、Rustdoc（warnings-as-errors）及 diff 检查通过。

验收命令：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```

HTTP 验收通过本地模拟服务完成，不对真实仓库发送测试通知。
