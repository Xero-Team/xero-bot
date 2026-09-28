# Issue #11 实现与验收

验收日期：2026-09-28。依据：[父 issue #10](https://github.com/Xero-Team/xero-bot/issues/10)
及 [配置子任务 #11](https://github.com/Xero-Team/xero-bot/issues/11)。
代码基线：`db17367`；开发分支：`feat/issue-11-repository-config`。

## 实现范围

- `src/config/repository.rs`：共享顶层 TOML、16 个规范指令与别名、四种模式及默认值、1..365 天会话 TTL 契约、分域/分规则错误、自动动作白名单及暂不支持诊断。
- `src/config/cache.rs`、`src/github/repository_config.rs`：按 installation/repository ID 缓存，默认分支及不可变 commit 读取、blob SHA、60 秒 TTL、single-flight、304、push/分支切换失效、15 秒及 Retry-After 退避。
- 评论及现有 CodeQL 标签入口：配置错误与 `disabled` 优先拦截，阻止会话查询和业务动作；明确调用提供中英文诊断，help/ping 可展示配置状态。
- idle 调度复用共享读取器和严格校验；不再把仓库不可读视为“未配置”。新配置读取不依赖 `IDLE_WORKFLOWS_ENABLED`。
- 默认配置示例：[repository-config.toml](../examples/repository-config.toml)；接口与使用边界：[repository-config.md](repository-config.md)。

## 验收矩阵

| #11 验收项 | 证据 |
| --- | --- |
| 快照、TTL、single-flight、push、默认分支切换、304、退避 | `tests/repository_config_cache.rs`：可控时钟/API mock；40 个并发请求只执行一次刷新；新 commit 不复用旧 ETag；失效与请求并发时不发布旧快照、不丢失限流退避 |
| 缺文件、空文件、部分配置、禁用、别名冲突、错误模式、未知字段 | `tests/repository_config.rs`；文件 404、仓库 404、分支 404、403 分别验证；目录、无内容、错误编码、非法 UTF-8 均阻断 |
| disabled 无入口绕过 | 每个规范指令/别名测试显式、裸调用、已有/无会话、Issue/PR 的策略拒绝；生产入口 mock 覆盖当前可解析别名、组合命令、历史会话和 CodeQL 标签事件。断言除配置读取与诊断评论外无 API 请求 |
| 配置收紧或失效后失败，不退回宽松默认 | 过期、显式失效、无效 TOML、403/503、网络中断、可控时钟超时、429/Retry-After；仅保留过期参考，help/ping 与混合指令只输出诊断 |
| 功能域隔离及尚未支持功能可见 | 评论/idle 语义错误隔离，事件/路径规则逐条错误，重复 ID 和路径公共参数使域不可用；禁用 label/cc 不改变路径静态动作；help 显示 `Unsupported` |
| 不依赖 idle 开关，旧行为回归 | 入口测试显式关闭 idle；原有 25 项 idle 调度测试通过；配置仍只允许默认分支，发现分支变化期间暂停调度 |

## 检查结果

环境：本地 macOS，Rust/Cargo 1.98.1。GitHub 调用全部使用 API mock，没有在真实仓库触发审批、通知或工作流。

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

三项均通过；全部 **362 个测试通过，0 失败、0 忽略**。其中新增 **37 项**配置契约、缓存及入口验收测试；原有 **25 项 idle 测试**继续通过。格式、clippy 和 `git diff --check` 无错误。

全量回归曾发现别名表重排改变了拼写提示优先级，已恢复原顺序并通过原有断言。复核还补充了 r= 参数无效时不得降级审批、刷新失效竞态不得覆盖 Retry-After 的反例测试。

## 与后续子任务的边界

本次完成 #11 的配置基础设施与禁用/故障拦截，未宣称父 issue #10 已完成。
`Comments::gate` 提供默认手动门槛契约；完整来源识别、严格裸语法、会话 TTL/顺序/续期及路由接入仍由 #12–#14 交付，当前启用命令继续使用既有会话执行链路。
`r= @user` 已安全归一到代审批，缺参数或额外参数不执行；新的裸 `r=` 语法属于 #12。
持久化 inbox、重启后的诊断去重和自动事件重试/投递属于 #13；本次诊断限流为进程内 10 分钟抑制。
自动规则及路径规则的类型入口默认空，显式启用返回 `Unsupported`；实际动作由 #15–#17 实现。
