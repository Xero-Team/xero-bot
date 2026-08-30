//! Which language the bot answers in.
//!
//! Every reply used to be Chinese, which is wrong on an English-speaking PR and
//! was never a decision anyone made — it's just what got typed first. The PR's
//! own commit subjects are the signal: they're written by the people who will
//! read the reply, they exist before the bot says anything, and they need no
//! configuration.
//!
//! Only English and Chinese are modelled. Anything else — Japanese, Korean,
//! Cyrillic, a repo of pure `bump deps` — reads as no signal and falls back to
//! English, which is the safer default for a public repo.

use serde_json::Value;

use crate::github::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Zh,
}

impl Lang {
    /// Choose between two ready-made values. Used for replies with no
    /// interpolation; [`crate::t`] covers the rest.
    pub fn pick<T>(self, en: T, zh: T) -> T {
        match self {
            Lang::En => en,
            Lang::Zh => zh,
        }
    }

    /// The one line every review prompt needs: which language the model should
    /// write its prose fields in. The rest of a prompt is an instruction to the
    /// model, not a reply to a human, so it stays as written.
    pub fn output_rule(self) -> &'static str {
        match self {
            Lang::En => "Write `summary`, `description` and `suggestion` in English.",
            Lang::Zh => "用中文输出 summary、description 和 suggestion。",
        }
    }
}

/// Pick one of two format strings by language.
///
/// Both arms are `format!` literals, so inline captures (`{user}`) work in each
/// and the two wordings sit on adjacent lines where a translation drift is
/// visible in review. For literals with nothing to interpolate use
/// [`Lang::pick`] instead — `format!` on a bare literal is a clippy warning.
#[macro_export]
macro_rules! t {
    ($lang:expr, $en:literal, $zh:literal) => {
        match $lang {
            $crate::lang::Lang::En => format!($en),
            $crate::lang::Lang::Zh => format!($zh),
        }
    };
}

/// True for the Han ideographs that actually turn up in commit messages.
///
/// Kana and Hangul are deliberately absent, so text written in them counts as
/// no signal and lands on English rather than being read as Chinese. Japanese
/// *kanji* is a different matter — it is the same code points as Chinese and
/// cannot be told apart here, so a kanji-heavy Japanese subject will be read as
/// Chinese. Accepted: distinguishing them needs a real language model, and the
/// two languages this bot can actually write are English and Chinese.
fn is_han(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF      // CJK extension A
        | 0x4E00..=0x9FFF    // CJK unified ideographs
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0x20000..=0x2FA1F  // extensions B–F
    )
}

/// Classify one piece of text, or `None` when it says nothing either way.
///
/// Ideographs are counted against *words*, not letters: both are roughly one
/// morpheme, whereas `zh_chars` against `en_chars` would call
/// `fix: 修复解析器` Chinese by a factor of two on every conventional commit.
pub fn detect(text: &str) -> Option<Lang> {
    let mut zh = 0usize;
    let mut en = 0usize;
    let mut letters = 0usize;
    for c in text.chars() {
        if is_han(c) {
            zh += 1;
            letters = 0;
        } else if c.is_ascii_alphabetic() {
            letters += 1;
            // A word counts on the letter that takes it to two, so `v2 -> v3`
            // abstains instead of voting English. One-letter English words are
            // rare in a commit subject and never the deciding signal, while
            // version tags and single-letter identifiers are common — and a
            // spurious English vote is what would tip a mixed Chinese PR.
            if letters == 2 {
                en += 1;
            }
        } else if c != '\'' {
            // an apostrophe keeps a word whole, so `don't` counts once
            letters = 0;
        }
    }
    if zh == 0 && en == 0 {
        None
    } else if zh > en {
        Some(Lang::Zh)
    } else {
        // A tie goes to English, per the rule that anything undecidable does.
        Some(Lang::En)
    }
}

/// Everything before the first newline.
///
/// Only the subject is read. Bodies carry `Signed-off-by`, `Co-authored-by`,
/// issue URLs and pasted logs — all English — so a Chinese PR whose authors
/// use trailers would be answered in English if the whole message counted.
fn subject(message: &str) -> &str {
    message.split('\n').next().unwrap_or("").trim()
}

