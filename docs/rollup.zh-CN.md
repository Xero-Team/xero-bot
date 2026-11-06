# Rollup 队列

[English](rollup.md)

每个 GitHub 仓库对应一个独立项目，各有自己的队列、配置、测试分支与历史。
绑定的 Forgejo 仓库向同一项目提交指令。由 GitHub 统一推进主分支，再通过 bridge
同步到 Forgejo。

网站采用极简表格：`/` 列项目，`/queue/OWNER/REPO` 查看项目队列和批次，
`/commands` 从运行版本的指令注册表自动生成文档。默认英文，English/中文切换
保留当前项目（`?lang=en` / `?lang=zh`），队列每 10 秒更新。网页只读。
私有仓库即使配置 `public=true` 也不会公开。项目收到首条队列指令后显示，
可以使用 `@bot queue` 注册尚未有 PR 排队的项目。

## 指令

在任意一侧的 PR 中 @ 对应机器人：

| 指令 | 行为 |
| --- | --- |
| `r+` | 批准当前源提交；默认单独测试，已设置 rollup 的条目保留组批设置。 |
| `r+ rollup` | 批准当前提交并允许组批。 |
| `rollup` / `rollup=never` | 修改已批准、尚未测试的条目的组批设置。 |
| `r-` | 撤销批准，使正在测试的候选失效。 |
| `try` | 单独试跑，成功也不合并。 |
| `retry` | 重试头提交未变、批准仍有效的失败条目。 |
| `queue` | 查看状态及当前项目的队列链接。 |

批准时校验实时写权限，作者不能批准自己的 PR。Forgejo 账号须在双方配置中
显式映射。转发的评论、标签、AI review、网页 Approve 不会让 PR 自动入队。
源提交变化须重新批准；副本尚未同步完成则等待。有写权限的审阅者在批准后
要求修改，也需要新的 `r+`。

## 测试与合并

收集窗口结束后固定批次成员及批准 SHA，在本地合并到最新主分支，再一次性推送
`automation/xero/rollup/N`（试跑为 `automation/xero/try/N`）。自动创建的审计 PR
记录成员、批准人和候选 SHA；后来的批准等待下一批。关闭审计 PR 会撤销该批次。

新增合并提交使用实际 GitHub App bot 用户 ID 对应的 noreply 邮箱。
将该邮箱绑定为 Forgejo bot 的已验证附加邮箱后，同一 SHA 在两侧均关联正确账号。
源提交作者与已有历史保持不变；普通用户 `@` 映射见 [Forgejo 配置](forgejo.md)。

GitHub 必须在候选的 push 工作流上通过全部配置的 job。矩阵任务或不同工作流
使用相同 job 名称时，最新工作流执行中的所有同名任务都必须通过，不能以一个成功覆盖失败。
GitHub 失败、API 不可用，
或等待超过 `fallback_after_seconds` 后，将**同一 SHA** 推到 Forgejo 测试。
Forgejo 必须通过全部指定 workflow 的所有 job。任一侧完整通过即可合并，
不会拼凑两侧的部分成功；跳过、取消、缺失和其他 SHA 的结果不能批准合并。

合并前重新检查源提交、权限与主分支，发布 `xero/rollup` 成功状态，再非强制
快进到经过测试的提交。主分支变化就重建重测。两侧都失败则二分批次，后续批次
基于实际合入后的主分支测试；单 PR 失败等待修复后重新批准，或 `retry`。
API 故障自动退避重试，测试超时等待手动重试。测试分支会清理，审计 PR 和历史保留。
Bridge 将通过测试的提交同步到 Forgejo
目标分支后，机器人调用原生接口将对应 PR 标记为已合并，不再产生新的合并提交。

## 启用项目

1. 部署新版，保留 `XERO_DATA_DIR`，设置 `ROLLUP_ENABLED=true`、`PUBLIC_URL`。
   仓库不启用 `[rollup]` 时继续使用旧队列。
2. 双方默认分支配置普通仓库 bridge、固定仓库 ID、`sync_refs=true`。
   切换前清空旧队列；旧队列仍有 testing 标签时新队列不会开始。
3. 在双方按实际项目修改 [配置示例](../examples/rollup.toml)，保持账号映射和
   Forgejo workflow 名称一致。每个项目独立配置，不复用其他项目的仓库 ID。
4. 两侧 CI 开启 `automation/xero/**` 的 push 触发。GitHub App 需要 contents、
   pull requests、commit statuses 写权限和 Actions 读权限；Forgejo 17 的 bot 需要
   仓库管理员权限，才能查询其他审阅者的实时权限。Forgejo 仓库还需开启“允许将
   PR 标记为手动合并”（`allow_manual_merge`），以便在测试提交同步后记录合并状态。
5. 分支保护要求该 App 的 `xero/rollup` 检查。若同时强制要求 GitHub 原始 CI，
   Forgejo 通过后仍会被 GitHub 失败挡住。机器人不会修改或绕过分支保护，
   其他保护规则应保留。
6. 使用 `queue` 注册项目，在一次性 PR 上用 `try`、`r+ rollup` 验证：两侧候选
   SHA 一致，主分支最终就是通过测试的 SHA。

队列状态、批准、候选和回退进度保存在 `XERO_DATA_DIR` 下的 SQLite 中；重启不会
把旧批准套到新提交上。同一目录只运行一个实例。回滚到不识别 `[rollup]` 的版本前，
先停用项目队列、结束测试批次，再移除该配置表。
