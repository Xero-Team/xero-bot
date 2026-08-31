//! Comment command language.
//!
//! A four-stage compiler rather than a set of regex scans:
//!
//! 1. [`mask`] blanks regions that render as code or quotation, preserving byte
//!    offsets so later spans still index the original text.
//! 2. [`lex`] produces tokens via `char_indices`, so no byte arithmetic can
//!    split a codepoint, and lexes `r?` / `r+` / `r-` / `?r` once each as
//!    distinct kinds.
//! 3. [`parse`] is recursive descent whose argument loops all stop at the same
//!    three boundary tokens.
//! 4. [`resolve`] applies within-comment policy: self-requests, duplicates,
//!    contradictory status labels.
//!
//! The structure exists to remove ambiguity rather than manage it. The previous
//! parser ran three regex passes that avoided each other using a
//! four-character window, which both double-fired on `@bot[bot] r? @alice` and
//! silently dropped `@bot, r? @alice`; and it mixed offsets from a lowercased
//! copy with slices of the original, which panicked on any comment containing
//! U+212A KELVIN SIGN.
//!
//! Semantics follow rust-lang/triagebot:
//! - `@bot <verb>` anywhere in a comment, case-insensitive
//! - several commands under one mention, separated by `;`
//! - bare `r? @user`, and `?r` shorthand for ready
//! - fenced code, inline code and blockquotes never trigger
//!
//! Text the bot doesn't understand stays silent unless it looks like a failed
//! command — see [`diag`].

pub mod diag;
mod lex;
mod mask;
pub mod parse;
mod resolve;

pub use diag::Diagnostic;
/// Re-exported for callers that put a login into a URL path or credit it on an
/// approval: the shape check belongs in one place, and the lexer's is the one
/// the parser already trusts.
pub use lex::is_valid_login;
pub use parse::ParsedCommand;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Ping,
    Help,
    Review,
    /// `r? @user` — assign reviewer
    RequestReview {
        user: String,
    },
    /// cc users (notify)
    Cc {
        users: Vec<String>,
    },
    /// ready / review / reviewer — set waiting-on-review, clear siblings
    Ready,
    /// author — set waiting-on-author, clear siblings
    Author,
    /// blocked — set blocked, clear siblings
    Blocked,
    /// label +a -b
    Label {
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// assign @user
    Assign {
        user: String,
    },
    /// claim (assign to commenter)
    Claim,
    /// unclaim / release-assignment (remove commenter from assignees)
    Unclaim,
    /// r+ [as @user] — approve on behalf of commenter (or the named user)
    Approve {
        on_behalf_of: Option<String>,
    },
    /// r- — withdraw approval
    Reject,
    /// codeql report
    Codeql,
}

impl Command {
    /// Whether this command is meaningless outside a pull request.
    ///
    /// GitHub's issues and PRs share the issues API, so labels, assignees and
    /// comments work identically on both — only the four commands that reach a
    /// `/pulls/` endpoint or a diff genuinely need a PR. Deciding it here rather
    /// than at the dispatch site keeps the list next to the enum it describes,
    /// so a new command has to answer the question to compile.
    pub fn requires_pr(&self) -> bool {
        match self {
            // reads the diff
            Command::Review => true,
            // maps code-scanning alerts onto the PR's changed files
            Command::Codeql => true,
            // submit / dismiss a pull request review
            Command::Approve { .. } | Command::Reject => true,
            Command::Ping
            | Command::Help
            | Command::RequestReview { .. }
            | Command::Cc { .. }
            | Command::Ready
            | Command::Author
            | Command::Blocked
            | Command::Label { .. }
            | Command::Assign { .. }
            | Command::Claim
            | Command::Unclaim => false,
        }
    }
}

