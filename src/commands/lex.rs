//! Stage 2: turn masked comment text into a token stream.
//!
//! Driven entirely by `char_indices`, so no byte arithmetic can split a
//! codepoint. The previous parser found an offset in a `to_lowercase()` copy
//! and sliced the original with it, which panicked on any comment containing
//! e.g. U+212A KELVIN SIGN.
//!
//! Sigils (`r?`, `r+`, `r-`, `?r`) are lexed once, as distinct token kinds.
//! That is what removes the ambiguity the old three-anchor scanner had: there
//! is no longer any way for two anchors to claim the same text.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// `@botname`, optionally with a `[bot]` suffix — starts a command
    Bot,
    /// `@login` that passes [`is_valid_login`]
    User(String),
    /// `@something` that doesn't — kept so diagnostics can explain why
    RawUser(String),
    /// a bare ASCII word; a verb candidate
    Word(String),
    /// `+label`
    Plus(String),
    /// `-label`
    Minus(String),
    /// `r?`
    ReviewReq,
    /// `r+`
    Approve,
    /// `r-`
    Reject,
    /// `?r`
    ShortReady,
    /// the `as` keyword, for `r+ as @user`
    As,
    /// `;` — separates commands sharing one mention
    Semi,
    /// end of line; ends a mention's scope
    Newline,
    /// CJK text, punctuation, emoji — anything that can't start or continue a
    /// command. Adjacent runs collapse into one token.
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Range<usize>,
}

/// GitHub login rules, as far as they matter here: alphanumerics and hyphens,
/// no leading or trailing hyphen, 39 characters max.
pub fn is_valid_login(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 39
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Labels are permissive: GitHub allows spaces, slashes and emoji. Only reject
/// what would break the request or clearly isn't a label. A positive charset
/// would start refusing real labels like `good first issue`.
pub fn is_valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s
            .chars()
            .any(|c| c.is_control() || ['"', ',', ';', ':', '\\', '<', '>', '|', '`'].contains(&c))
}

