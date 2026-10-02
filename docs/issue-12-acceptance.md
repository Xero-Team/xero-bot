# Issue #12 实现与验收

验收日期：2026-10-01。依据：[父 issue #10](https://github.com/Xero-Team/xero-bot/issues/10)
与 [解析器 issue #12](https://github.com/Xero-Team/xero-bot/issues/12)。规范指令 ID 和别名复用 #11 的 `CommandId`，解析器不读取配置、不执行网络请求。

## 实现契约

`parse_commands` 返回尚未去重的 `ParsedCommand` 候选。每个候选保留完整语义参数、规范 ID（`id()`）、原文 UTF-8 字节范围 `span`、入口形式 `source`、真实 bot mention 的范围 `mention_span`，以及拆分 `?r` 组合时共享的 `compound_span`。mention 作用域止于换行；大小写和正确的 `[bot]` 后缀不影响识别，其他账户或非法后缀不能冒充 bot。

普通裸单词命令先验证整条评论：每个非空指令必须完整匹配，参数不得跨换行/分号。只要存在散文、列表符号、代码或引用，就不抽取裸单词命令；代码遮罩不能删除内容后制造一个合法指令块。显式调用保留原有会话式零参数语法，`r?` / `?r` 仍可独立出现在散文中。裸审批只在完整指令块中产生候选，明确的审批语法错误仍给出诊断。

`cc` 只接受合法的个人 `@login` 列表，支持空格或逗号分隔；`assign` 只接受一个目标。单词与参数间需要空白，`cc@alice`、相连的 `@alice@bob` 不算指令。`r= @user`、`r+ as @user`、`r+ @user` 共用审批参数解析，缺失、非法或多余参数不会退化成普通批准。目标后已有的英文/中文句末标点兼容性保留。

分发层逐候选调用 #11 的模式检查，随后才调用 `resolve_commands` 处理获准集合的去重与状态冲突。所有入口（含符号、别名、组合子命令）都经过检查。明确语法错误只产生诊断，不读取仓库配置、提交、权限或历史，也不调用业务动作 API；若回复评论，唯一 HTTP 写入是该诊断。

## 验收矩阵

| #12 要求 | 测试证据 |
| --- | --- |
| 英文/中文、UTF-8、分号、多行、标点、代码与引用的正反例 | `tests/command_parser.rs` 的表驱动用例；包含 `claim`、`take; cc @alice`、纯多行指令及散文、列表、行内/跨行代码、围栏、连续缩进代码反例 |
| `cc` / `assign` / 审批完整消费参数，显式与裸入口一致 | `parameters_are_identical_and_never_absorb_the_next_command`；逐项组合前缀与换行/分号，检查下一条指令不会被吞并 |
| 三种审批形式语义等价，错误不降级 | `approvals_share_target_grammar_and_never_degrade_after_an_error`；覆盖缺目标、非法用户名、长度边界、多目标、多余散文/标签和中英文句末标点；保留 #11 的 `r=` 回归 |
| 规范 ID / 别名契约一致 | `every_registered_alias_produces_the_canonical_id_in_both_forms` 遍历全部 16 个规范 ID 的所有别名及两种入口 |
| mention 作用域和组合来源完整 | `mention_scope_is_exact_case_insensitive_and_ends_at_newline`、`utf8_ranges_and_compound_origins_index_the_original_comment`；在含中文、K、emoji 的原文上直接验证切片 |
| 先模式判断，再去重/冲突 | 纯函数策略测试，以及分发 HTTP 测试 `disabled_blocked_does_not_cancel_permitted_ready`、`denied_bare_duplicate_does_not_erase_explicit_approval`；后者断言仅产生一次合法 APPROVE |
| 快捷入口不能绕过模式检查 | `symbols_cannot_bypass_always_mention_even_in_compounds`；禁用别名/组合的既有测试继续通过 |
| 解析错误不触发业务 API，散文静默 | `parser_errors_never_reach_github_action_or_lookup_apis` 检查只发诊断；`ordinary_chat_code_and_quotes_never_load_config` 检查零 HTTP 请求 |
| bot 自回复屏蔽不退化 | `self_comment_ignored_via_app_id`、扩充的 `self_comment_ignored_via_bot_suffix_login`，以及双语 help 文本回归 |

## 验证命令

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
git diff --check
```

以上检查全部通过：**380 个测试通过，0 失败、0 忽略**；fmt、clippy 与 rustdoc（`-D warnings`）以及 `git diff --check` 无错误。新增 13 项表驱动解析验收测试和 5 项分发 HTTP 验收测试，并更新旧语义回归用例。HTTP 测试使用本地 wiremock，不向真实 GitHub 仓库提交审批、标签或评论。

## PR #22 审查修复

- [mention 边界](https://github.com/Xero-Team/xero-bot/pull/22#discussion_r4152319158)：中文和外围 Markdown 标点结束账号 token，`@bot请 review`、括号/加粗中的 `r? @alice` 恢复识别；显式参数支持闭合括号。下划线、路径分隔符、错误 bot 后缀仍作为整个非法账号拒绝，不能按合法前缀触发。
- [跨段落行内代码](https://github.com/Xero-Team/xero-bot/pull/22#discussion_r4152319167)：只在同一段落内匹配反引号，空行、带空白的空行、CRLF 和已遮罩代码/引用块都终止段落；同段跨行代码仍被屏蔽，并保持 UTF-8 原文字节范围。
- [`cc` 句末标点](https://github.com/Xero-Team/xero-bot/pull/22#discussion_r4152319174)：显式、裸入口和 `?r ... cc` 组合统一接受句末标点及尾逗号；多余散文、非法名单仍拒绝，分号/换行后的下一条指令不被吞并。

以上三项均先用回归测试复现失败，再修复并验证；另补充了候选来源、解析边界和验收测试的函数文档。

## 范围边界

本项没有修改执行权限，也没有建立持久会话。会话证据仍是现有的历史读取机制，仅显式、未禁用的候选可作为证据，新增裸语法不能自行开启会话。持久化 TTL、源评论顺序、授权预检、r- 的 write+ 校验仍由 #13/#14 完成；自动事件与路径规则仍由对应子任务完成，不能据此宣称父 issue #10 已整体交付。
