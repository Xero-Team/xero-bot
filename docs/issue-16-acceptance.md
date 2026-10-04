# Issue #16 实现与验收

本项实现父 issue #10 的 PR 路径匹配与自动标签部分。路径通知名单和总人数预算保留给 #17；配置 `cc` 时会报告 `Unsupported`，不会产生副作用。

## 行为

- 只处理 `pull_request.opened` 和 `pull_request.synchronize`，普通 Issue、评论、`reopened` 和配置变化都不会触发路径扫描。
- 规则以仓库根为基准，路径使用 `/`、区分大小写；支持段内 `*`、`?` 和独立路径段 `**`。绝对路径、反斜杠、`.`/`..` 路径段、NUL、反向规则、brace expansion、字符类和空段均拒绝。
- `include` 任一命中且没有 `exclude` 命中才算规则命中。删除使用 `filename`，改名同时检查 `previous_filename` 与 `filename`；改名缺少旧路径时整轮失败。
- 读取完整分页文件列表，并核对 PR 的 `changed_files`、文件名唯一性、状态和当前 base/head。超过 GitHub 文件上限、分页不完整、字段缺失、权限/API 失败或读取期间 PR 变化时不执行部分结果；变化最多立即重取两次。
- 命中规则的标签取并集，只确认仓库中已有的标签并添加到 PR。不会创建标签、删除标签、修改人工标签，或触碰 merge queue 的 `queued/testing` 与 `CODEQL_LABEL`。
- 同一 PR、事件和 base/head 快照的重复 delivery 共用持久化计划和操作记录，不会重复写入。计划在执行前重新校验原规则；规则被删除、禁用、改动或控制标签冲突时作废。
- 每个源事件先持久化订阅（包括空订阅）；读取完整 diff 前发生配置撤销也会保留拒绝记录。恢复配置或重放旧事件不会补加新规则。后续受支持事件可以建立新订阅。
- 新快照会作废旧计划，并先核对不确定的子写入。配置撤销在网络核对前持久化，核对失败、重启或恢复原配置均不会复活旧计划。
- 仓库标签清单读取失败保留重试；重试会将原始待发送标签集合收窄到仍存在的标签，避免重放已删除的标签。最终 SHA 校验位于清单分页读取之后；GitHub 读取与写入之间仍存在远端竞态。

## 验收证据

路径匹配单元测试覆盖根目录与嵌套目录、大小写、中文路径、`*`/`?`/`**`、排除规则、删除和改名。目录与段内星号均使用有界动态规划；小规模 Unicode 模式与 glob 库结果交叉验证，并覆盖多星号不匹配的最坏输入。

`src/trigger_state/path_review_tests.rs` 覆盖 Unicode 标签大小写、完整七种文件状态、未知状态拒绝、跨页与 3000 文件上限、重复文件、改名缺字段、API 失败、快照连续变化及写入前变化、空订阅、配置撤销与重启、不确定写入恢复、标签删除后的重试及控制标签隔离；wiremock 不访问真实仓库。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```
