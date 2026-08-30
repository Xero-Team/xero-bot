//! Compiler diagnostics.
//!
//! The old parser dropped anything it didn't understand, so `@bot assign
//! @alice。` — a full-width period away from correct — did nothing at all and
//! left the user believing it had worked. Naming the problem is the point.
//!
//! Silence is still the default for prose. `@bot 谢谢!` must not draw a reply;
//! only something that looks like a failed command does. That decision lives in
//! the parser, which only emits `UnknownVerb` for a word directly after a
//! mention.

use std::ops::Range;

use super::parse::VERBS;

#[derive(Debug, Clone, PartialEq)]
pub enum Diagnostic {
    UnknownVerb {
        verb: String,
        suggestion: Option<&'static str>,
        span: Range<usize>,
    },
    MissingArgument {
        verb: &'static str,
        expected: &'static str,
        span: Range<usize>,
    },
    ExtraArguments {
        verb: &'static str,
        span: Range<usize>,
    },
    InvalidLogin {
        raw: String,
        span: Range<usize>,
    },
    InvalidLabel {
        raw: String,
        span: Range<usize>,
    },
    ConflictingStatus {
        kept: &'static str,
        dropped: Vec<&'static str>,
    },
    SelfRequestIgnored {
        span: Range<usize>,
    },
    DuplicateCommand {
        what: String,
        times: usize,
    },
}

impl Diagnostic {
    pub fn unknown_verb(verb: String, span: Range<usize>) -> Diagnostic {
        let suggestion = suggest(&verb);
        Diagnostic::UnknownVerb {
            verb,
            suggestion,
            span,
        }
    }

    /// One line of user-facing explanation.
    pub fn message(&self) -> String {
        match self {
            Diagnostic::UnknownVerb {
                verb,
                suggestion: Some(s),
                ..
            } => format!("`{verb}` 不是命令,是否想用 `{s}`?"),
            Diagnostic::UnknownVerb {
                verb,
                suggestion: None,
                ..
            } => format!("`{verb}` 不是命令。"),
            Diagnostic::MissingArgument { verb, expected, .. } => {
                format!("`{verb}` 需要{expected}。")
            }
            Diagnostic::ExtraArguments { verb, .. } => {
                format!("`{verb}` 不接受参数,后面的内容被忽略了。")
            }
            Diagnostic::InvalidLogin { raw, .. } => {
                format!("`@{raw}` 不是合法的用户名(只能是字母、数字和连字符)。")
            }
            Diagnostic::InvalidLabel { raw, .. } => {
                format!("`{raw}` 不是合法的标签名。")
            }
            Diagnostic::ConflictingStatus { kept, dropped } => {
                let d = dropped
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join("、");
                format!("{d} 与 `{kept}` 冲突,已采用 `{kept}`。")
            }
            Diagnostic::SelfRequestIgnored { .. } => {
                "不能请 bot 自己审查,已忽略。".to_string()
            }
            Diagnostic::DuplicateCommand { what, times } => {
                format!("`{what}` 出现了 {times} 次,只执行一次。")
            }
        }
    }
}

/// Render diagnostics as one comment, or None when there's nothing to say.
pub fn render(diags: &[Diagnostic], bot_name: &str) -> Option<String> {
    let messages: Vec<String> = diags.iter().map(Diagnostic::message).collect();
    render_messages(&messages, bot_name)
}

/// Assemble already-rendered messages into one comment body.
///
/// Separate from [`render`] so the dispatch layer can carry plain strings —
/// `Work` stays free of parser types — while the wording lives in one place.
pub fn render_messages(messages: &[String], bot_name: &str) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let mut out = format!("⚠️ 有 {} 处没看懂:\n\n", messages.len());
    for m in messages {
        out.push_str("- ");
        out.push_str(m);
        out.push('\n');
    }
    out.push_str(&format!("\n完整命令表: `@{bot_name} help`"));
    Some(out)
}

/// Closest known verb within edit distance 2.
fn suggest(word: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for v in VERBS {
        let d = edit_distance(word, v);
        if d <= 2 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, v));
        }
    }
    best.map(|(_, v)| v)
}

/// Levenshtein distance, two-row form. Hand-rolled to avoid a dependency for
/// twenty lines; inputs here are short verbs.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("reviwe", "review"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn suggests_the_obvious_typo() {
        assert_eq!(suggest("reviwe"), Some("review"));
        assert_eq!(suggest("revew"), Some("review"));
        assert_eq!(suggest("pign"), Some("ping"));
        assert_eq!(suggest("halp"), Some("help"));
        assert_eq!(suggest("labe"), Some("label"));
    }

    #[test]
    fn no_suggestion_for_unrelated_words() {
        assert_eq!(suggest("thanks"), None);
        assert_eq!(suggest("looksgoodtome"), None);
    }

    #[test]
    fn render_is_none_when_there_is_nothing_to_say() {
        assert_eq!(render(&[], "bot"), None);
    }

    #[test]
    fn render_counts_what_it_found() {
        let one = render_messages(&["a".to_string()], "bot").expect("some");
        assert!(one.contains("有 1 处"), "{one}");
        let two = render_messages(&["a".to_string(), "b".to_string()], "bot").expect("some");
        assert!(two.contains("有 2 处"), "{two}");
        assert_eq!(render_messages(&[], "bot"), None);
    }

    #[test]
    fn render_lists_every_diagnostic() {
        let d = vec![
            Diagnostic::unknown_verb("reviwe".into(), 0..6),
            Diagnostic::MissingArgument {
                verb: "label",
                expected: "至少一个 `+标签` 或 `-标签`",
                span: 0..0,
            },
        ];
        let out = render(&d, "xero-review").expect("some");
        assert!(out.contains("是否想用 `review`"), "{out}");
        assert!(out.contains("`label` 需要"), "{out}");
        assert!(out.contains("@xero-review help"), "{out}");
    }
}
