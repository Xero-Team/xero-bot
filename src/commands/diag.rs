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
use crate::lang::Lang;
use crate::t;

/// What an argument slot wanted.
///
/// An enum rather than the sentence itself, so the wording is chosen when the
/// reply is written — the parser has no idea which language the PR speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// exactly one login, as `assign` takes
    User,
    /// one or more logins, as `cc` takes
    Users,
    /// one or more `+label` / `-label`
    Labels,
}

impl Expected {
    fn describe(self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Expected::User, Lang::En) => "one @username",
            (Expected::User, Lang::Zh) => "一个 @用户名",
            (Expected::Users, Lang::En) => "at least one @username",
            (Expected::Users, Lang::Zh) => "至少一个 @用户名",
            (Expected::Labels, Lang::En) => "at least one `+label` or `-label`",
            (Expected::Labels, Lang::Zh) => "至少一个 `+标签` 或 `-标签`",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Diagnostic {
    UnknownVerb {
        verb: String,
        suggestion: Option<&'static str>,
        span: Range<usize>,
    },
    MissingArgument {
        verb: &'static str,
        expected: Expected,
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

    /// One line of user-facing explanation, in the PR's language.
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Diagnostic::UnknownVerb {
                verb,
                suggestion: Some(s),
                ..
            } => t!(
                lang,
                "`{verb}` is not a command — did you mean `{s}`?",
                "`{verb}` 不是命令,是否想用 `{s}`?"
            ),
            Diagnostic::UnknownVerb {
                verb,
                suggestion: None,
                ..
            } => t!(lang, "`{verb}` is not a command.", "`{verb}` 不是命令。"),
            Diagnostic::MissingArgument { verb, expected, .. } => {
                let what = expected.describe(lang);
                t!(lang, "`{verb}` needs {what}.", "`{verb}` 需要{what}。")
            }
            Diagnostic::ExtraArguments { verb, .. } => t!(
                lang,
                "`{verb}` takes no arguments; what followed it was ignored.",
                "`{verb}` 不接受参数,后面的内容被忽略了。"
            ),
            Diagnostic::InvalidLogin { raw, .. } => t!(
                lang,
                "`@{raw}` is not a valid username (letters, digits and hyphens only).",
                "`@{raw}` 不是合法的用户名(只能是字母、数字和连字符)。"
            ),
            Diagnostic::InvalidLabel { raw, .. } => t!(
                lang,
                "`{raw}` is not a valid label name.",
                "`{raw}` 不是合法的标签名。"
            ),
            Diagnostic::ConflictingStatus { kept, dropped } => {
                let d = dropped
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(lang.pick(", ", "、"));
                t!(
                    lang,
                    "{d} conflicts with `{kept}`; kept `{kept}`.",
                    "{d} 与 `{kept}` 冲突,已采用 `{kept}`。"
                )
            }
            Diagnostic::SelfRequestIgnored { .. } => lang
                .pick(
                    "The bot can't review on its own request; ignored.",
                    "不能请 bot 自己审查,已忽略。",
                )
                .to_string(),
            Diagnostic::DuplicateCommand { what, times } => t!(
                lang,
                "`{what}` appeared {times} times; running it once.",
                "`{what}` 出现了 {times} 次,只执行一次。"
            ),
        }
    }
}

/// Render diagnostics as one comment, or None when there's nothing to say.
pub fn render(diags: &[Diagnostic], bot_name: &str, lang: Lang) -> Option<String> {
    let messages: Vec<String> = diags.iter().map(|d| d.message(lang)).collect();
    render_messages(&messages, bot_name, lang)
}

/// Assemble already-rendered messages into one comment body.
///
/// Separate from [`render`] so the dispatch layer can carry plain strings —
/// `Work` stays free of parser types — while the wording lives in one place.
pub fn render_messages(messages: &[String], bot_name: &str, lang: Lang) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    let n = messages.len();
    let mut out = t!(
        lang,
        "⚠️ {n} thing(s) I didn't understand:\n\n",
        "⚠️ 有 {n} 处没看懂:\n\n"
    );
    for m in messages {
        out.push_str("- ");
        out.push_str(m);
        out.push('\n');
    }
    out.push_str(&t!(
        lang,
        "\nFull command list: `@{bot_name} help`",
        "\n完整命令表: `@{bot_name} help`"
    ));
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
        assert_eq!(render(&[], "bot", Lang::Zh), None);
        assert_eq!(render(&[], "bot", Lang::En), None);
    }

    #[test]
    fn render_counts_what_it_found() {
        let one = render_messages(&["a".to_string()], "bot", Lang::Zh).expect("some");
        assert!(one.contains("有 1 处"), "{one}");
        let two =
            render_messages(&["a".to_string(), "b".to_string()], "bot", Lang::Zh).expect("some");
        assert!(two.contains("有 2 处"), "{two}");
        let en =
            render_messages(&["a".to_string(), "b".to_string()], "bot", Lang::En).expect("some");
        assert!(en.contains("2 thing"), "{en}");
        assert_eq!(render_messages(&[], "bot", Lang::Zh), None);
    }

    #[test]
    fn render_lists_every_diagnostic() {
        let d = vec![
            Diagnostic::unknown_verb("reviwe".into(), 0..6),
            Diagnostic::MissingArgument {
                verb: "label",
                expected: Expected::Labels,
                span: 0..0,
            },
        ];
        let out = render(&d, "xero-review", Lang::Zh).expect("some");
        assert!(out.contains("是否想用 `review`"), "{out}");
        assert!(out.contains("`label` 需要"), "{out}");
        assert!(out.contains("@xero-review help"), "{out}");
    }

    /// The same diagnostics in English, with no Chinese left anywhere in the
    /// body — a half-translated reply is worse than either language.
    #[test]
    fn english_render_has_no_chinese_left() {
        let d = vec![
            Diagnostic::unknown_verb("reviwe".into(), 0..6),
            Diagnostic::MissingArgument {
                verb: "label",
                expected: Expected::Labels,
                span: 0..0,
            },
            Diagnostic::MissingArgument {
                verb: "assign",
                expected: Expected::User,
                span: 0..0,
            },
            Diagnostic::ExtraArguments {
                verb: "ping",
                span: 0..0,
            },
            Diagnostic::InvalidLogin {
                raw: "-bad".into(),
                span: 0..0,
            },
            Diagnostic::InvalidLabel {
                raw: "a`b".into(),
                span: 0..0,
            },
            Diagnostic::ConflictingStatus {
                kept: "blocked",
                dropped: vec!["ready", "author"],
            },
            Diagnostic::SelfRequestIgnored { span: 0..0 },
            Diagnostic::DuplicateCommand {
                what: "review".into(),
                times: 2,
            },
        ];
        let out = render(&d, "xero-review", Lang::En).expect("some");
        assert!(out.contains("did you mean `review`"), "{out}");
        assert!(out.contains("`label` needs"), "{out}");
        assert!(
            !out.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "Chinese left in an English reply: {out}"
        );
    }
}
