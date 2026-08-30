//! Comment command parser.
//!
//! Mirrors rust-lang/triagebot semantics:
//! - `@xero <verb>` commands, case-insensitive, anywhere in the comment
//! - fenced code blocks are ignored (no false triggers)
//! - multiple commands per comment, executed in order
//!
//! Unknown text after a valid mention is not an error — the bot simply ignores
//! comments it doesn't understand (triagebot posts an error; we stay quiet to
//! avoid noise, matching the original Python bot's behavior).

use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Ping,
    Help,
    /// `r? @user` — assign reviewer
    RequestReview {
        user: String,
    },
}

/// One parsed command plus where it appeared (for ordered execution).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommand {
    pub command: Command,
    pub start: usize,
}

/// Strip fenced code blocks from a comment, replacing them with spaces so
/// character offsets stay stable (we don't need offsets to survive, but it
/// keeps command detection from matching inside examples).
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

    commands.sort_by_key(|c| c.start);
    commands
}

/// Parse the verb (and arguments) following an `@bot` mention.
fn parse_verb(bot_name: &str, rest: &str) -> Option<Command> {
    let trimmed = rest.trim_start();
    let lower = trimmed.to_lowercase();

    // word + remainder (first whitespace boundary)
    let (word, args) = match lower.find(char::is_whitespace) {
        Some(i) => (&lower[..i], trimmed[i..].trim()),
        None => (lower.as_str(), ""),
    };

    match word {
        "ping" => Some(Command::Ping),
        "help" | "commands" => Some(Command::Help),
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

fn is_valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ping_help() {
        let cmds = parse_commands("xero-review", "@xero-review ping");
        assert!(matches!(cmds[0].command, Command::Ping));
        let cmds = parse_commands("xero-review", "@xero-review help");
        assert!(matches!(cmds[0].command, Command::Help));
    }

    #[test]
    fn test_bot_suffix() {
        let cmds = parse_commands("xero-review", "@xero-review[bot] ping");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Ping));
    }

    #[test]
    fn test_case_insensitive_mid_comment() {
        let cmds = parse_commands("xero-review", "fix the bug\n@XERO-REVIEW   ping please");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Ping));
    }

    #[test]
    fn test_code_blocks_ignored() {
        let cmds = parse_commands(
            "xero-review",
            "example:\n```\n@xero-review ping\n```\n@xero-review help",
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0].command, Command::Help));
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
    fn test_r_question_inside_code_block_ignored() {
        let cmds = parse_commands("xero-review", "```\nr? @octocat\n```");
        assert!(cmds.is_empty());
    }
}
