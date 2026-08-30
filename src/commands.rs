//! Comment command parser.
//!
//! Mirrors rust-lang/triagebot semantics:
//! - `@xero <verb>` commands, case-insensitive, anywhere in the comment
//! - bare `r? @user` (review request) anywhere — `r? user` without `@` also works
//! - `?`-prefixed short commands: `?r` (ready), args forwarded (`?r cc @user`)
//! - multiple commands per comment, executed in order
//! - fenced code blocks are ignored (no false triggers)
//!
//! Unknown text after a valid mention is not an error — the bot simply ignores
//! comments it doesn't understand (triagebot posts an error; we stay quiet to
//! avoid noise, matching the original Python bot's behavior).

use regex::Regex;

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

/// One parsed command plus where it appeared (for ordered execution).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommand {
    pub command: Command,
    pub start: usize,
}

/// Strip fenced code blocks from a comment, replacing them with spaces so
/// character offsets stay stable (we don't need offsets to survive, but it
/// keeps `r?` detection from matching inside examples).
fn strip_code_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(&" ".repeat(line.len()));
            continue;
        }
        if in_fence {
            out.push_str(&" ".repeat(line.len()));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Parse all bot commands out of a comment body.
///
/// `bot_name` is the configured BOT_NAME (e.g. "xero-review"). Both
/// `@xero-review[bot]` and plain `@xero-review` mentions are recognized.
pub fn parse_commands(bot_name: &str, text: &str) -> Vec<ParsedCommand> {
    let cleaned = strip_code_blocks(text);
    let name_lower = bot_name.to_lowercase();

    let mut commands: Vec<ParsedCommand> = Vec::new();

    // --- anchor 1: @bot mentions ---
    let mention_re = Regex::new(&format!(r"(?i)@{}\b", regex::escape(&name_lower))).unwrap();
    let mut mention_spans: Vec<(usize, usize)> = Vec::new();
    for m in mention_re.find_iter(&cleaned) {
        let rest = &cleaned[m.end()..];
        // skip the "[bot]" suffix if present
        let rest = rest.strip_prefix("[bot]").unwrap_or(rest);
        let start = m.start();
        mention_spans.push((start, m.end()));
        if let Some(cmd) = parse_verb(bot_name, rest) {
            commands.push(ParsedCommand {
                command: cmd,
                start,
            });
        }
    }

    // --- anchor 2: bare r? review requests (triagebot: works anywhere, no mention needed) ---
    // skip occurrences that a mention already consumed (e.g. `@bot r? @user`)
    let r_re = Regex::new(r"(?i)\br\?\s*@?([A-Za-z0-9][A-Za-z0-9-]*)").unwrap();
    for m in r_re.captures_iter(&cleaned) {
        let match_start = m.get(0).map(|g| g.start()).unwrap_or(0);
        let inside_mention = mention_spans
            .iter()
            .any(|(s, e)| match_start >= *s && match_start < *e + 4);
        if inside_mention {
            continue;
        }
        // capture group 1 = the username
        let user = m.get(1).map(|g| g.as_str().to_string()).unwrap_or_default();
        if user.is_empty() || user.eq_ignore_ascii_case(&name_lower) {
            continue;
        }
        commands.push(ParsedCommand {
            command: Command::RequestReview { user },
            start: match_start,
        });
    }

    // --- anchor 3: ? short commands (?r, ?r cc @user) ---
    // (no look-behind in the regex crate; emulate with a char class capture)
    let short_re = Regex::new(r"(?i)(?:^|[^A-Za-z0-9?-])\?r\b").unwrap();
    for m in short_re.find_iter(&cleaned) {
        let start = m.start()
            + if cleaned[m.start()..].starts_with('?') {
                0
            } else {
                1 // skip the separator char
            };
        commands.push(ParsedCommand {
            command: Command::Ready,
            start,
        });
        // `?r cc @user` — parse a trailing cc
        let rest = &cleaned[m.end()..];
        if let Some(users) = parse_cc_args(rest) {
            commands.push(ParsedCommand {
                command: Command::Cc { users },
                start,
            });
        }
    }

    commands.sort_by_key(|c| c.start);
    commands
}

/// Parse the verb (and arguments) following an `@bot` mention.
fn parse_verb(bot_name: &str, rest: &str) -> Option<Command> {
    let trimmed = rest.trim_start();

    // Split on the first whitespace using offsets from the ORIGINAL string.
    // `to_lowercase()` is not length-preserving in UTF-8 (`K` U+212A is 3 bytes,
    // `k` is 1), so an offset found in a lowercased copy can land mid-codepoint
    // here and panic. Verbs are ASCII, so ASCII-lowercase for comparison only.
    let (word_raw, args) = match trimmed.char_indices().find(|(_, c)| c.is_whitespace()) {
        Some((i, _)) => (&trimmed[..i], trimmed[i..].trim()),
        None => (trimmed, ""),
    };
    let word = word_raw.to_ascii_lowercase();

    match word.as_str() {
        "ping" => Some(Command::Ping),
        "help" | "commands" => Some(Command::Help),
        "review" => {
            // `@xero review` = AI review; `@rustbot review` was an alias for
            // ready in triagebot, but for this bot review is the primary verb.
            Some(Command::Review)
        }
        "cc" => {
            let users = parse_users(args);
            if users.is_empty() {
                None
            } else {
                Some(Command::Cc { users })
            }
        }
        "ready" | "reviewer" => Some(Command::Ready),
        "author" => Some(Command::Author),
        "blocked" => Some(Command::Blocked),
        "label" | "relabel" => {
            let (add, remove) = parse_label_args(args);
            if add.is_empty() && remove.is_empty() {
                None
            } else {
                Some(Command::Label { add, remove })
            }
        }
        "assign" => {
            let users = parse_users(args);
            users
                .into_iter()
                .next()
                .map(|user| Command::Assign { user })
        }
        "claim" => Some(Command::Claim),
        "unclaim" | "release-assignment" | "release" => Some(Command::Unclaim),
        "r+" => {
            // `@xero r+` or `@xero r+ as @user` or `@xero r+ @user`
            let on_behalf_of = parse_r_assignee(args);
            Some(Command::Approve { on_behalf_of })
        }
        "r-" => Some(Command::Reject),
        "codeql" => Some(Command::Codeql),
        _ => {
            // `r? @user` also valid right after mention: `@xero r? @user`
            if word == "r?" {
                let users = parse_users(args);
                users
                    .into_iter()
                    .next()
                    .map(|user| Command::RequestReview { user })
                    .or(Some(Command::RequestReview {
                        user: bot_name.to_string(), // self-request: ignore
                    }))
            } else {
                None
            }
        }
    }
}

/// `r+ as @user` / `r+ @user` / `r+=user` — the bors `r=` credit form.
fn parse_r_assignee(args: &str) -> Option<String> {
    let args = args.trim();
    if args.is_empty() {
        return None;
    }
    // "as @user" or "as user"
    if let Some(rest) = args.to_lowercase().strip_prefix("as ") {
        return parse_users(rest).into_iter().next();
    }
    parse_users(args).into_iter().next()
}

/// Collect @mentions from a whitespace-separated arg string.
fn parse_users(args: &str) -> Vec<String> {
    args.split_whitespace()
        .filter_map(|t| {
            let t = t.trim();
            if let Some(u) = t.strip_prefix('@') {
                if is_valid_login(u) {
                    return Some(u.to_string());
                }
            }
            None
        })
        .collect()
}

/// For `?r cc @user1 @user2` — match "cc" followed by at least one @user.
fn parse_cc_args(rest: &str) -> Option<Vec<String>> {
    let trimmed = rest.trim_start();
    // `get(..2)` yields None unless byte 2 is a char boundary, so the slice
    // below can't split a codepoint. Same hazard as parse_verb.
    let Some(prefix) = trimmed.get(..2) else {
        return None;
    };
    if !prefix.eq_ignore_ascii_case("cc") {
        return None;
    }
    let after_raw = &trimmed[2..];
    let users = parse_users(after_raw);
    if users.is_empty() {
        None
    } else {
        Some(users)
    }
}

/// `+label -label` tokens (also `label: +x` style not supported; keep triagebot's).
fn parse_label_args(args: &str) -> (Vec<String>, Vec<String>) {
    let mut add = Vec::new();
    let mut remove = Vec::new();
    for token in args.split_whitespace() {
        if let Some(name) = token.strip_prefix('+') {
            if is_valid_label(name) {
                add.push(name.to_string());
            }
        } else if let Some(name) = token.strip_prefix('-') {
            if is_valid_label(name) {
                remove.push(name.to_string());
            }
        }
    }
    (add, remove)
}

fn is_valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn is_valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s
            .chars()
            .any(|c| ['"', ',', ';', ':', '\\', '<', '>', '|'].contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_command() {
        let cmds = parse_commands("xero-review", "@xero-review review");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Review));
    }

    #[test]
    fn test_review_case_insensitive_and_mid_comment() {
        let cmds = parse_commands("xero-review", "fix the bug\n@XERO-REVIEW   Review please");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Review));
    }

    #[test]
    fn test_bot_suffix() {
        let cmds = parse_commands("xero-review", "@xero-review[bot] ping");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Ping));
    }

    #[test]
    fn test_ping_help() {
        let cmds = parse_commands("xero-review", "@xero-review ping");
        assert!(matches!(cmds[0].command, Command::Ping));
        let cmds = parse_commands("xero-review", "@xero-review help");
        assert!(matches!(cmds[0].command, Command::Help));
    }

    #[test]
    fn test_r_question_bare() {
        let cmds = parse_commands("xero-review", "r? @octocat");
        assert_eq!(cmds.len(), 1);
        match &cmds[0].command {
            Command::RequestReview { user } => assert_eq!(user, "octocat"),
            other => panic!("expected RequestReview, got {other:?}"),
        }
    }

    #[test]
    fn test_r_question_no_at() {
        let cmds = parse_commands("xero-review", "thanks! r? octocat");
        assert_eq!(cmds.len(), 1);
        match &cmds[0].command {
            Command::RequestReview { user } => assert_eq!(user, "octocat"),
            other => panic!("expected RequestReview, got {other:?}"),
        }
    }

    #[test]
    fn test_r_question_after_mention() {
        let cmds = parse_commands("xero-review", "@xero-review r? @alice");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(&cmds[0].command, Command::RequestReview { user } if user == "alice"));
    }

    #[test]
    fn test_short_r() {
        let cmds = parse_commands("xero-review", "?r");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Ready));
    }

    #[test]
    fn test_short_r_cc() {
        let cmds = parse_commands("xero-review", "?r cc @alice @bob");
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0].command, Command::Ready));
        match &cmds[1].command {
            Command::Cc { users } => {
                assert_eq!(users, &vec!["alice".to_string(), "bob".to_string()])
            }
            other => panic!("expected Cc, got {other:?}"),
        }
    }

    #[test]
    fn test_ready_author_blocked() {
        for (text, want) in [
            ("@xero-review ready", Command::Ready),
            ("@xero-review reviewer", Command::Ready),
            ("@xero-review author", Command::Author),
            ("@xero-review blocked", Command::Blocked),
        ] {
            let cmds = parse_commands("xero-review", text);
            assert_eq!(cmds.len(), 1, "for {text}");
            assert_eq!(cmds[0].command, want);
        }
    }

    #[test]
    fn test_labels() {
        let cmds = parse_commands("xero-review", "@xero-review label +bug +A-compiler -wip");
        match &cmds[0].command {
            Command::Label { add, remove } => {
                assert_eq!(add, &vec!["bug".to_string(), "A-compiler".to_string()]);
                assert_eq!(remove, &vec!["wip".to_string()]);
            }
            other => panic!("expected Label, got {other:?}"),
        }
    }

    #[test]
    fn test_assign_claim() {
        let cmds = parse_commands("xero-review", "@xero-review assign @octocat");
        assert!(matches!(&cmds[0].command, Command::Assign { user } if user == "octocat"));
        let cmds = parse_commands("xero-review", "@xero-review claim");
        assert!(matches!(cmds[0].command, Command::Claim));
        let cmds = parse_commands("xero-review", "@xero-review unclaim");
        assert!(matches!(cmds[0].command, Command::Unclaim));
        let cmds = parse_commands("xero-review", "@xero-review release-assignment");
        assert!(matches!(cmds[0].command, Command::Unclaim));
    }

    #[test]
    fn test_r_plus_minus() {
        let cmds = parse_commands("xero-review", "@xero-review r+");
        assert!(matches!(
            &cmds[0].command,
            Command::Approve { on_behalf_of: None }
        ));
        let cmds = parse_commands("xero-review", "@xero-review r+ as @octocat");
        assert!(
            matches!(&cmds[0].command, Command::Approve { on_behalf_of: Some(u) } if u == "octocat")
        );
        let cmds = parse_commands("xero-review", "@xero-review r-");
        assert!(matches!(cmds[0].command, Command::Reject));
    }

    #[test]
    fn test_multiple_commands() {
        let cmds = parse_commands(
            "xero-review",
            "r? @alice and @xero-review label +bug -wip then @xero-review ping",
        );
        assert_eq!(cmds.len(), 3);
        assert!(matches!(cmds[0].command, Command::RequestReview { .. }));
        assert!(matches!(cmds[1].command, Command::Label { .. }));
        assert!(matches!(cmds[2].command, Command::Ping));
    }

    #[test]
    fn test_code_blocks_ignored() {
        let cmds = parse_commands(
            "xero-review",
            "example:\n```\n@xero-review label +bug\n```\n@xero-review ping",
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Ping));
    }

    #[test]
    fn test_r_question_inside_code_block_ignored() {
        let cmds = parse_commands("xero-review", "```\nr? @octocat\n```");
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_unknown_verb_ignored() {
        let cmds = parse_commands("xero-review", "@xero-review do the thing please");
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_plain_mention_no_verb() {
        let cmds = parse_commands("xero-review", "cc @xero-review about this");
        assert!(cmds.is_empty());
    }

    #[test]
    fn test_codeql() {
        let cmds = parse_commands("xero-review", "@xero-review codeql");
        assert!(matches!(cmds[0].command, Command::Codeql));
    }

    /// `to_lowercase()` is not length-preserving in UTF-8, so a byte offset found
    /// in a lowercased copy could land mid-codepoint in the original and panic.
    /// Each of these shrinks or grows when lowercased:
    ///   U+212A KELVIN SIGN  3 bytes -> `k` 1 byte
    ///   U+2126 OHM SIGN     3 bytes -> `ω` 2 bytes
    ///   U+0130 İ            2 bytes -> `i̇` 3 bytes
    /// Reachable from any comment body, and the webhook has no catch layer, so a
    /// panic here used to take down the response and trigger GitHub redelivery.
    #[test]
    fn test_parse_verb_is_utf8_safe() {
        let nasty = [
            "\u{212A}", "\u{2126}", "\u{212B}", "\u{130}", "\u{1E9E}", "\u{FB00}", "就绪", "🎉",
        ];
        for s in nasty {
            for body in [
                // as the verb itself
                format!("@xero-review {s} x"),
                format!("@xero-review {s}"),
                // as the first argument
                format!("@xero-review cc {s} @alice"),
                format!("@xero-review assign {s}"),
                format!("@xero-review label {s} +bug"),
                // doubled, then followed by multibyte text
                format!("@xero-review {s}{s} 中文"),
                // after a valid verb
                format!("@xero-review review {s}"),
                format!("@xero-review r+ as {s}"),
                // bare anchors
                format!("r? {s}"),
                format!("?r cc {s}"),
                format!("{s}?r"),
            ] {
                // must not panic; result content is irrelevant here
                let _ = parse_commands("xero-review", &body);
            }
        }
    }

    /// Mixed-script bodies with no command at all must parse cleanly.
    #[test]
    fn test_prose_with_multibyte_is_inert() {
        for body in [
            "这个 PR 看起来不错,谢谢!🎉",
            "@xero-review 谢谢你的审查",
            "Ω K Å İ ẞ ﬀ",
            "r?",
            "?r",
        ] {
            let _ = parse_commands("xero-review", body);
        }
    }
}
