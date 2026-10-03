# Issue #16 实现与验收

本项实现父 issue #10 的 PR 路径匹配与自动标签部分。路径通知名单和总人数预算保留给 #17；配置 `cc` 时会报告 `Unsupported`，不会产生副作用。

## 行为

- 只处理 `pull_request.opened` 和 `pull_request.synchronize`，普通 Issue、评论、`reopened` 和配置变化都不会触发路径扫描。
- 规则以仓库根为基准，路径使用 `/`、区分大小写；支持段内 `*`、`?` 和独立路径段 `**`。绝对路径、反斜杠、`..`、反向规则、brace expansion、字符类和空段均拒绝。
- `include` 任一命中且没有 `exclude` 命中才算规则命中。删除使用 `filename`，改名同时检查 `previous_filename` 与 `filename`；改名缺少旧路径时整轮失败。
- 读取完整分页文件列表，并核对 PR 的 `changed_files`、文件名唯一性、状态和当前 base/head。超过 GitHub 文件上限、分页不完整、字段缺失、权限/API 失败或读取期间 PR 变化时不执行部分结果；变化最多立即重取两次。
- 命中规则的标签取并集，只确认仓库中已有的标签并添加到 PR。不会创建标签、删除标签、修改人工标签，或触碰 merge queue 的 `queued/testing` 与 `CODEQL_LABEL`。
- 同一 PR、事件和 base/head 快照的重复 delivery 共用持久化计划和操作记录，不会重复写入。计划在执行前重新校验原规则；规则被删除、禁用、改动或控制标签冲突时作废。

## 验收证据

路径匹配单元测试覆盖根目录与嵌套目录、大小写、中文路径、`*`/`?`/`**`、排除规则、删除和改名。触发器验收覆盖完整 diff、跨规则标签并集、重复 delivery 去重、改名缺旧路径、数量不符、保留标签和 API 失败；wiremock 不访问真实仓库。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```
