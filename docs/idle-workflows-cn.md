# 空闲 workflow 调度

[English](idle-workflows.md) | [简体中文](idle-workflows-cn.md)

各仓库通过**默认分支上的 `.github/xero-bot.toml`** 显式启用此功能。
bot 等待开发活动进入空闲、相关 CI 结束后，再触发 workflow，并补触发遗漏的运行、
重试失败的运行。构建步骤、镜像标签和发布规则仍由仓库现有的 GitHub Actions workflow 负责。

## 启用

1. 为需要调度的仓库授予 App **Contents: read**、**Pull requests: read** 和
   **Actions: write** 权限。仅用于监控的关联仓库需要 **Actions: read** 权限。
   修改 App 权限后，在 GitHub 中批准安装权限更新。
2. 订阅 **Push** 和 **Pull request** Webhook。使用 GitHub 原生合并队列时，建议再订阅
   **Merge group**；xero-bot 的 staging 队列由 push 事件覆盖。无需订阅 Workflow run 事件。
3. 在 bot 部署配置中设置 `IDLE_WORKFLOWS_ENABLED=true`，并保留 `XERO_DATA_DIR` 持久卷。
   `IDLE_WORKFLOWS_POLL_INTERVAL_SECS` 默认为 `60`，最小为 `15`。
   Docker Compose 已将 `/data` 配置为持久存储。
4. 根据仓库实际情况修改 [TOML 示例](../examples/idle-workflows.toml)，将其作为
   `.github/xero-bot.toml` 合并到默认分支。目标 workflow 必须处于启用状态，
   且默认分支和目标分支上的定义都要支持 `workflow_dispatch`。

bot 每轮核对时重新读取规则。配置文件不存在或 `enabled = false` 时，关闭该仓库的调度。
TOML 无效、workflow 输入不正确、workflow 不存在、监控仓库不可访问或权限不足时，
暂停调度，并在日志中报告具体原因。规则发生变化后，重新等待完整的空闲时长。
`/cron` 也会执行核对，并返回 `idle_workflows` 摘要；它与内置循环共用同一把写入锁。

仓库规则使用 TOML。GitHub workflow 的定义仍使用 YAML；bot 会读取这些文件以校验
`workflow_dispatch` 和 inputs，不会修改文件。

## 配置等待对象与执行任务

| 配置项 | 含义 |
| --- | --- |
| `idle_workflows.enabled` | 显式启用开关，默认 `false`。 |
| `idle_workflows.idle_minutes` | 所需的开发活动空闲时长，默认 `30` 分钟。 |
| `monitors[].repository` | 关联仓库，格式为 `owner/repository`；省略时为当前仓库。App 必须能访问它。 |
| `monitors[].workflows` | 需要等待结束的 CI，支持 workflow 文件名、`.github/workflows/...` 路径或加引号的数字 ID。空列表表示全部。 |
| `tasks[].workflow` | 需要触发、重试的 workflow，支持文件名、workflow 路径或加引号的数字 ID。 |
| `tasks[].branch` | 目标分支，支持名称中包含 `/` 的分支。 |
| `tasks[].inputs` | workflow 声明的 dispatch 输入，使用 TOML 字符串、数字或布尔值。 |
| `tasks[].retry_interval_minutes` | 距离失败或上次请求的最短间隔，默认 `15` 分钟。 |
| `tasks[].max_retries` | 首次尝试之外的重试次数，默认 `2`，即总共最多三次尝试；`0` 表示不重试。 |
| `tasks[].run_events` | 哪些触发事件产生的运行可视为此任务的等价执行，默认 `["workflow_dispatch"]`，且必须保留该事件。 |

省略 `monitors` 时，等待当前仓库的全部 Actions。关联仓库会扩展监控范围，当前仓库始终在范围内。
添加一条不填写 `repository` 的 monitor，并指定 workflow 列表，可以缩小当前仓库的 CI 监控范围。
筛选针对所选 workflow 的全部运行，包括 PR、push、staging、合并队列和手动运行。
`workflows` 为空表示全部 CI。关联仓库的开发活动和所选 CI 一起参与空闲判定。