/// Commands, plus any complaints about what couldn't be understood.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseOutput {
    pub commands: Vec<Command>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ParseOutput {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Compile a comment body into commands.
pub fn parse_commands(bot_name: &str, text: &str) -> ParseOutput {
    let masked = mask::mask_noncommand_regions(text);
    let tokens = lex::lex(bot_name, &masked);
    let parsed = parse::parse(&tokens);
    let mut diagnostics = parsed.diagnostics;
    let commands = resolve::resolve(bot_name, parsed.commands, &mut diagnostics);
    ParseOutput {
        commands,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Just the commands, for the many tests that don't care about diagnostics.
    fn cmds(bot: &str, text: &str) -> Vec<Command> {
        parse_commands(bot, text).commands
    }

    // ---- behavioural baseline (carried over from the regex parser) --------

    #[test]
    fn test_review_command() {
        assert_eq!(
            cmds("xero-review", "@xero-review review"),
            vec![Command::Review]
        );
    }

    #[test]
    fn test_review_case_insensitive_and_mid_comment() {
        assert_eq!(
            cmds("xero-review", "fix the bug\n@XERO-REVIEW   Review please"),
            vec![Command::Review]
        );
    }

    #[test]
    fn test_bot_suffix() {
        assert_eq!(
            cmds("xero-review", "@xero-review[bot] ping"),
            vec![Command::Ping]
        );
    }

    #[test]
    fn test_ping_help() {
        assert_eq!(
            cmds("xero-review", "@xero-review ping"),
            vec![Command::Ping]
        );
        assert_eq!(
            cmds("xero-review", "@xero-review help"),
            vec![Command::Help]
        );
    }

    #[test]
    fn test_bare_r_question() {
        assert_eq!(
            cmds("xero-review", "r? @octocat"),
            vec![Command::RequestReview {
                user: "octocat".into()
            }]
        );
    }

    #[test]
    fn test_mention_r_question() {
        assert_eq!(
            cmds("xero-review", "@xero-review r? @alice"),
            vec![Command::RequestReview {
                user: "alice".into()
            }]
        );
    }

    #[test]
    fn test_short_ready() {
        assert_eq!(cmds("xero-review", "?r"), vec![Command::Ready]);
    }

    #[test]
    fn test_short_ready_with_cc() {
        assert_eq!(
            cmds("xero-review", "?r cc @alice @bob"),
            vec![
                Command::Ready,
                Command::Cc {
                    users: vec!["alice".into(), "bob".into()]
                }
            ]
        );
    }

    #[test]
    fn test_label_command() {
        assert_eq!(
            cmds("xero-review", "@xero-review label +bug +A-compiler -wip"),
            vec![Command::Label {
                add: vec!["bug".into(), "A-compiler".into()],
                remove: vec!["wip".into()],
            }]
        );
    }

    #[test]
    fn test_assign_claim_unclaim() {
        assert_eq!(
            cmds("xero-review", "@xero-review assign @octocat"),
            vec![Command::Assign {
                user: "octocat".into()
            }]
        );
        assert_eq!(
            cmds("xero-review", "@xero-review claim"),
            vec![Command::Claim]
        );
        assert_eq!(
            cmds("xero-review", "@xero-review unclaim"),
            vec![Command::Unclaim]
        );
        assert_eq!(
            cmds("xero-review", "@xero-review release-assignment"),
            vec![Command::Unclaim]
        );
    }

    #[test]
    fn test_approve_reject() {
        assert_eq!(
            cmds("xero-review", "@xero-review r+"),
            vec![Command::Approve { on_behalf_of: None }]
        );
        assert_eq!(
            cmds("xero-review", "@xero-review r+ as @octocat"),
            vec![Command::Approve {
                on_behalf_of: Some("octocat".into())
            }]
        );
        assert_eq!(
            cmds("xero-review", "@xero-review r-"),
            vec![Command::Reject]
        );
    }

    #[test]
    fn test_codeql() {
        assert_eq!(
            cmds("xero-review", "@xero-review codeql"),
            vec![Command::Codeql]
        );
    }

    #[test]
    fn test_r_question_inside_code_block_ignored() {
        assert!(cmds("xero-review", "```\nr? @octocat\n```").is_empty());
    }

    #[test]
    fn test_unknown_verb_yields_no_command() {
        assert!(cmds("xero-review", "@xero-review do the thing please").is_empty());
    }

    #[test]
    fn test_plain_mention_no_verb() {
        assert!(cmds("xero-review", "cc @xero-review about this").is_empty());
    }

    // ---- structural fixes ------------------------------------------------

    /// Arguments stop at end of line. They used to run to the end of the whole
    /// comment, so `cc` collected every later mention.
    #[test]
    fn args_bounded_to_line() {
        assert_eq!(
            cmds(
                "bot",
                "@bot cc @alice\nsomething about @bob and @carol later"
            ),
            vec![Command::Cc {
                users: vec!["alice".into()]
            }]
        );
    }

    /// A following mention also ends the previous command's arguments.
    #[test]
    fn args_bounded_by_next_mention() {
        assert_eq!(
            cmds("bot", "@bot cc @alice @bot assign @bob"),
            vec![
                Command::Cc {
                    users: vec!["alice".into()]
                },
                Command::Assign { user: "bob".into() }
            ]
        );
    }

    /// Labels no longer collect signed tokens from later lines.
    #[test]
    fn label_args_bounded_to_line() {
        assert_eq!(
            cmds("bot", "@bot label +bug\nsee -wip and +other below"),
            vec![Command::Label {
                add: vec!["bug".into()],
                remove: vec![],
            }]
        );
    }

    /// The bug behind the `help` storm: every row of the help table wraps its
    /// command in backticks, and inline code wasn't masked.
    #[test]
    fn inline_code_is_not_a_command() {
        let help_row = "| `@bot review` | AI 代码审查 |\n| `@bot label +a -b` | 标签 |";
        assert!(
            cmds("bot", help_row).is_empty(),
            "inline code must not execute: {:?}",
            cmds("bot", help_row)
        );
    }

    #[test]
    fn blockquote_is_not_a_command() {
        assert!(cmds("bot", "> @bot review\n\n我同意").is_empty());
    }

    /// `[bot]` is five characters, which overflowed the old four-character
    /// window, so both the mention anchor and the bare `r?` anchor fired.
    #[test]
    fn bot_suffix_does_not_double_fire() {
        assert_eq!(
            cmds("xero-review", "@xero-review[bot] r? @alice"),
            vec![Command::RequestReview {
                user: "alice".into()
            }],
            "must be exactly one request"
        );
    }

    /// Punctuation after the mention used to suppress the bare anchor while the
    /// verb parser read `,` as the verb, dropping the command entirely.
    #[test]
    fn mention_followed_by_punctuation_still_parses() {
        for text in [
            "@bot, r? @alice",
            "@bot: r? @alice",
            "@bot , ping",
            "@bot: ping",
        ] {
            assert!(
                !cmds("bot", text).is_empty(),
                "command vanished for {text:?}"
            );
        }
    }

    #[test]
    fn mention_prefix_is_not_the_bot() {
        assert!(cmds("bot", "@bottle ping").is_empty());
        assert!(cmds("xero-review", "@xero-reviewer ping").is_empty());
    }

    /// `\s*` in the old regex matched newlines, so a rhetorical `r?` at end of
    /// line assigned whatever word started the next one.
    #[test]
    fn r_question_does_not_cross_newline() {
        assert!(
            cmds("bot", "who should review this r?\nMaybe next week").is_empty(),
            "must not assign 'Maybe'"
        );
    }

    /// Tokens had to be exactly `@login`, so a comma dropped the first user.
    #[test]
    fn cc_accepts_comma_separated() {
        assert_eq!(
            cmds("bot", "@bot cc @alice, @bob"),
            vec![Command::Cc {
                users: vec!["alice".into(), "bob".into()]
            }]
        );
    }

    /// Full-width punctuation is normal in this bot's audience, and used to
    /// make the whole command disappear with no reply.
    #[test]
    fn assign_accepts_fullwidth_punctuation() {
        assert_eq!(
            cmds("bot", "@bot assign @alice。"),
            vec![Command::Assign {
                user: "alice".into()
            }]
        );
    }

    /// `starts_with("cc")` matched `ccache`.
    #[test]
    fn ccache_is_not_cc() {
        assert_eq!(cmds("bot", "?r ccache @alice"), vec![Command::Ready]);
    }

    /// The old code built `RequestReview { user: bot_name }` with a comment
    /// claiming it would be ignored, and the handler assigned it anyway.
    #[test]
    fn self_request_is_dropped() {
        assert!(cmds("bot", "@bot r? @bot").is_empty());
        assert!(cmds("bot", "r? @bot").is_empty());
    }

    /// `?r @user` used to drop the user in silence, so it only set the label
    /// and looked like the parser was broken.
    #[test]
    fn short_ready_requests_review() {
        assert_eq!(
            cmds("bot", "?r @alice"),
            vec![
                Command::Ready,
                Command::RequestReview {
                    user: "alice".into()
                }
            ]
        );
    }

    #[test]
    fn short_ready_takes_the_first_user_only() {
        assert_eq!(
            cmds("bot", "?r @alice @bob"),
            vec![
                Command::Ready,
                Command::RequestReview {
                    user: "alice".into()
                }
            ]
        );
    }

    #[test]
    fn short_ready_user_and_cc() {
        assert_eq!(
            cmds("bot", "?r @alice cc @bob"),
            vec![
                Command::Ready,
                Command::RequestReview {
                    user: "alice".into()
                },
                Command::Cc {
                    users: vec!["bob".into()]
                }
            ]
        );
    }

    // ---- `;` chaining under one mention ----------------------------------

    #[test]
    fn semicolon_chains_commands_under_one_mention() {
        assert_eq!(
            cmds("bot", "@bot label +bug -wip; assign @alice; ready"),
            vec![
                Command::Label {
                    add: vec!["bug".into()],
                    remove: vec!["wip".into()],
                },
                Command::Assign {
                    user: "alice".into()
                },
                Command::Ready,
            ]
        );
    }

    /// A `;` ends the previous command's arguments, so `cc` doesn't absorb the
    /// user belonging to the next one.
    #[test]
    fn semicolon_bounds_arguments() {
        assert_eq!(
            cmds("bot", "@bot cc @alice; assign @bob"),
            vec![
                Command::Cc {
                    users: vec!["alice".into()]
                },
                Command::Assign { user: "bob".into() },
            ]
        );
    }

    #[test]
    fn trailing_semicolon_is_harmless() {
        assert_eq!(cmds("bot", "@bot ping;"), vec![Command::Ping]);
        assert_eq!(cmds("bot", "@bot ping; ;"), vec![Command::Ping]);
    }

    /// Newlines deliberately do not continue a mention's scope: `ready`,
    /// `review` and `blocked` are all real verbs, so an implicit continuation
    /// would execute prose like "ready to merge after this".
    #[test]
    fn newline_does_not_continue_a_mention() {
        assert_eq!(
            cmds("bot", "@bot review\nready to merge after this"),
            vec![Command::Review],
            "the second line is prose, not a command"
        );
    }

    // ---- resolve policies -------------------------------------------------

    #[test]
    fn duplicate_commands_collapse() {
        let out = parse_commands("bot", "@bot review; review");
        assert_eq!(out.commands, vec![Command::Review]);
        assert!(
            out.diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::DuplicateCommand { .. })),
            "should say it deduplicated: {:?}",
            out.diagnostics
        );
    }

    /// The three status labels clear each other, so running two in one comment
    /// did both operations and posted two contradictory replies.
    #[test]
    fn conflicting_status_labels_keep_the_last() {
        let out = parse_commands("bot", "@bot ready; blocked");
        assert_eq!(out.commands, vec![Command::Blocked]);
        assert!(
            out.diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::ConflictingStatus { .. })),
            "should explain the conflict: {:?}",
            out.diagnostics
        );
    }

    // ---- diagnostics vs silence -------------------------------------------

    /// The class of bug this whole module exists to end: a near-miss command
    /// used to vanish, leaving the user certain it had worked.
    #[test]
    fn typo_after_mention_is_named_with_a_suggestion() {
        let out = parse_commands("bot", "@bot reviwe");
        assert!(out.commands.is_empty());
        assert_eq!(
            out.diagnostics,
            vec![Diagnostic::UnknownVerb {
                verb: "reviwe".into(),
                suggestion: Some("review"),
                span: 5..11,
            }]
        );
    }

    /// Punctuation between the mention and the typo doesn't change that.
    #[test]
    fn typo_after_punctuation_is_still_named() {
        for text in ["@bot, reviwe", "@bot: reviwe", "@bot、reviwe"] {
            let out = parse_commands("bot", text);
            assert!(
                out.diagnostics
                    .iter()
                    .any(|d| matches!(d, Diagnostic::UnknownVerb { .. })),
                "expected a complaint for {text:?}, got {:?}",
                out.diagnostics
            );
        }
    }

    /// Noise suppression. A mention inside a sentence is not a failed command,
    /// and answering it would make the bot something people mute. Without the
    /// prose check, `@bot 这个 PR 很好` draws "`pr` 不是命令,是否想用 `cc`?".
    #[test]
    fn prose_around_a_mention_stays_silent() {
        for text in [
            "@bot 谢谢!🎉",
            "@bot 这个 PR 很好",
            "@bot 我觉得这里可以 merge 了",
            "cc @bot about this",
            "@bot",
        ] {
            let out = parse_commands("bot", text);
            assert!(
                out.commands.is_empty() && out.diagnostics.is_empty(),
                "must stay silent for {text:?}, got {out:?}"
            );
        }
    }

    /// Silence about unknown words must not cost the known ones: a verb still
    /// runs when it's embedded in Chinese prose, which is how this audience
    /// writes.
    #[test]
    fn known_verb_after_prose_still_runs() {
        assert_eq!(cmds("bot", "@bot 请 review 一下"), vec![Command::Review]);
        assert_eq!(
            cmds("bot", "@bot 麻烦 r? @alice"),
            vec![Command::RequestReview {
                user: "alice".into()
            }]
        );
    }

    /// The two roles of a mention. Mid-sentence it still commands the bot —
    /// `@bot cc @a @bot assign @b` has always relied on that — but it is no
    /// longer treated as an attempt worth correcting.
    #[test]
    fn mention_in_a_sentence_acts_but_stays_quiet() {
        assert_eq!(cmds("bot", "please cc @bot review"), vec![Command::Review]);
        assert!(parse_commands("bot", "please ask @bot about this")
            .diagnostics
            .is_empty());
    }

    /// A list bullet doesn't stop a line from being addressed to the bot.
    #[test]
    fn list_bullet_before_a_mention_still_addresses() {
        let out = parse_commands("bot", "- @bot reviwe");
        assert!(
            out.diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::UnknownVerb { .. })),
            "{:?}",
            out.diagnostics
        );
    }

    /// A recognised verb missing its argument is worth naming even in prose —
    /// the user clearly meant to command the bot.
    #[test]
    fn missing_argument_is_named() {
        for (text, verb) in [
            ("@bot assign", "assign"),
            ("@bot label", "label"),
            ("@bot cc", "cc"),
        ] {
            let out = parse_commands("bot", text);
            assert!(out.commands.is_empty(), "{text} should run nothing");
            assert!(
                out.diagnostics.iter().any(
                    |d| matches!(d, Diagnostic::MissingArgument { verb: v, .. } if *v == verb)
                ),
                "expected MissingArgument({verb}) for {text:?}, got {:?}",
                out.diagnostics
            );
        }
    }

    #[test]
    fn invalid_login_is_named() {
        let out = parse_commands("bot", "@bot assign @-bad");
        assert!(out.commands.is_empty());
        assert!(
            out.diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::InvalidLogin { raw, .. } if raw == "-bad")),
            "{:?}",
            out.diagnostics
        );
    }

    /// Only the four commands that reach a `/pulls/` endpoint or read a diff
    /// need a pull request. Spelled out as a list rather than derived, so this
    /// fails if `requires_pr` is widened without a decision being made.
    #[test]
    fn only_four_commands_need_a_pull_request() {
        let pr_only = [
            Command::Review,
            Command::Codeql,
            Command::Approve { on_behalf_of: None },
            Command::Approve {
                on_behalf_of: Some("alice".into()),
            },
            Command::Reject,
        ];
        for c in &pr_only {
            assert!(c.requires_pr(), "{c:?} should be PR-only");
        }
        let issue_ok = [
            Command::Ping,
            Command::Help,
            Command::RequestReview {
                user: "alice".into(),
            },
            Command::Cc {
                users: vec!["alice".into()],
            },
            Command::Ready,
            Command::Author,
            Command::Blocked,
            Command::Label {
                add: vec!["bug".into()],
                remove: vec![],
            },
            Command::Assign {
                user: "alice".into(),
            },
            Command::Claim,
            Command::Unclaim,
        ];
        for c in &issue_ok {
            assert!(!c.requires_pr(), "{c:?} works on an issue");
        }
        // and every variant is accounted for above
        assert_eq!(pr_only.len() - 1 + issue_ok.len(), 15);
    }

    /// The input that started all of this. The help table lists every command,
    /// so the bot used to execute six of them on its own output — and must not
    /// now answer itself with a wall of diagnostics either.
    #[test]
    fn own_help_text_produces_nothing_at_all() {
        // Both tables: the English one lists the same commands, so it is the
        // same trap in a different language.
        for lang in [crate::lang::Lang::En, crate::lang::Lang::Zh] {
            // Both settings of the on-behalf gate: it rewrites a row of the
            // table, and a row is exactly where a live command would hide.
            for on_behalf in [false, true] {
                let help = crate::handlers::help_text("bot", lang, on_behalf);
                let out = parse_commands("bot", &help);
                assert!(
                    out.commands.is_empty() && out.diagnostics.is_empty(),
                    "{lang:?} help text (on_behalf={on_behalf}) must be inert, got {out:?}"
                );
            }
        }
    }

    #[test]
    fn utf8_input_never_panics() {
        for s in [
            "@bot \u{212A} x",
            "@bot \u{2126}\u{2126} 中文",
            "@bot cc \u{130} @alice",
            "@bot 谢谢!🎉",
            "就绪?r",
            "r? \u{212B}",
            "@bot label +中文标签 -另一个",
        ] {
            let _ = parse_commands("bot", s);
        }
    }
}
