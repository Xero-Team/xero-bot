# Issue #15 实现与验收

验收日期：2026-10-03。依据：[父 issue #10](https://github.com/Xero-Team/xero-bot/issues/10)
和 [issue #15](https://github.com/Xero-Team/xero-bot/issues/15)。

## 使用方式与范围

配置只读取目标仓库默认分支的 `.github/xero-bot.toml`，复用已有 60 秒快照、
默认分支变更失效和读取失败阻断行为。GitHub App 需订阅 Issues / Pull requests
事件，并具备相应 Issue/PR 写权限；AI/CodeQL 仍需原有部署配置与读取权限。

```toml
[[event_triggers]]
id = "review-on-open"
event = "pull_request.opened"
command = "review"

[[event_triggers]]
id = "issue-triage-on-open"
event = "issues.opened"
command = "label"
add = ["needs-triage"]
```

- 默认规则为空，最多 32 条，ID 非空且唯一。只响应 `opened`，包括草稿 PR；
  `edited`、`reopened`、`synchronize` 不重新触发创建规则。
- 仅允许 PR 的 `review` / `codeql` 和 Issue/PR 的静态 `label.add`。
  参数按规范动作合并；标签名称大小写、顺序和重复项规范化，相同动作只运行一次，
  保留全部命中规则 ID。`relabel` 与 `label` 共用规范动作。
- 审批及其他非白名单命令在共享配置类型校验时拒绝；引用 `disabled` 命令也拒绝。
  自动订阅不改动手动模式、不调用人类审批处理器、不建立或续期用户会话。
- 标签列表必须非空且全部已存在；任一不存在则整条标签动作失败，不部分添加。
  不创建/删除仓库标签。部署配置的 queued/testing 控制标签及非空 `CODEQL_LABEL`
  在计划生成前拒绝，即使对应旧功能开关暂时关闭也不能使用这些标签。
- 自动 AI 审查复用既有 per-PR 互斥和语言选择，模型 approve 结论仍只产生 COMMENT；
  发布边界再检查 COMMENT 并设置实际 `commit_id`，不进入审批或合并队列。
- 本 App 的创建来源通过 App ID 或实际 Bot 用户 ID 核验；正文 marker、相似 login
  和第三方正常 bot PR 均不会直接被当作本 App 输出。

## 去重、恢复与审计

验签后的创建事件先进入持久化 inbox 再确认接收。首次成功读取的创建计划通过 SQLite
事务固定，空规则也记录空计划；相同/不同 delivery、重启和 inbox 清理不会重建计划，
配置变化不会回填旧线程。每组动作以仓库、线程和排序后首个稳定 rule ID 为业务键，
其余合并规则 ID 全部保留在动作记录内。

已确认的 TOML 结构错误或 events 域错误（如重复 ID、超过 32 条）会记录拒绝并固定
空计划，inbox 可正常完成和清理；修复配置不会补跑这些旧线程。网络、权限、限流及
不完整 API 响应仍保留重试。已有计划遇到无效策略则永久撤销，撤销与未发送子动作的
作废在同一事务中提交，且先于可能失败的远端核对。

每项动作执行或恢复前重新读取可用的当前配置；只要至少一个原始规则仍授权同一规范
动作和参数，该合并动作才可继续。所有授权规则被移除、禁用或改变后，计划标为
`superseded`，以后恢复旧配置也不会重放。耗时审查之后的下一项动作同样重验证，
不会沿用已过期的配置。一个动作暂停不妨碍其他独立动作完成。

动作记录保留原计划配置 SHA、执行配置 SHA、创建事件/审计用户和实际 PR head/base。
报告正文标明事件、规则 ID、配置和实际 head。创建后 head 前进时，首次执行可采用
当前 head；API 审查读取固定 base/head 的 compare diff，子进程 checkout 必须匹配
已确认的 head/base。CodeQL 文件列表需完整且文件名非空、不重复，读取期间 head/base
变化则报告失败。

自动动作通过独立接口省略进度和引擎回退提示，最终评论意图显式标记为报告；旧的未分类
评论回执不能证明完成，尚有 pending 子动作也不能宣称完成。计算开始前记录状态；
中断后先核对独立 GitHub 写入回执，只有实际 App 的 marker 能确认结果。结果不明时
保留 `unknown` 并告警，不盲目发起第二轮 AI 或重复报告；计算已开始但没有可确认报告
也需人工核对。标签可在远端状态核对后按当前兼容规则幂等重试。
参见 [trigger-state 运维说明](trigger-state.md)。

SQLite 与 GitHub 不支持跨系统原子提交；上述策略不承诺 exactly-once，无法确认时
可能暂停待人工处理。状态卷须持久化，使用现有单实例所有权机制。

## 验收矩阵

| 要求 | 验证 |
| --- | --- |
| 默认零动作、两种创建事件和草稿 | 空规则无写入；Issue 与草稿 PR 的标签正例；非创建事件不执行；新增配置不回填 |
| 白名单、别名、禁用和规则限制 | `r+`、`r-`、`r=`、带审批参数、claim/take、assign、cc、ban 拒绝；Issue review/codeql 拒绝；disabled/relabel 双向覆盖；32/33 条边界、空/重复 ID、删除参数拒绝 |
| 不产生审批/合并 | mock AI 返回 approve；唯一 GitHub 写入为 COMMENT，实际 commit_id 为执行时新 head，无权限伪造查询、标签或队列写入 |
| 同动作多规则、并发与重启 | 不同 delivery 并发执行规范化同一标签动作，只 POST 一次；保留两个规则 ID；重启和相同 ID 参数改变不重跑 |
| 会话隔离 | 自动创建成功后查询同用户持久化会话仍为空；既有手动模式测试继续通过 |
| 安全标签 | 自定义控制标签与 CODEQL_LABEL 大小写变体拒绝；不存在标签不写入；CodeQL 正例只发布一份带审计信息的报告 |
| 自触发过滤 | 自己的 App ID / Bot ID 被过滤；其他 App、第三方 bot、人类和复制 marker 的正常 PR 可以执行 |
| 恢复时重新校验 | 未执行计划重启后执行一次；规则移除、同 ID 改动作、禁用后 superseded；未领取计划撤销后恢复配置不重放 |
| 响应丢失 | review POST 503 后重启；伪造作者回执无效，真实 App 回执可确认；期间无第二次 AI/报告请求；标签失联后核对采用或撤销，不重复 POST |
| 计算中断 | 已开始且无回执的 AI 动作保持 unknown，无第二轮 AI |
| 路由组合与入口 | Issues opened 入库并返回 persisted；edited/reopened 不入库；PR opened/reopened/synchronize 仍路由 rebase，旧 CodeQL 标签/关单路由保留 |

新增测试位于 `src/trigger_state/event_tests.rs`、`tests/repository_config.rs` 与
`src/main.rs`。HTTP 和 AI 均使用本地 wiremock，不向真实仓库写入，不调用真实付费模型。
既有缓存、持久化故障、merge queue、idle、授权和解析测试一并运行。

## 验证命令

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```

检查全部通过：**456 个测试通过，0 失败、0 忽略**。初次实现新增 14 项测试
（11 项自动事件/恢复、2 项入口/路由、1 项配置限制），本轮复审再新增 8 项，
并扩展已有白名单校验用例。
fmt、clippy（`-D warnings`）、rustdoc（`-D warnings`）及 diff 检查均通过。

本次交付仅验收 #15。路径匹配/通知和父 issue 的最终整体验收仍属于 #16–#18。

## PR #25 审查修复与全量复审

| 发现 | 修复与证据 |
| --- | --- |
| [CodeRabbit：events 域无效导致永久重试和配置修复后补跑](https://github.com/Xero-Team/xero-bot/pull/25#discussion_r4172075822) | 区分确定的策略拒绝与临时读取失败；覆盖重复 ID、33 条、无效 TOML、重启、30 天清理和不同 delivery；已有未领取计划也永久作废 |
| 规则撤销晚于网络核对，核对失败会丢失 veto | 先事务保存 superseded 与未发送子动作作废，再只读核对已发送结果；恢复规则后不重放，也不残留可执行 pending 子动作 |
| agent 回退提示可能被当成最终报告 | 进度/回退提示显式分流，自动动作不发布；新报告回执显式标记，旧未分类提示不能完成 planner；实际延迟 AI 请求并取消后仍 unknown，无第二轮 AI |
| 非 ASCII 控制标签可经不同大小写绕过 | 部署保留标签与动作使用同一 Unicode 小写规范化；分别验证 queued/testing/CODEQL 的 `队列/ÜBER` 变体，外部 POST 为 0 |
| CodeQL 文件列表只校验长度，缺名/重名可误报干净 | 校验非空文件名、唯一性和总数；缺名、空名及重名响应均返回失败报告 |

本轮按 PR 全部变更逐项复核：配置白名单/别名/禁用、默认分支缓存、webhook 验签及路由
组合、App 来源、计划冻结与原子领取、撤销/未知状态恢复、审查互斥、SHA 归属、COMMENT
发布边界、现有手动流程和文档。7 项负例先在修复前复现失败，再修复通过；另有 1 项
正例确认 403/429/503 配置读取失败仍可恢复。完整 456 项测试、fmt、clippy、rustdoc
和 diff 检查通过；本轮未再发现阻塞问题。

策略在动作开始或恢复前检查；已启动的计算不具备在线撤销功能，紧急停止需停止服务/
worker 并核对 unknown 写入后再恢复。此边界已同步到运维说明。