以 AstrBot 为例，monitor 可以列出 `coverage_test.yml`、`dashboard_ci.yml`、
`unit_tests.yml`、`linux-development.yml` 及其他相关开发检查。
task 可以填写 `publish-nightly.yml`，并设置 `branch = "master"`。
这些名称由配置指定，代码中没有内置假设。现有 cron 仍是独立的触发来源：
如果希望**所有**自动构建都遵守空闲调度，需要在 AstrBot 中移除或调整 cron。
该 workflow 的定时模式会选择昨天的提交，因此应保留 `run_events = ["workflow_dispatch"]`。

## 调度行为

- 分支 push，包括 force push，以及 PR 创建、代码更新、重新打开、合并和原生 merge group
  创建，都会重置计时。普通评论和 review 不会重置计时。没有变化的未关闭 PR 不会阻止触发。
- 轮询会比较分支 tip 和未关闭 PR 的 head，以发现遗漏的状态变化；不会使用受评论影响的
  `updated_at` 时间戳。重启、配置变化或活动观测中断后，重新等待完整的空闲时长，
  因为无法确认未观测期间是否一直空闲。
- 空闲时长满足后，所选 CI 还必须没有 queued、running、requested、pending 或 waiting
  状态的运行。等待人工批准也会阻塞。已经结束但失败的 CI 视为已结束。
  API 读取失败或结果不完整时，不会判为空闲。
- 只调度目标分支的最新 SHA，多次更新会合并为一个待执行任务。成功记录在重启后仍保留。
  同一 workflow、分支、SHA 已有排队或运行中的执行时，不会再次触发。
  任务标识使用解析后的 workflow ID。
- 失败、超时和启动失败会通过重跑原 run 进行重试，保留原 SHA 和 inputs。
  重试必须同时满足空闲条件和重试间隔。分支前进后，停止为旧 SHA 安排新的重试。
  inputs 的变化应用于新的 dispatch；重跑仍使用原始 inputs。仅修改 inputs 不会重建已成功的 SHA。
- 任何取消都会阻止该 SHA 的自动重试。之后成功的手动运行仍可使任务完成。
  skipped、neutral 和 action-required 结果需要人工处理。`run_events` 范围内的历史运行、
  手动运行及其尝试次数，都会计入同一 SHA 的尝试上限。
- `run_events` 决定哪些已完成运行可视为等价执行。PR 检查或构建其他提交的 cron，
  不应使镜像构建被误判为已完成。只有在其他事件同样处理 `run.head_sha`、且 inputs 等价时，
  才应将它们加入列表。workflow 必须确保 `success` 代表预期工作确实完成。
- 开发活动恢复后，已经启动的后台构建会继续运行。本版本在触发时减少资源争用，
  不会抢占构建，也不会为之后到来的 PR 预留立即可用的并发容量。

## 持久化与结果不明的请求

调用 GitHub 前，bot 会先将触发意图和尝试次数提交到
`XERO_DATA_DIR/workflow-scheduler.sqlite`。dispatch 响应提供 run ID 时，会将其保存。
响应丢失、请求超时或进程重启后，bot 会先核对该 ID，或查询最近由 bot 触发的 dispatch。
如果分支在触发期间发生变化，会将结果记到 GitHub 实际运行的 SHA 上。

未确认的请求至少等待五分钟，让 Actions 的运行记录可见。
成功查询历史、仍未找到对应执行后，才允许再次尝试，并继续遵守空闲条件、重试间隔和次数上限。
结果不明的请求会预占一次尝试；明确被 HTTP 拒绝的请求不消耗次数。
GitHub 的 dispatch 不支持客户端幂等键：运行记录出现任意长的延迟，或外部同时发起 dispatch 时，
无法绝对保证仅执行一次。要求此保证的 workflow，应在发布前额外检查该 SHA 的产物是否已经存在。

请将 SQLite 文件及 WAL 一起保存在持久卷中。删除它们会丢失重试、取消和成功记录，
而 GitHub 可能已不再保留相应历史。同一数据库只允许一个调度进程占用；
第二个使用同一卷的进程会打开失败。不同持久卷副本之间不协调调度。无需部署外部数据库服务。

## 验证

测试使用可控时钟、临时 SQLite 数据库和模拟 GitHub API，覆盖空闲计时、workflow 筛选、
关联仓库、分页、读取失败、取消、重试上限、结果不明的 dispatch、分支推进、配置刷新和重启恢复，
不会触发真实 workflow。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```