/// Majority vote over commit subjects, or `None` if none of them said anything.
///
/// One vote per commit rather than a sum of characters: "if most are English"
/// is about commits, and voting stops a single long message from deciding for
/// the rest.
pub fn from_commits(commits: &[Value]) -> Option<Lang> {
    let mut zh = 0usize;
    let mut en = 0usize;
    for c in commits {
        let message = c
            .get("commit")
            .and_then(|c| c.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        match detect(subject(message)) {
            Some(Lang::Zh) => zh += 1,
            Some(Lang::En) => en += 1,
            None => {}
        }
    }
    if zh == 0 && en == 0 {
        None
    } else if zh > en {
        Some(Lang::Zh)
    } else {
        Some(Lang::En)
    }
}

/// The language to answer a PR in: its commits, then `fallback`, then English.
///
/// `fallback` is normally what the triggering comment was written in — a
/// weaker signal than the commits (a Chinese drive-by comment on an English
/// repo shouldn't flip the review), but better than a coin toss when the
/// commits are all `bump deps`.
pub async fn for_pr(gh: &Client, repo: &str, pr: i64, fallback: Option<Lang>) -> Lang {
    match gh.list_pr_commits(repo, pr).await {
        Ok(commits) => from_commits(&commits).or(fallback).unwrap_or_default(),
        Err(e) => {
            // Not worth failing a command over; the fallback is honest.
            tracing::warn!("reply language: cannot read commits of {repo}#{pr}: {e}");
            fallback.unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_cases() {
        assert_eq!(detect("fix: resolve the App's real slug"), Some(Lang::En));
        assert_eq!(detect("修复解析器崩溃"), Some(Lang::Zh));
    }

    /// Conventional-commit prefixes are English even in Chinese repos, so the
    /// counting must not let `fix:` outvote the actual subject.
    #[test]
    fn conventional_prefix_does_not_decide() {
        assert_eq!(detect("fix: 修复解析器崩溃"), Some(Lang::Zh));
        assert_eq!(detect("docs: 补充说明"), Some(Lang::Zh));
        assert_eq!(detect("refactor(commands): 拆分词法与语法"), Some(Lang::Zh));
    }

    /// Nothing to go on — the caller falls back to English.
    #[test]
    fn no_signal_is_none() {
        for text in ["", "1.2.3", "🎉🎉", "   ", "v2 -> v3 (#41)"] {
            assert_eq!(detect(text), None, "for {text:?}");
        }
    }

    /// Scripts that are neither language read as no signal, so the caller lands
    /// on English. Kanji is the documented exception — it is the same code
    /// points as Chinese.
    #[test]
    fn other_scripts_fall_back_to_english() {
        assert_eq!(detect("バグをしゅうせい"), None);
        assert_eq!(detect("バグを修正"), Some(Lang::Zh)); // kanji: indistinguishable
        assert_eq!(detect("버그 수정"), None);
        assert_eq!(detect("исправить ошибку"), None);
    }

    #[test]
    fn ties_go_to_english() {
        // two words against two ideographs
        assert_eq!(detect("支持 dark mode"), Some(Lang::En));
        assert_eq!(detect("add docs 支持"), Some(Lang::En));
        // and one word against two ideographs is Chinese, not a tie
        assert_eq!(detect("add 支持"), Some(Lang::Zh));
    }

    /// A one-letter run isn't a word. `v2 -> v3` says nothing about the
    /// author's language, and letting it vote English is what would decide a
    /// mixed Chinese PR the wrong way.
    #[test]
    fn single_letters_do_not_vote() {
        assert_eq!(detect("v2 -> v3"), None);
        assert_eq!(detect("a b c d e"), None);
        assert_eq!(detect("修复 v2 的崩溃"), Some(Lang::Zh));
        // two commits, one of them noise: the Chinese one still wins
        assert_eq!(
            from_commits(&commits(&["修复崩溃", "v2 -> v3"])),
            Some(Lang::Zh)
        );
    }

    fn commits(subjects: &[&str]) -> Vec<Value> {
        subjects
            .iter()
            .map(|s| json!({"commit": {"message": s}}))
            .collect()
    }

    #[test]
    fn majority_of_commits_wins() {
        assert_eq!(
            from_commits(&commits(&["修复崩溃", "补充测试", "fix: typo"])),
            Some(Lang::Zh)
        );
        assert_eq!(
            from_commits(&commits(&["add parser", "fix lexer", "修复崩溃"])),
            Some(Lang::En)
        );
        // an even split is undecidable, so English
        assert_eq!(
            from_commits(&commits(&["add parser", "修复崩溃"])),
            Some(Lang::En)
        );
    }

    /// English trailers must not outvote a Chinese subject — only the first
    /// line of each message is read.
    #[test]
    fn only_the_subject_line_counts() {
        let c = commits(&[
            "修复解析器崩溃\n\nThe parser sliced a lowercased copy of the text.\n\nSigned-off-by: Someone <a@b.c>\nCo-authored-by: Other <d@e.f>",
        ]);
        assert_eq!(from_commits(&c), Some(Lang::Zh));
    }

    #[test]
    fn commits_without_signal_yield_none() {
        assert_eq!(from_commits(&commits(&["1.2.3", "🎉"])), None);
        assert_eq!(from_commits(&[]), None);
        // a malformed commit object is skipped, not counted
        assert_eq!(from_commits(&[json!({"sha": "abc"})]), None);
    }

    #[test]
    fn pick_and_t_agree_on_the_language() {
        let user = "alice";
        assert_eq!(Lang::En.pick("pong", "乒"), "pong");
        assert_eq!(
            t!(Lang::Zh, "Assigned @{user}.", "已指派给 @{user}。"),
            "已指派给 @alice。"
        );
    }
}
