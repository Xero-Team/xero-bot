//! Command documentation shares the identities, aliases and modes used by the parser.
use crate::{config::repository::CommandId, lang::Lang};

impl CommandId {
    /// Include parameterized aliases and compound forms as well as the canonical spelling.
    /// The parser tests exercise these examples to keep the reference executable.
    pub fn examples(self) -> &'static [&'static str] {
        match self {
            Self::Cc => &["cc @user", "cc @alice @bob"],
            Self::RequestReview => &["r? @user"],
            Self::Ready => &["ready", "?r", "reviewer", "?r @user", "?r @user cc @other"],
            Self::Approve => &[
                "r+",
                "r= @user",
                "r+ as @user",
                "r+ @user",
                "r+ rollup",
                "r+ rollup=always",
                "r+ rollup=never",
                "r+ rollup-",
            ],
            Self::Label => &["label +name -other", "relabel +name -other"],
            Self::Assign => &["assign @user"],
            Self::Claim
            | Self::Unclaim
            | Self::Reject
            | Self::Review
            | Self::Codeql
            | Self::Author
            | Self::Blocked
            | Self::Ping
            | Self::Help
            | Self::Queue
            | Self::Rollup
            | Self::Try
            | Self::Retry => self.aliases(),
        }
    }

    pub fn needs_rollup(self) -> bool {
        matches!(self, Self::Rollup | Self::Try | Self::Retry)
    }

    pub fn supports_forgejo(self) -> bool {
        match self {
            Self::Review | Self::Codeql => false,
            Self::Claim
            | Self::Unclaim
            | Self::Cc
            | Self::RequestReview
            | Self::Ready
            | Self::Approve
            | Self::Reject
            | Self::Label
            | Self::Assign
            | Self::Author
            | Self::Blocked
            | Self::Ping
            | Self::Help
            | Self::Queue
            | Self::Rollup
            | Self::Try
            | Self::Retry => true,
        }
    }

    pub fn forgejo_description(self, lang: Lang) -> &'static str {
        if self == Self::Ready {
            lang.pick(
                "Waiting for review (status labels only); use r? @user to request a reviewer",
                "等待审阅（仅修改状态标签）；使用 r? @user 请求审阅者",
            )
        } else {
            self.description(lang)
        }
    }
    /// Describe the action and its key restrictions independently of its trigger mode.
    pub fn description(self, lang: Lang) -> &'static str {
        let (en, zh) = match self {
        CommandId::Rollup => ("Allow batching / rollup=never; write+ required", "允许组批 / rollup=never 单测；需要 write+ 权限"),
        CommandId::Try => ("Test a pinned revision without merging", "测试固定提交，通过后不合并"),
        CommandId::Retry => ("Retry the same approved revision", "重试仍有有效批准的同一提交"),
        CommandId::Claim => ("Assign yourself", "认领，指派给自己"),
        CommandId::Unclaim => ("Release your assignment", "释放自己的指派"),
        CommandId::Cc => ("`cc @user…`: notify users", "`cc @user…`：通知用户"),
        CommandId::RequestReview => (
            "`r? @user`: request a PR review; assign on an Issue",
            "`r? @user`：PR 请求审阅；Issue 指派",
        ),
        CommandId::Ready => (
            "Waiting for review; on PRs also re-request/ping reviewers",
            "等待审阅；PR 上还会重新请求/通知审阅者",
        ),
        CommandId::Approve => (
            "Approve current revision; enqueue when rollup is enabled. Requires write+, no self-approval",
            "批准当前提交；队列启用时入队。须有 write+ 权限，禁止自我审批",
        ),
        CommandId::Reject => (
            "Withdraw bot approval / queue entry; requires write+",
            "撤回 bot 审批/出队；须有 write+ 权限",
        ),
        CommandId::Review => (
            "Incremental AI review (COMMENT only)",
            "增量 AI 审查（仅 COMMENT）",
        ),
        CommandId::Codeql => (
            "Report existing CodeQL alerts for this change",
            "报告与本次改动相关的现有 CodeQL 告警",
        ),
        CommandId::Label => (
            "`label +a -b`: add/remove labels; queue control labels forbidden",
            "`label +a -b`：添加/移除标签；禁止修改队列控制标签",
        ),
        CommandId::Assign => ("`assign @user`: assign a user", "`assign @user`：指派用户"),
        CommandId::Author => ("Waiting on author", "等待作者"),
        CommandId::Blocked => ("Mark blocked", "标记受阻"),
        CommandId::Ping => ("Health check", "健康检查"),
        CommandId::Help => (
            "Show effective policy and session status",
            "显示有效策略与会话状态",
        ),
        CommandId::Queue => ("Show merge queue", "查看合并队列"),
    };
        lang.pick(en, zh)
    }
}
