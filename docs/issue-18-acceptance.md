# Issue #18 跨功能交付与验收

父 issue：[#10](https://github.com/Xero-Team/xero-bot/issues/10)。本项在已合入的
#11–#17 上交付动态帮助、双语升级/部署/恢复说明及跨功能验收。

## 本次交付

- help 从经过验证的默认分支快照生成 16 条手动模式、别名、PR-only 限制和部署代审批开关；
  会话显示范围、剩余秒数与 UTC 到期时间，或未确认/存储不可用。旧快照不作为有效配置展示。
- 自动创建、路径规则分开展示；部署保留标签的拒绝可见，规则 ID/路径转义并限长，CC 只显示人数，
  不会因查看帮助再次 @ 配置名单。不会回显 idle workflow inputs 或私有 API 错误正文。
- 前置指令耗时导致快照过期时，help 发布前再次刷新并检查自己的门槛；配置禁用或故障时仅诊断。
  过期参考诊断不会错误声称同条评论中先前完成的动作也没有执行。
- [README](../README.md) / [中文 README](../README.zh-CN.md) 和
  [英文指南](triggers.md) / [中文指南](triggers.zh-CN.md) 对应相同默认、语法、会话、缓存、
  路径、预算及恢复规则，补充 Issues Webhook 和按功能授予的 App 权限。
- [完整默认配置](../.github/xero-bot.toml)与
  [默认示例副本](../examples/repository-config.toml) 一致且自动规则为空；
  [显式启用示例](../examples/triggers-opt-in.toml) 独立存放，两类均做 TOML 和语义校验。

交叉检查发现并修复：旧路径规则刻意独立于全部手动模式，导致 `label`/`relabel` 或 `cc`
被设为 `disabled` 后仍可执行对应路径动作。现在只有其余三种提及模式保持独立；`disabled`
按父 issue 的最高优先级要求拒绝引用规则。混合规则任一动作禁用时整条拒绝，评论域无效也阻断
依赖规则。恢复时同样使旧计划失效，已确认/不确定发送的历史证据和终身预算不被清空。

## 验收矩阵

HTTP/AI 使用 Wiremock；会话/操作/预算使用临时 SQLite；配置 TTL 和帮助刷新使用可控时钟。
没有对真实 Issue/PR 发送批量测试通知，也没有用真实付费模型验收。

| #18 要求 | 证据与检查结果 |
| --- | --- |
| 无文件、空文件、idle-only、部分覆盖；默认零自动动作 | `trigger_state/acceptance_tests.rs::default_variants_run_issue_commands_show_effective_help_and_never_auto_act` 经生产 consumer 验证默认模式、Issue 指令、help 和两种 opened 事件；`repository_config.rs` 验证完整 16 项默认及示例 |
| 新用户 take/untake/cc/r?；散文不认领；PR/Issue 区分 | 上述端到端测试无唤醒执行四条指令、拒绝散文；`result_honesty.rs` 的 Issue 仅指派与 PR 双端点/独立结果；parser、dispatch 反例覆盖代码/引用和多余参数 |
| 四种模式、过期/重启/乱序、审批别名、权限撤销、r- write+ | `dispatch/config_tests.rs` 全别名禁用和低门槛组合测试；`trigger_state/acceptance_tests.rs` 重启、旧评论/他人隔离、TTL 收紧；`trigger_state/tests.rs` 会话范围/执行时到期、恢复时权限撤销；`approve_permissions.rs` 与 `integration.rs` 覆盖审批及拒绝权限 |
| disabled 从所有入口 fail-closed | `repository_config.rs::path_actions_cannot_bypass_disabled_commands_or_invalid_comment_policy`、`path_notification_tests.rs::disabled_path_actions_never_create_a_notification_or_label_request`；预占恢复测试增加 disabled 场景；原有评论别名、组合、CodeQL 标签与自动创建拒绝用例继续通过 |
| 创建白名单、控制标签、AI COMMENT 无批准/入队 | `trigger_state/event_tests.rs::auto_review_is_comment_only_and_pins_actual_head_without_session_or_queue` 用 approve 模型结果验证唯一 COMMENT；配置白名单、控制标签、禁用及事件恢复测试覆盖拒绝路径 |
| opened/synchronize 完整 diff、改名/删除/生成文件、大 PR | `path_triggers.rs` glob 语义；`trigger_state/event_tests.rs` 路径生产入口；`path_review_tests.rs` 分页/3000 上限、畸形数据/改名缺字段、连续快照变化；`path_notification_tests.rs` 删除/改名/生成文件通知复用完整列表 |
| 重复 push、多规则、重启、并发预算、不额外 @ | `path_notification_tests.rs` 终身去重、聚合排序、0/1/10/调低、剩余一名并发、抑制人数和安全正文；`notification_scope_tests.rs` installation 隔离、迁移和恢复 |
| 60 秒刷新、收紧后 API 失败、存储不可用、unknown、旧 head、配置禁用 | `repository_config_cache.rs` 精确 TTL/失效/失败和退避；新增 `dispatch/config_tests.rs` 过期参考、私有错误脱敏、前置指令后的刷新/禁用；`trigger_state/tests.rs` 写入前存储故障、远端成功本地失败、旧 head/禁用恢复；进程测试验证崩溃证据与排他锁 |
| idle/rebase/queue/CodeQL/self-reply/授权/结果真实性回归 | 原 `idle_workflows/tests.rs`、`integration.rs`、`merge_queue.rs`、`app_identity.rs`、`result_honesty.rs`、dispatch/main 路由用例整体执行；保持既有开关和路由，不增重复执行链 |
| 示例可解析；中英文 help/文档一致 | `repository_config.rs::shipped_defaults_and_opt_in_examples_are_valid_and_separate` 校验两类 fixture 及部署控制标签；`handlers/help.rs` 双语逐项检查 16 条模式/别名/范围、代审批开关、TTL、转义及长度；指南人工对照同一配置契约 |

上表中的测试路径以 `src/` 或 `tests/` 为根，原子领取、恢复与跨功能测试也直接复用
[#13](issue-13-acceptance.md)、[#14](issue-14-acceptance.md)、
[#15](issue-15-acceptance.md)、[#16](issue-16-acceptance.md)、[#17](issue-17-acceptance.md)
已有验收用例，不把 mock 通过宣传为真实 GitHub exactly-once。

## 已说明的运维边界

- 单实例持久卷；备份数据库及 WAL，停止服务后恢复完整状态。丢卷/首次升级时旧 PR 自动 CC
  需完整账本证明，不能只凭仍可见评论证明没人收到过通知。
- unknown 写入可能已成功，核对 marker、App 身份和正文后人工处置；明确未发送才允许重试。
  查询、人工确认和通知账本导入见[运维说明](trigger-state.md)，不承诺 GitHub/SQLite 原子提交。
- 缓存窗口内已验证配置可执行；失效后读取失败阻断。已开始的计算不在线撤销，紧急停止须停服务。
- 新源评论的诊断限频是进程内 10 分钟；重启不重复已有源评论的持久写入，但会重置新评论限频窗口。
- Webhook 权限/订阅需要部署者在 GitHub App 设置中按指南更新；本次交付没有修改生产 App。

## 检查命令

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```

2026-10-04 本地最终验收：**521 项测试通过，0 失败、0 忽略**；fmt、Clippy
（warnings-as-errors）、Rustdoc（warnings-as-errors）、diff 和新增文档的本地链接检查通过。
本次新增 11 项测试，并扩展原有通知预占恢复测试及示例默认测试。

## PR #28 review 与示例补充

- 修正英文 README 把 30 天写成固定会话时长的表述；中英文均说明默认 30 天，
  通过 `command_sessions.ttl_days` 可配置为 1–365 天。补齐新增辅助函数及验收用例的文档注释。
- 新增根目录 [example.toml](../example.toml)，集中说明 #10 已实现的手动策略、会话、
  创建/路径触发和 idle 共存配置。复制此文件保留默认手动行为，自动规则为空且 idle 关闭。
- 独立 opt-in 示例补全 PR CodeQL、PR 静态标签，覆盖所有创建事件白名单动作；
  现有 fixture 测试校验完整参考不会启用自动动作，以及显式启用示例的解析与语义。

## 完整 CI 管理示例补齐

2026-10-05 对照 main 的 `idle_workflows::config` 补齐 `example.toml` 中遗漏的实际配置块：
当前及关联仓库 monitors、tasks 的 workflow/branch、重试间隔/次数、run_events，以及
字符串/布尔/数字 inputs。默认保留 `enabled=false`；启用前需替换示例仓库、workflow 和 inputs。
部署级 idle 轮询和 merge queue 参数标明其 `.env.example` 配置位置，不作为未知 TOML 字段加入。

新增 `complete_reference_contains_valid_ci_monitors_tasks_and_dispatch_inputs`：先将原示例的
idle 开关打开复现 `InvalidIdle`，补齐后通过真实解析与语义校验，并校验所有任务输入与
`workflow_dispatch` 定义兼容；原有检查继续保证直接复制主示例时不会启用自动调度。

本轮完整验收：522 项测试通过，0 失败、0 忽略；fmt、Clippy、Rustdoc、diff 和文档本地链接检查通过。
