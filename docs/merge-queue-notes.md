# Merge Queue — 首次实测记录

> 本文件是一次真实运行的记录:合并队列上线后第一个完整批次(PR #3)暴露的两个
> bug、修复方式、以及验收清单。留着它,下一位排查队列问题时可以从这里的
> 时间线读起。

## 现场:PR #3 首跑(2026-09-08)

运行日志时间线(本地时区 UTC):

| 时间 | 事件 | 含义 |
|---|---|---|
| 11:00:24 | `merge queue enqueue after r+ on …#3: queued` | r+ 入队成功,`merge queue: queued` 标签 |
| 11:01:19 | `pump Xero-Team/xero-bot: started [3]` | 组批:PR #3 head 并入 `staging`,提交 `e05ee18`,标签换 `testing` |
| 11:02:58 | `merge queue: delete unused staging …` WARN | **bug 1**:staging 被误删(见下) |
| 11:04+ | 每个 tick `pump done`,无事发生 | CI 永远等不到,直到超时 |

同时,在 staging 提交 `e05ee18` 上用 API 核实:`check-runs total_count = 0`
—— **bug 2**:staging push 根本不会触发 CI。

## Bug 1:staging 在测中被误删

`POST /repos/{owner}/{repo}/merges` 在 fast-forward-able 的并入场景下
**忽略我们传的 `commit_message`**,自拟提交消息
(`Merge <head> into <base>`)。链解析依赖 `xero-bot: merge #n` 前缀,
于是解析出空链 → 驱动误判"无事可做" → 走进空闲清理分支,
**把在测的 staging 分支删了**。

修复:**批次成员以 `merge queue: testing` 标签为真源**(标签是状态载体,
不依赖 commit message)。marker 链降级为辅助信息;空闲清理只允许删除
"已指向 main"的纯残留,其余一律 reset 不删除。

## Bug 2:staging push 不触发 CI

`ci.yml` 原来是 `on: push: branches: [main]` —— 即使 staging 活着,
上面的批次也永远不会得到任何 check run。修复:`branches: [main, staging]`。

## 验收清单(下一个测试 PR 上逐项核对)

- [ ] `r+` 后 PR 挂 `merge queue: queued` + 🧪 评论
- [ ] ≤30s 内组批:staging 出现 `xero-bot: merge #n` 提交(或 GitHub 自拟
      消息的 merge 提交 —— 两种都算组批成功,标签换了就是),PR 标签换
      `merge queue: testing`,最老成员收到 🚀 批次评论
- [ ] **staging push 触发 CI**(Actions 页面能看到 `sha=staging` 的运行)
- [ ] CI 绿后:出现 `staging`→`main` 的推进 PR 并被自动合并,main 前进
- [ ] 批次成员收到 🎉 评论、摘掉 testing 标签,staging 分支被删除
- [ ] 全程 `docker compose logs` 无 `delete unused staging` 出现在
      有标签成员存在时

## 已知边界

- `POST /merges` 的 `commit_message` 不受控:marker 可能缺失,链解析靠标签
  兜底 —— 这是设计内的,不是问题。
- 推进 PR 若被分支保护的 required review 挡住:bot 会评论说明
  "批准推进 PR 即批准整批"并持续重试,属正常路径。