pub fn lex(bot_name: &str, text: &str) -> Vec<Token> {
    Lexer {
        text,
        bot: bot_name.to_ascii_lowercase(),
        chars: text.char_indices().peekable(),
        out: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    text: &'a str,
    bot: String,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    out: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn run(mut self) -> Vec<Token> {
        while let Some(&(i, c)) = self.chars.peek() {
            match c {
                '\n' => {
                    self.chars.next();
                    self.push(Tok::Newline, i..i + 1);
                }
                ';' => {
                    self.chars.next();
                    self.push(Tok::Semi, i..i + 1);
                }
                c if c.is_whitespace() => {
                    self.chars.next();
                }
                '@' => self.lex_at(i),
                '?' => self.lex_question(i),
                '+' | '-' => self.lex_signed(i, c),
                c if c.is_ascii_alphanumeric() => self.lex_word(i),
                _ => self.lex_other(i),
            }
        }
        self.out
    }

    fn push(&mut self, tok: Tok, span: Range<usize>) {
        self.out.push(Token { tok, span });
    }

    /// Consume while `pred` holds; returns the offset of the first character
    /// that didn't match, or the end of the text. Always a char boundary,
    /// because it only ever reports offsets `char_indices` produced.
    fn take_while(&mut self, mut pred: impl FnMut(char) -> bool) -> usize {
        while let Some(&(j, c)) = self.chars.peek() {
            if !pred(c) {
                return j;
            }
            self.chars.next();
        }
        self.text.len()
    }

    /// `@name`, `@name[bot]` — the bot mention, a user, or an invalid login.
    fn lex_at(&mut self, start: usize) {
        self.chars.next(); // '@'
        let name_start = start + 1;
        let name_end = self.take_while(|c| c.is_ascii_alphanumeric() || c == '-');
        let name = &self.text[name_start..name_end];

        // An optional `[bot]` suffix, consumed case-insensitively. GitHub's
        // mention autocomplete inserts it for Apps. Folding it into the token
        // here is what removed the old scanner's `+ 4` window heuristic.
        // `get(..5)` yields None unless byte 5 is a char boundary, so a name
        // followed by multibyte text can't split a codepoint here.
        let mut end = name_end;
        if self
            .text
            .get(name_end..)
            .and_then(|rest| rest.get(..5))
            .is_some_and(|p| p.eq_ignore_ascii_case("[bot]"))
        {
            for _ in 0..5 {
                self.chars.next();
            }
            end = name_end + 5;
        }

        if name.eq_ignore_ascii_case(&self.bot) {
            self.push(Tok::Bot, start..end);
        } else if is_valid_login(name) {
            self.push(Tok::User(name.to_string()), start..end);
        } else {
            self.push(Tok::RawUser(name.to_string()), start..end);
        }
    }

    /// `?r`, else punctuation.
    fn lex_question(&mut self, start: usize) {
        let is_short_ready = matches!(self.peek_at(start + 1), Some('r' | 'R'))
            && !matches!(self.peek_at(start + 2), Some(c) if c.is_ascii_alphanumeric());
        if is_short_ready {
            self.chars.next(); // '?'
            self.chars.next(); // 'r'
            self.push(Tok::ShortReady, start..start + 2);
        } else {
            self.lex_other(start);
        }
    }

    /// `+label` / `-label`. A lone sign is punctuation.
    fn lex_signed(&mut self, start: usize, sign: char) {
        self.chars.next(); // sign
        let val_start = start + 1;
        let end = self.take_while(|c| !c.is_whitespace() && c != ';' && c != ',');
        let value = &self.text[val_start..end];
        if value.is_empty() {
            self.push(Tok::Other, start..val_start);
            return;
        }
        let tok = if sign == '+' {
            Tok::Plus(value.to_string())
        } else {
            Tok::Minus(value.to_string())
        };
        self.push(tok, start..end);
    }

    /// A word, or one of the `r`-sigils. Sigils are matched maximally and
    /// before words, so `r?` is never split into `r` and `?`.
    fn lex_word(&mut self, start: usize) {
        if matches!(self.peek_at(start), Some('r' | 'R')) {
            if let Some(sigil) = self.peek_at(start + 1) {
                let tok = match sigil {
                    '?' => Some(Tok::ReviewReq),
                    '+' => Some(Tok::Approve),
                    '-' => Some(Tok::Reject),
                    _ => None,
                };
                // `r-fix` is a word, not a Reject followed by junk; require the
                // sigil to end the token.
                let terminated = !matches!(
                    self.peek_at(start + 2),
                    Some(c) if c.is_ascii_alphanumeric() || c == '-'
                );
                if let (Some(tok), true) = (tok, terminated || sigil != '-') {
                    self.chars.next();
                    self.chars.next();
                    self.push(tok, start..start + 2);
                    return;
                }
            }
        }

        let end = self.take_while(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let word = &self.text[start..end];
        if word.eq_ignore_ascii_case("as") {
            self.push(Tok::As, start..end);
        } else {
            self.push(Tok::Word(word.to_ascii_lowercase()), start..end);
        }
    }

    /// A run of anything else, collapsed into one token to keep the stream
    /// short. Trailing punctuation therefore never joins a user or verb token,
    /// which is why `@alice。` yields `User("alice")` and not a dropped command.
    fn lex_other(&mut self, start: usize) {
        // Advance past the leading character using its real width; `start + 1`
        // would land inside a multibyte codepoint.
        let first_len = self
            .chars
            .next()
            .map(|(_, c)| c.len_utf8())
            .unwrap_or(1);
        let end = self.take_while(|c| {
            !c.is_whitespace()
                && c != '@'
                && c != ';'
                && c != '+'
                && c != '-'
                && c != '?'
                && !c.is_ascii_alphanumeric()
        });
        self.push(Tok::Other, start..end.max(start + first_len));
    }

    fn peek_at(&self, byte: usize) -> Option<char> {
        self.text.get(byte..)?.chars().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(bot: &str, text: &str) -> Vec<Tok> {
        lex(bot, text).into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_a_plain_command() {
        assert_eq!(
            kinds("xero-review", "@xero-review ping"),
            vec![Tok::Bot, Tok::Word("ping".into())]
        );
    }

    /// GitHub's autocomplete writes `@name[bot]`, and the suffix is consumed
    /// into the mention token — the old scanner's window heuristic double-fired
    /// on exactly this input.
    #[test]
    fn bot_suffix_is_part_of_the_mention() {
        for text in [
            "@xero-review[bot] ping",
            "@xero-review[BOT] ping",
            "@XERO-REVIEW[Bot] ping",
        ] {
            assert_eq!(
                kinds("xero-review", text),
                vec![Tok::Bot, Tok::Word("ping".into())],
                "input {text:?}"
            );
        }
    }

    #[test]
    fn mention_prefix_is_not_the_bot() {
        assert_eq!(
            kinds("xero-review", "@xero-reviewer ping"),
            vec![Tok::User("xero-reviewer".into()), Tok::Word("ping".into())]
        );
    }

    #[test]
    fn sigils_are_single_tokens() {
        assert_eq!(
            kinds("b", "r? r+ r- ?r"),
            vec![
                Tok::ReviewReq,
                Tok::Approve,
                Tok::Reject,
                Tok::ShortReady
            ]
        );
    }

    #[test]
    fn sigil_glued_to_user() {
        assert_eq!(
            kinds("b", "r?@alice"),
            vec![Tok::ReviewReq, Tok::User("alice".into())]
        );
    }

    /// A hyphenated word starting with `r` is a word, not a Reject.
    #[test]
    fn r_hyphen_word_is_not_reject() {
        assert_eq!(kinds("b", "r-fix"), vec![Tok::Word("r-fix".into())]);
        assert_eq!(kinds("b", "-wip"), vec![Tok::Minus("wip".into())]);
    }

    /// Trailing punctuation stays out of the user token, so the command that
    /// follows it survives. Full-width punctuation matters for a bot whose
    /// replies are in Chinese.
    #[test]
    fn trailing_punctuation_does_not_swallow_the_login() {
        assert_eq!(
            kinds("b", "@alice。"),
            vec![Tok::User("alice".into()), Tok::Other]
        );
        assert_eq!(
            kinds("b", "@alice, @bob"),
            vec![
                Tok::User("alice".into()),
                Tok::Other,
                Tok::User("bob".into())
            ]
        );
    }

    #[test]
    fn semicolon_and_newline_are_separators() {
        assert_eq!(
            kinds("b", "ping; help\nx"),
            vec![
                Tok::Word("ping".into()),
                Tok::Semi,
                Tok::Word("help".into()),
                Tok::Newline,
                Tok::Word("x".into())
            ]
        );
    }

    #[test]
    fn labels_carry_their_sign() {
        assert_eq!(
            kinds("b", "label +bug -wip"),
            vec![
                Tok::Word("label".into()),
                Tok::Plus("bug".into()),
                Tok::Minus("wip".into())
            ]
        );
    }

    /// A login the API would reject still becomes one token, so a diagnostic
    /// can quote it back instead of the command vanishing.
    #[test]
    fn invalid_logins_are_kept_for_diagnostics() {
        // hyphens are in the login charset, so this is a single bad name
        assert_eq!(kinds("b", "@-bad"), vec![Tok::RawUser("-bad".into())]);
        assert_eq!(kinds("b", "@trailing-"), vec![Tok::RawUser("trailing-".into())]);
        assert_eq!(kinds("b", "@"), vec![Tok::RawUser("".into())]);
    }

    #[test]
    fn as_is_a_keyword() {
        assert_eq!(
            kinds("b", "r+ as @alice"),
            vec![Tok::Approve, Tok::As, Tok::User("alice".into())]
        );
    }

    /// Every span must be a valid slice of the input, on char boundaries.
    #[test]
    fn spans_are_valid_char_boundaries() {
        let corpus = [
            "@xero-review \u{212A} x",
            "就绪?r @中文",
            "@xero-review label +中文标签 -wip",
            "🎉 r? @alice 🎉",
            "@xero-review[bot] cc @a, @b; ready",
            "Ω K Å İ ẞ ﬀ",
            "",
        ];
        for text in corpus {
            for t in lex("xero-review", text) {
                assert!(
                    text.is_char_boundary(t.span.start) && text.is_char_boundary(t.span.end),
                    "span {:?} not on char boundaries in {text:?}",
                    t.span
                );
                assert!(t.span.start <= t.span.end, "inverted span in {text:?}");
                assert!(t.span.end <= text.len(), "span past end in {text:?}");
                let _ = &text[t.span.clone()];
            }
        }
    }

    /// The lexer must terminate and not panic on adversarial input.
    #[test]
    fn never_panics_on_nasty_input() {
        let seeds = [
            "@@@@", "????", "++++", "----", ";;;;", "@[bot]", "r?r?r?", "?r?r",
            "@a[bot][bot]", "\u{212A}\u{2126}\u{130}", "中文@中文", "@-", "+", "-",
        ];
        for s in seeds {
            let _ = lex("xero-review", s);
        }
        // fixed-seed mutation over the corpus; deterministic, no dev-dependency
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let alphabet: Vec<char> = "@?+-;rR[bot] \n中🎉\u{212A}aZ0".chars().collect();
        for _ in 0..2000 {
            let len = (next() % 24) as usize;
            let s: String = (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize])
                .collect();
            let _ = lex("xero-review", &s);
        }
    }
}
