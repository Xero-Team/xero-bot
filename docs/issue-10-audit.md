# Issue #10 实现审计（2026-10-05）

审计基线：`0366cbe64a675a12b026558f1676b9f769f9c907`（PR #28 合并后的 main）。
读取父 Issue、GitHub 原生子任务关系、全部子任务正文、关联 PR 正文及审查讨论，再对照合并后的执行链和测试。
原生子任务共 8 个，#11–#18 均没有更深层子任务。

## 覆盖范围

| 子任务 | 已合并 PR | 审计重点 |
| --- | --- | --- |
| [#11](https://github.com/Xero-Team/xero-bot/issues/11) | [#21](https://github.com/Xero-Team/xero-bot/pull/21) | 默认分支配置、60 秒缓存、失效/失败、disabled 和别名 |
| [#12](https://github.com/Xero-Team/xero-bot/issues/12) | [#22](https://github.com/Xero-Team/xero-bot/pull/22) | 候选来源、裸语法、屏蔽区域、审批参数、组合解析 |
| [#13](https://github.com/Xero-Team/xero-bot/issues/13) | [#23](https://github.com/Xero-Team/xero-bot/pull/23) | inbox/原子领取、独立写入回执、未知结果、恢复和持久化 |
| [#14](https://github.com/Xero-Team/xero-bot/issues/14) | [#24](https://github.com/Xero-Team/xero-bot/pull/24) | 会话、当前权限、模式收紧、审批/队列旁路 |
| [#15](https://github.com/Xero-Team/xero-bot/issues/15) | [#25](https://github.com/Xero-Team/xero-bot/pull/25) | 自动动作白名单、COMMENT 发布、App 来源与规则撤销 |
| [#16](https://github.com/Xero-Team/xero-bot/issues/16) | [#26](https://github.com/Xero-Team/xero-bot/pull/26) | 完整 diff、glob、base/head 检查、标签发布与恢复 |
| [#17](https://github.com/Xero-Team/xero-bot/issues/17) | [#27](https://github.com/Xero-Team/xero-bot/pull/27) | 聚合 CC、installation 隔离、终身预算、未知结果和丢卷恢复 |
| [#18](https://github.com/Xero-Team/xero-bot/issues/18) | [#28](https://github.com/Xero-Team/xero-bot/pull/28) | 动态帮助、双语契约、跨功能配置失效与验收 |

同时核对 #21 提到的前置 PR #20，保留既有队列串行化、重复审批和 CI 状态回归。
#11 早期正文允许路径静态动作独立于 disabled；采用父 #10 最新契约和 #28 已合入的统一禁用语义。
未修改上述 Issue 的完成状态。

## 发现及修复

| 编号 | 优先级 | 触发条件与影响 | 修复 |
| --- | --- | --- | --- |
| A1 | P1 | 部署使用 `queued` 等可解析的自定义队列标签时，普通 `label`/`relabel` 能借用 App 权限添加/移除控制标签，绕过 `r+`/`r-` 的权限和 disabled 门槛。 | 候选授权及直接 handler 均拒绝队列控制标签，大小写归一，混合请求整条拒绝；要求通过审批/撤回入口操作。 |
| A2 | P1 | 同一评论前面的命令耗时超过缓存窗口，后续命令（含审批）仍持有旧策略；原代码只为 help 重载配置，其他会话调用还使用旧 TTL。 | 每条命令开始前重载当前配置，并用保留的显式提及证据、当前模式及 TTL 重新判定；失败阻断，持久记录采用当前配置 SHA。 |
| A3 | P2 | 路径标签清单或 PR 快照读取期间配置已失效，标签操作继续依据之前的 `compatible` 判断写入。 | 在这些读取之后、领取/发送之前重新验证路径策略；撤销计划持久标记 superseded，配置读取故障保留可重试状态。 |
| A4 | P1 | 隐藏 HTML 注释里的显式或符号命令仍被解析，可能在有权限用户复制的评论中执行未显示的动作。原有审批权限检查仍适用。 | 屏蔽 HTML 注释（含未闭合注释），保持 UTF-8 字节偏移和换行；不能删除隐藏区域后制造合法裸指令块。 |
| A5 | P2 | 只忽略本 App 评论，第三方审查/CI bot 引用的命令可被执行，形成误触发或机器人互相响应。 | 所有 GitHub `Bot` 作者评论在持久化前忽略；同名人类账户仍按正常策略处理。自动 PR 来源判定保持原有规则。 |
| A6 | P1 | 公开的审查 marker 单独被当作作者凭据，贡献者可伪造“机器人历史审查”；原来的 login 归一还混同了同名人类账户和 App。 | 要求 `Bot` 类型和精确的 `slug[bot]` 登录名；审查仅取 COMMENTED，普通评论还须带报告 marker，排除审批、help、CC 等输出。身份未知时不采纳。该修复防止上下文污染，不赋予 AI 审批能力。 |
| A7 | P2 | 审查首次 422 后移除 inline 重试，第二次遇到 429/5xx 时仍降级发普通评论。持久层之外的调用可产生重复报告并误报成功。 | 第二次不确定失败原样返回；仅明确的 403/404/422 拒绝允许降级，保持无盲目重发语义。 |
| A8 | P2 | `r?` 请求审阅成功、后续指派中断后，持久回执没有保留 `requested_reviewers`，恢复时误报“未列为 reviewer”。 | 在最小回执中仅保留确认的 reviewer login；恢复既不重复请求，也不丢失成功结果。 |

## 可复现证据

将下列 8 个 `audit_` 回归保留、执行代码回退到上述原始基线，运行
`cargo test --locked --all-targets --no-fail-fast audit_`：**8 项全部失败**，分别重现上表问题。
恢复修复后均通过。另补充伪造评论 marker 和直接 label handler 的回归，共新增 10 项测试。

- A1：`src/dispatch/config_tests.rs::audit_manual_labels_cannot_bypass_approval_authority`；
  `tests/approve_permissions.rs::audit_direct_label_handler_refuses_queue_controls`。
- A2：`src/dispatch/config_tests.rs::audit_all_sibling_commands_revalidate_policy`：
  可控时钟模拟前置动作跨越 60 秒，覆盖 disabled、always_mention、mention_once、故障及合法显式调用。
- A3：`src/trigger_state/path_review_tests.rs::audit_path_labels_recheck_policy_after_inventory`：
  在清单读取中注入缓存失效，验证禁用和 503 均不写入。
- A4：`tests/command_parser.rs::audit_html_comments_cannot_inject_commands`：隐藏、未闭合、符号指令、
  裸指令拼接、后续合法指令及原文 span。
- A5：`src/main.rs::audit_third_party_bot_comments_do_not_enter_inbox`：验签后的生产 ingress，
  无提及、显式提及和审批均不入库。
- A6：`tests/app_identity.rs::audit_review_context_requires_bot_identity_and_report_kind` 及
  `own_previous_reviews_ignores_forged_marker`：伪造评论/审查、同名人类、第三方 bot、空身份、审批和普通 bot 评论。
- A7：`tests/result_honesty.rs::audit_inline_fallback_stops_after_uncertain_second_review`：
  第二次 429/500/502/503 均不再发普通评论。
- A8：`src/trigger_state/review_tests.rs::audit_reviewer_receipt_survives_partial_command_recovery`：
  真实 SQLite 操作恢复，只请求一次 reviewer，最终报告仍准确，回执不保留多余用户数据。

## 最终验收

- `cargo fmt --all -- --check`：通过。
- `cargo clippy --locked --all-targets -- -D warnings`：通过。
- `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`：通过。
- `cargo test --locked --all-targets --no-fail-fast`：**532 通过，0 失败、0 忽略**。
- `git diff --check`：通过。

测试使用 Wiremock、可控时钟和临时 SQLite，没有向真实仓库发批量测试通知、审批或付费 AI 请求。
原有配置、解析、会话、自动白名单、路径完整性、通知预算/迁移/恢复、进程崩溃和旧流程测试继续通过。
单实例持久卷、未知写入暂停以及最后一次远端读取与写入之间的竞态边界继续适用；已开始的计算不在线撤销。
本审计没有验证实际部署的 GitHub App 权限、网络环境或运行时机密配置，不宣称形式化或穷尽式安全证明。
