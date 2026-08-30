//! Stage 4: semantic pass over the parsed commands.
//!
//! Pure — no API calls — so the policies here are cheap to unit test.

use super::diag::Diagnostic;
use super::parse::ParsedCommand;
use super::Command;
use crate::github::normalize_login;

/// Apply within-comment policy: drop self-requests, collapse duplicates, and
/// resolve contradictory status labels.
pub fn resolve(
    bot_name: &str,
    mut parsed: Vec<ParsedCommand>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<Command> {
    parsed.sort_by_key(|c| c.start);

    let me = normalize_login(bot_name);
    parsed.retain(|c| match &c.command {
        Command::RequestReview { user } | Command::Assign { user }
            if normalize_login(user) == me =>
        {
            diagnostics.push(Diagnostic::SelfRequestIgnored {
                span: c.start..c.start,
            });
            false
        }
        _ => true,
    });

    // Ready / Author / Blocked are mutually exclusive: each clears the others'
    // labels, so running two in one comment did both operations and posted two
    // contradictory replies. Keep the last one the user wrote.
    let status_positions: Vec<usize> = parsed
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            matches!(
                c.command,
                Command::Ready | Command::Author | Command::Blocked
            )
        })
        .map(|(i, _)| i)
        .collect();
    if status_positions.len() > 1 {
        let keep = *status_positions.last().unwrap();
        let kept = status_name(&parsed[keep].command);
        let dropped: Vec<&'static str> = status_positions
            .iter()
            .filter(|i| **i != keep)
            .map(|i| status_name(&parsed[*i].command))
            .collect();
        diagnostics.push(Diagnostic::ConflictingStatus { kept, dropped });
        let drop_set: std::collections::HashSet<usize> = status_positions
            .into_iter()
            .filter(|i| *i != keep)
            .collect();
        parsed = parsed
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !drop_set.contains(i))
            .map(|(_, c)| c)
            .collect();
    }

    // Collapse exact repeats. `@bot review` twice is a duplicated model spend,
    // not two requests.
    let mut out: Vec<Command> = Vec::new();
    let mut dupes: Vec<(String, usize)> = Vec::new();
    for c in parsed {
        if out.contains(&c.command) {
            let name = command_name(&c.command);
            match dupes.iter_mut().find(|(n, _)| *n == name) {
                Some((_, n)) => *n += 1,
                None => dupes.push((name, 2)),
            }
            continue;
        }
        out.push(c.command);
    }
    for (what, times) in dupes {
        diagnostics.push(Diagnostic::DuplicateCommand { what, times });
    }

    out
}

fn status_name(c: &Command) -> &'static str {
    match c {
        Command::Ready => "ready",
        Command::Author => "author",
        Command::Blocked => "blocked",
        _ => "status",
    }
}

fn command_name(c: &Command) -> String {
    match c {
        Command::Ping => "ping".into(),
        Command::Help => "help".into(),
        Command::Review => "review".into(),
        Command::Codeql => "codeql".into(),
        Command::Ready => "ready".into(),
        Command::Author => "author".into(),
        Command::Blocked => "blocked".into(),
        Command::Claim => "claim".into(),
        Command::Unclaim => "unclaim".into(),
        Command::Reject => "r-".into(),
        Command::Approve { .. } => "r+".into(),
        Command::RequestReview { user } => format!("r? @{user}"),
        Command::Assign { user } => format!("assign @{user}"),
        Command::Cc { .. } => "cc".into(),
        Command::Label { .. } => "label".into(),
    }
}
