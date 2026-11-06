# Service admission

Making the GitHub App public lets other owners install it. It does not grant permission to use this deployment. The operator must explicitly admit every repository using **server environment configuration**, before repository TOML, commenter permissions or review automation can take effect.

## Upgrade and onboarding

1. Inventory the repositories already using the deployment, including both ends of every bridge. Populate `BOT_ACCESS_POLICY` **before upgrading**. Missing, empty, or `{}` means deny all. Malformed JSON, unknown fields, invalid/nonpositive IDs and duplicate repository names/IDs fail startup.
2. Install the GitHub App on the intended owner and select only the needed repositories. Register its **installation ID**, the owner's numeric **account ID**, the **repository ID**, and `owner/repo`. The installation ID is different from the App ID. It is present in signed webhook payloads; App administrators can also query the [installation endpoints with an App JWT](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app). Repository metadata is available at `GET https://api.github.com/repos/OWNER/REPO`: use `full_name`, `id`, and `owner.id`.
3. Register each Forgejo repository's `full_name`, `id`, and `owner.id` from `GET FORGEJO_BASE_URL/api/v1/repos/OWNER/REPO` (omit the additional `/api/v1` if the configured URL already includes it). Forgejo entries refer only to the configured Forgejo instance. An instance migration requires rechecking all entries.
4. Set `BOT_ACCESS_POLICY` to a single-line JSON value in the server `.env` / orchestrator environment and restart. This is not a repository `.github/xero-bot.toml` setting. Keep the existing signed webhooks, bilateral bridge configuration and command authorization in place; admission is an additional gate.

Example structure, with **illustrative IDs that must be replaced**:

```json
{
  "github": [
    {
      "installation_id": 100,
      "owner_id": 200,
      "repository_id": 300,
      "repository": "Xero-Team/xero-bot"
    },
    {
      "installation_id": 101,
      "owner_id": 201,
      "repository_id": 301,
      "repository": "Zero-kernel/Nilix"
    }
  ],
  "forgejo": [
    {
      "owner_id": 400,
      "repository_id": 500,
      "repository": "xero-team/xero-bot"
    },
    {
      "owner_id": 400,
      "repository_id": 501,
      "repository": "xero-team/Nilix"
    }
  ]
}
```

Names match case-insensitively, but all IDs must match the **same entry**. There is no wildcard or automatic organization-wide enrollment. A new project, rename, transfer, recreated repository or reinstalled App requires the operator to review and update the relevant entry. One GitHub installation cannot name different owner IDs. At most 500 repositories may be listed per installation, matching GitHub's scoped installation-token limit. Tokens used by Git subprocesses are restricted to that installation's admitted repository IDs.

## Enforcement and revocation

- Signatures are still verified first. Signed but unapproved repository events receive HTTP 200 with `{"ignored":"repository not admitted"}` and never enter command/bridge queues, configuration caches or idle scheduling. Repository-less events (including installation events and pings) are ignored. Invalid signatures remain HTTP 401.
- Discovery filters repositories by the complete identity tuple before loading TOML or scheduling work. Persisted command deliveries, bridge deliveries and rollup queues are rechecked on execution. Live metadata checks catch replacement/transfer; repository settings cannot authorize a different sync destination.
- Remove entries and restart to revoke access. Queued commands are discarded without performing their actions; bridge deliveries do not execute. Rollup state is retained but suspended while its GitHub repository is denied. Re-authorizing a suspended rollup can resume its existing queue, so review pending approvals before re-enabling. Updating an environment file alone does not change the running process or cancel in-flight work: restart the service to apply the policy.
- Keep the App private key and Forgejo token on the server. This policy controls use of this deployment; it cannot prevent someone from running the open-source software with their own credentials. It also does not stop GitHub delivering unrelated installation events to a public App.

## External cron

`CRON_SECRET` protects manual/external `GET /cron` triggers. An empty or whitespace-only value **disables** that endpoint (HTTP 401). With a nonempty value, requests must send `Authorization: Bearer <CRON_SECRET>`. Generate a random secret, for example with `openssl rand -hex 32`, if an external scheduler needs this route. Internal scheduled workers do not use `/cron` and continue to apply the same admission policy.

## 中文

将 GitHub App 设为公开后，其他组织可以安装，但安装本身不代表可以使用这台服务器上的机器人。服务端 `BOT_ACCESS_POLICY` 明确列出获准仓库；仓库里的 TOML 和用户的仓库管理员身份都不能自行放行。

升级前先登记所有现有项目和同步两端。GitHub 每条记录必须包含安装 ID（不是 App ID）、所属账号/组织 ID、仓库 ID 和 `owner/repo`；Forgejo 每条记录包含所属账号/组织 ID、仓库 ID 和仓库名，限定在当前配置的 Forgejo 实例。上面的示例数字均为占位示意，必须替换为平台真实 ID。`GET /repos/OWNER/REPO` 返回 `id`、`owner.id` 和 `full_name`；GitHub 安装 ID 可从签名 webhook 或使用 App JWT 的安装 API 获取。

将 JSON 压成一行写入部署环境的 `BOT_ACCESS_POLICY`，重启生效。两侧未配置、留空或 `{}` 都默认拒绝全部项目；错误配置会阻止启动。新增项目、仓库改名/转移/重建或 App 重装后，要人工重新核对配置，不支持通配符。新增 Nilix 时只添加该仓库对应的两侧记录，不会自动开放整个 Zero-kernel。

未获准事件即使签名正确也不会入队。后台扫描、持久化队列恢复、rollup、同步目标和 Git 推送路径均重新校验。删除配置条目并重启可撤权：旧评论命令不再执行，rollup 队列保留但暂停；重新放行前应审阅已有批准。仅修改 `.env` 不会中止正在运行的进程，需重启应用。

`CRON_SECRET` 是外部调用 `/cron` 的 Bearer 密钥。留空意味着关闭该入口并返回 401，内置定时任务继续运行；有外部调度器需要调用时再设置随机密钥。
