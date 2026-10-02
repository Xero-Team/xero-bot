# Issue #13 实现与验收

验收日期：2026-10-02。依据：[父 issue #10](https://github.com/Xero-Team/xero-bot/issues/10)
与 [状态存储 issue #13](https://github.com/Xero-Team/xero-bot/issues/13)。

## 交付内容

- `src/trigger_state/`：独立于 idle 开关的 SQLite 持久化 inbox、动作状态、事务领取、尝试编号隔离、业务去重键、恢复核对、会话记录和通知预算。使用 `WAL`、`synchronous=FULL` 与进程生命周期排他锁，同卷第二实例启动失败。
- webhook 验签后，先持久化最小恢复上下文再确认接受。缺少必需源信息返回 400，写库失败返回 503；评论执行不再依赖仅驻内存的 `tokio::spawn`。
- 手动指令按评论 ID + 规范指令/参数去重；指令内每个 GitHub 写入另存结果，成功部分不随失败部分重发。配置不可用保留 inbox，恢复进入当前配置、适用性及既有授权检查；旧 head/base 的未发送任务可标为 `superseded`。
- `running` 遗留先进入 `unknown`。评论/review 使用稳定 marker，并核验 App ID、Bot 类型与 App login；找不到确认结果即暂停，不把缺失 marker 当作未发送证据。标签确保存在支持核对后幂等重试；发送意图未落库的请求可证明未发出。
- 通知动作领取与跨规则总预算预占为同一事务；最多 10 人，可降至 0。未知结果保留占用；成功提交，明确未发送才释放。成功/未知记录和接收人不按 scheduler 的 7 天规则清理。
- 会话存储以 GitHub 用户 ID、仓库/线程及真实 @ 源评论 ID 为依据，提供按 GitHub 源时间及评论 ID 排序的 TTL 查询接口。
- `trigger-state` 离线管理 CLI：查询动作/inbox、人工确认成功/未发送、重试明确失败；记录证据和审计，未知写入不能直接 `retry`。确认指派成功需要真实 assignees 响应，避免恢复时错误宣称未指派。Docker 镜像包含管理命令。

## 验收矩阵

| 要求 | 实际验证 |
| --- | --- |
| 相同/不同 delivery 并发不重复领取 | 16 个线程、4 个 delivery 争用同一业务键，只有一个动作领取成功；每条 inbox 只有一个领取者；同 delivery 异内容被拒绝 |
| 同卷排他所有权、进程重启 | 真实子进程持锁，第二个 `trigger-state` 进程以 2 退出并报告 locked；SIGKILL 后可重新取得所有权 |
| 入库前、入库后、领取后、发送意图后崩溃 | 子进程用无析构退出注入各阶段；重启断言 inbox/动作状态与发送证据，不把 unknown 直接重新领取 |
| 在途 HTTP 被取消 | wiremock 确认收到 POST、延迟响应时取消执行；重启仍 unknown，核对结果缺失时 POST 总数保持 1 |
| 远端成功、本地提交失败 | 实际 POST 返回 201，同时 SQLite 故障触发器拒绝成功回执；重启后通过真实 App marker 核对，未产生第二次 POST |
| 用户伪造 marker 无效 | 人类账号、其他 App、缺少 App 作者身份均不能确认；评论与 review 的合法 App marker 均能确认成功 |
| 标签幂等重试与评论暂停分离 | 标签先 GET 核对，缺失进入有退避的 pending，已存在直接成功；评论不存在时保持 unknown，不发第二次 POST |
| 部分失败不重做成功部分 | `ping; cc` 的 cc 503 不重跑 ping；添加标签成功、删除标签未知后人工确认未发送，仅重试删除，添加 POST 总数为 1 |
| 恢复重新检查配置、权限和快照 | 配置 503 的 inbox 保留 pending；恢复后 ping disabled 不执行；r+ 恢复重新读当前 read 权限并拒绝审批；旧 head 任务 superseded 且无写入 |
| 预算预占/提交/释放事务一致 | 8 个并发规则共享最多 10 个接收人；大小写名单去重；0/非法 11 预算；未知保留、明确未发释放、成功永久占用；中途写库故障回滚整个领取和预算事务 |
| 通知状态不随重启/新 head 重置 | 已预占 unknown 通知重启后仍占预算，新 head 无法复用；已发送动作不能直接 superseded 释放预算 |
| 会话源时间、作用域与 TTL | 用户/仓库/线程隔离、后发 @ 不放行旧调用、相同时间按源评论 ID 排序、30 天边界、重复记录不续期、重启保留 |
| 未启用 idle 仍可运行 | 实际评论执行和 webhook 入口测试均设置 idle=false，成功记录可重启恢复 |
| 存储故障 fail-closed | inbox INSERT 故障返回 503，不声明 accepted；动作 INSERT 失败即使尚无动作记录也保留 inbox；发送意图 UPDATE 故障时，外部写入数为 0 |
| 人工管理与迟到任务隔离 | 真实 CLI 查询/人工确认；无证据拒绝、unknown 直接 retry 拒绝、确认指派需回执；旧尝试不能完成新尝试 |

测试分别位于 `src/trigger_state/tests.rs`、`tests/trigger_state_process.rs` 和
`src/main.rs` 的入口验收模块；HTTP 使用本地 wiremock，不向真实 GitHub 发布评论、审批或标签。

## 验证命令

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```

以上检查全部通过：**424 个测试通过，0 失败、0 忽略**。其中新增 44 项测试（36 项状态/HTTP 验收、4 项 webhook 入口验收、4 项进程/CLI 验收，含子进程测试入口）。fmt、clippy、rustdoc（`-D warnings`）及 `git diff --check` 均通过。

## 运维与范围边界

部署须保留 `XERO_DATA_DIR/command-triggers.sqlite` 及 WAL；单实例使用持久卷。
停止服务后使用 `trigger-state /data list`、`inbox`、`show <marker中的SHA256>` 查询。
`confirm-success` / `confirm-not-sent` 必须给出外部证据；`retry` 只接受明确失败。
完整命令、Docker 操作步骤与恢复契约见 [trigger-state.md](trigger-state.md)。

SQLite 与 GitHub 不能组成事务，**不承诺跨崩溃 exactly-once**。未知非幂等结果优先暂停，
这会牺牲自动补偿能力；无法同时保证绝不重复和绝不遗漏。不同持久卷之间不提供去重。

本项接入已有评论执行的持久化，同时保存创建/路径事件的恢复信封，未新增自动动作。
会话持久化接口已交付，真实 @ 的授权预检、会话生命周期接入和 r- 的 write+ 补齐仍属于 #14；
创建规则、路径匹配和聚合通知消费者分别由 #15–#17 接入。通知消费者只能发送实际预占名单，
名单为空必须跳过发送。rebase、CodeQL 标签事件、原生 review 合并队列及 idle scheduler
继续走既有开关和路由。本验收只对应 #13，不能据此宣称父 issue #10 已整体完成。


## PR #23 审查修复与全量复审

| 问题 | 修复与回归证据 |
| --- | --- |
| [DELETE 204 被 JSON 反序列化误判](https://github.com/Xero-Team/xero-bot/pull/23#discussion_r4162637736) | 在 Octocrab 0.44.1 复现 EOF 错误；无请求体 DELETE 校验原始响应状态，204 不再进入 unknown；403/404/503 状态保留，指派删除仍读取响应 |
| [长 review 阻塞后续命令](https://github.com/Xero-Team/xero-bot/pull/23#discussion_r4162637743) | 最多 8 个并发 worker，持续填补空位；长任务未结束时，后来到达的短任务已完成；20 个重复 delivery 实际只 POST 一次 |
| 一个 worker 结束影响其他领取 | 恢复按当前 lease_delivery 隔离，健康任务不受影响；数据库 v1→v2 事务迁移保留证据 |
| 原始 delivery 与当前领取者不一致 | 独立保留当前领取者，重领后恢复只作用于新 owner，原始 delivery 留作审计 |
| 旧未发送证据放行新一次未知写入 | 核对提交校验尝试编号及 unknown 状态，旧证据不能将新一轮未知结果改回 pending |
| 普通评论永久入库、查询无限增长 | 路由过滤散文和 bot 自回复；只清理超过 30 天且无未完成/失败关联动作的 succeeded inbox，每批最多 500；CLI 每页 100；永久动作和通知账本保留 |
| 删除不存在标签错误记为失败 | 已用回归复现；标签移除的明确 404 记为成功无操作，与既有语义一致；其他 DELETE 错误不改变 |
| 函数契约说明不足 | 补齐持久化接口、执行边界、管理入口及测试意图的 rustdoc 注释 |

复审覆盖全部实现、部署配置、CLI、文档与测试，重点核查验签/入库顺序、
任务作用域、事务与状态迁移、重启及取消、marker 身份、权限/配置/快照、
通知预算与永久去重、会话源时间、存储升级和独立旧路由。修复后未发现新增阻塞问题。
新增复审回归位于 `src/trigger_state/review_tests.rs` 和 webhook 入口测试。

复审新增 13 项回归；全仓库 424 项测试通过，0 失败、0 忽略。格式、clippy、rustdoc 及 diff 检查全部通过。
