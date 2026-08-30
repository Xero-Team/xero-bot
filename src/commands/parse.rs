//! Stage 3: recursive descent over the token stream.
//!
//! Grammar:
//!
//! ```text
//! program   := item*
//! item      := command | <advance one token>
//! command   := Bot verb_tail (Semi verb_tail)*
//!            | ShortReady short_args
//!            | ReviewReq user_arg               -- bare `r? @user`
//! verb_tail := Word(v) args_for(v)
//!            | ReviewReq user_arg
//!            | Approve approve_args
//!            | Reject
//! ```
//!
//! Two rules do most of the work:
//!
//! * Every argument loop stops at `Bot`, `Semi` or `Newline`. That one rule is
//!   what bounds a command's arguments — the old parser let them run to the end
//!   of the comment, so `@bot cc @a` collected every later `@mention` and
//!   `@bot label +x` collected every later `+tok`.
//! * `?r`, `r?`, `r+` and `r-` arrive as distinct token kinds, so no two
//!   readings can claim the same text. The old scanner ran three regex passes
//!   that avoided each other with a four-character window, which double-fired
//!   on `@bot[bot] r? @alice` and silently dropped `@bot, r? @alice`.

use std::ops::Range;

use super::diag::{Diagnostic, Expected};
use super::lex::{is_valid_label, Tok, Token};
use super::Command;

/// A command plus where it appeared, so execution order follows the text.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommand {
    pub command: Command,
    pub start: usize,
}

/// Verbs taking no arguments, and the command each produces.
fn nullary(word: &str) -> Option<Command> {
    Some(match word {
        "ping" => Command::Ping,
        "help" | "commands" => Command::Help,
        "review" => Command::Review,
        "codeql" => Command::Codeql,
        "ready" | "reviewer" => Command::Ready,
        "author" => Command::Author,
        "blocked" => Command::Blocked,
        "claim" => Command::Claim,
        "unclaim" | "release-assignment" | "release" => Command::Unclaim,
        _ => return None,
    })
}

/// Every verb the parser knows, for "did you mean" suggestions.
pub const VERBS: &[&str] = &[
    "ping",
    "help",
    "commands",
    "review",
    "codeql",
    "ready",
    "reviewer",
    "author",
    "blocked",
    "claim",
    "unclaim",
    "release",
    "release-assignment",
    "cc",
    "label",
    "relabel",
    "assign",
];

pub struct Parsed {
    pub commands: Vec<ParsedCommand>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn parse(tokens: &[Token]) -> Parsed {
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        commands: Vec::new(),
        diagnostics: Vec::new(),
    };
    p.program();
    Parsed {
        commands: p.commands,
        diagnostics: p.diagnostics,
    }
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    commands: Vec<ParsedCommand>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn program(&mut self) {
        while self.pos < self.toks.len() {
            let before = self.pos;
            match &self.toks[self.pos].tok {
                Tok::Bot => self.mention_command(),
                Tok::ShortReady => self.short_ready(),
                Tok::ReviewReq => self.bare_review_request(),
                _ => self.pos += 1,
            }
            // Guarantee forward progress even if a branch declines to consume.
            if self.pos == before {
                self.pos += 1;
            }
        }
    }

    // --- helpers ---------------------------------------------------------

    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn span(&self) -> Range<usize> {
        self.toks
            .get(self.pos)
            .or_else(|| self.toks.last())
            .map(|t| t.span.clone())
            .unwrap_or(0..0)
    }

    /// True when the mention at `mention` opens its line, ignoring punctuation
    /// such as a list bullet.
    ///
    /// This is what separates an address to the bot from a reference to it. Both
    /// execute verbs — `@bot cc @a @bot assign @b` has always worked — but only
    /// an address may be told off for an unrecognised word. Otherwise
    /// `cc @bot about this` answers "`about` 不是命令".
    fn addressed_at(&self, mention: usize) -> bool {
        self.toks[..mention]
            .iter()
            .rev()
            .take_while(|t| !matches!(t.tok, Tok::Newline))
            .all(|t| matches!(t.tok, Tok::Punct))
    }

    /// True at a hard command boundary: a new mention, a `;`, or end of line.
    fn at_boundary(&self) -> bool {
        matches!(
            self.peek(),
            None | Some(Tok::Bot) | Some(Tok::Semi) | Some(Tok::Newline)
        )
    }

    /// Skip filler inside a command's arguments. Punctuation between arguments
    /// is not meaningful, so `cc @a, @b` works.
    ///
    /// Returns whether any of it was prose. Callers that report a mistake use
    /// that to stay quiet: an unknown word is a typo when it sits right after
    /// the mention and just a word when a sentence got there first.
    fn skip_filler(&mut self) -> bool {
        let mut prose = false;
        while let Some(t @ (Tok::Punct | Tok::Prose)) = self.peek() {
            prose |= matches!(t, Tok::Prose);
            self.pos += 1;
        }
        prose
    }

    fn emit(&mut self, command: Command, start: usize) {
        self.commands.push(ParsedCommand { command, start });
    }

    /// Consume the next argument as a login, reporting why if it isn't one.
    fn user_arg(&mut self, verb: &'static str) -> Option<String> {
        self.skip_filler();
        match self.peek() {
            Some(Tok::User(u)) => {
                let u = u.clone();
                self.pos += 1;
                Some(u)
            }
            Some(Tok::RawUser(raw)) => {
                let raw = raw.clone();
                let span = self.span();
                self.pos += 1;
                self.diagnostics.push(Diagnostic::InvalidLogin { raw, span });
                None
            }
            _ => {
                self.diagnostics.push(Diagnostic::MissingArgument {
                    verb,
                    expected: Expected::User,
                    span: self.span(),
                });
                None
            }
        }
    }

    /// Collect logins up to the next boundary.
    fn user_list(&mut self) -> Vec<String> {
        let mut users = Vec::new();
        while !self.at_boundary() {
            match self.peek() {
                Some(Tok::User(u)) => {
                    let u = u.clone();
                    if !users.contains(&u) {
                        users.push(u);
                    }
                    self.pos += 1;
                }
                Some(Tok::RawUser(raw)) => {
                    let raw = raw.clone();
                    let span = self.span();
                    self.pos += 1;
                    self.diagnostics.push(Diagnostic::InvalidLogin { raw, span });
                }
                _ => self.pos += 1,
            }
        }
        users
    }

    // --- productions -----------------------------------------------------

    /// `Bot verb_tail (Semi verb_tail)*`
    ///
    /// A mention's scope covers the rest of its line; `;` starts another
    /// command inside it, so one mention can carry several.
    fn mention_command(&mut self) {
        let addressed = self.addressed_at(self.pos);
        self.pos += 1; // Bot
        loop {
            self.verb_tail(addressed);
            if matches!(self.peek(), Some(Tok::Semi)) {
                self.pos += 1;
                // A trailing `;` with nothing after it is harmless.
                if self.at_boundary() {
                    break;
                }
                continue;
            }
            break;
        }
    }

    fn verb_tail(&mut self, addressed: bool) {
        // Leading punctuation after the mention is not an error: `@bot, ping`
        // and `@bot: ping` are what people actually type. Prose is skipped too —
        // `@bot 请 review 一下` should still run — but it's remembered, because
        // it decides whether an unrecognised word counts as a typo.
        let after_prose = self.skip_filler();
        let start = self.span().start;

        match self.peek().cloned() {
            Some(Tok::Word(word)) => {
                self.pos += 1;
                self.verb_with_args(&word, start, addressed && !after_prose);
            }
            Some(Tok::ReviewReq) => {
                self.pos += 1;
                if let Some(user) = self.user_arg("r?") {
                    self.emit(Command::RequestReview { user }, start);
                }
            }
            Some(Tok::Approve) => {
                self.pos += 1;
                self.approve_args(start);
            }
            Some(Tok::Reject) => {
                self.pos += 1;
                self.emit(Command::Reject, start);
            }
            // A mention with nothing command-like after it. Staying silent here
            // is deliberate: `@bot 谢谢!` must not draw a reply. Only a word
            // that looks like a verb attempt earns a diagnostic, which is
            // decided in `verb_with_args`.
            _ => {}
        }
    }

    /// `may_complain` gates only the unknown-word diagnostic. A verb the parser
    /// does recognise is a clear enough signal on its own, so its own problems
    /// (a missing argument, a bad login) are always reported.
    fn verb_with_args(&mut self, word: &str, start: usize, may_complain: bool) {
        if let Some(cmd) = nullary(word) {
            self.reject_extra_args(word, start);
            self.emit(cmd, start);
            return;
        }
        match word {
            "cc" => {
                let users = self.user_list();
                if users.is_empty() {
                    self.diagnostics.push(Diagnostic::MissingArgument {
                        verb: "cc",
                        expected: Expected::Users,
                        span: self.span(),
                    });
                } else {
                    self.emit(Command::Cc { users }, start);
                }
            }
            "assign" => {
                if let Some(user) = self.user_arg("assign") {
                    self.emit(Command::Assign { user }, start);
                }
            }
            "label" | "relabel" => self.label_args(start),
            other => {
                // An unknown word directly after a mention that opens the line
                // is a typo worth naming. Anything looser is noise:
                // `@bot 这个 PR 很好` must not be answered with
                // "`pr` 不是命令,是否想用 `cc`?".
                if may_complain {
                    self.diagnostics.push(Diagnostic::unknown_verb(
                        other.to_string(),
                        self.toks[self.pos.saturating_sub(1)].span.clone(),
                    ));
                }
            }
        }
    }

    fn label_args(&mut self, start: usize) {
        let mut add = Vec::new();
        let mut remove = Vec::new();
        while !self.at_boundary() {
            match self.peek().cloned() {
                Some(Tok::Plus(name)) => {
                    let span = self.span();
                    self.pos += 1;
                    if is_valid_label(&name) {
                        add.push(name);
                    } else {
                        self.diagnostics
                            .push(Diagnostic::InvalidLabel { raw: name, span });
                    }
                }
                Some(Tok::Minus(name)) => {
                    let span = self.span();
                    self.pos += 1;
                    if is_valid_label(&name) {
                        remove.push(name);
                    } else {
                        self.diagnostics
                            .push(Diagnostic::InvalidLabel { raw: name, span });
                    }
                }
                _ => self.pos += 1,
            }
        }
        if add.is_empty() && remove.is_empty() {
            self.diagnostics.push(Diagnostic::MissingArgument {
                verb: "label",
                expected: Expected::Labels,
                span: self.span(),
            });
        } else {
            self.emit(Command::Label { add, remove }, start);
        }
    }

    /// `r+`, `r+ @user`, `r+ as @user`
    fn approve_args(&mut self, start: usize) {
        self.skip_filler();
        if matches!(self.peek(), Some(Tok::As)) {
            self.pos += 1;
        }
        self.skip_filler();
        let on_behalf_of = match self.peek() {
            Some(Tok::User(u)) => {
                let u = u.clone();
                self.pos += 1;
                Some(u)
            }
            Some(Tok::RawUser(raw)) => {
                let raw = raw.clone();
                let span = self.span();
                self.pos += 1;
                self.diagnostics.push(Diagnostic::InvalidLogin { raw, span });
                None
            }
            _ => None,
        };
        self.emit(Command::Approve { on_behalf_of }, start);
    }

    /// `?r`, `?r @user`, `?r cc @a @b`, `?r @user cc @b`
    ///
    /// `@user` requests review from that user — it used to be dropped in
    /// silence, so `?r @alice` only set the label and looked broken.
    fn short_ready(&mut self) {
        let start = self.span().start;
        self.pos += 1; // ?r
        self.emit(Command::Ready, start);

        self.skip_filler();
        // A reviewer, if one was named. `?r` requires the `@` sigil, unlike the
        // bare `r?` form, so a following bare word stays available for verbs.
        if let Some(Tok::User(u)) = self.peek().cloned() {
            let at = self.span().start;
            self.pos += 1;
            self.emit(Command::RequestReview { user: u }, at);
        }

        self.skip_filler();
        // `cc` must be the whole token; `starts_with("cc")` used to make
        // `?r ccache @alice` parse as a cc.
        if matches!(self.peek(), Some(Tok::Word(w)) if w == "cc") {
            let at = self.span().start;
            self.pos += 1;
            let users = self.user_list();
            if users.is_empty() {
                self.diagnostics.push(Diagnostic::MissingArgument {
                    verb: "cc",
                    expected: Expected::Users,
                    span: self.span(),
                });
            } else {
                self.emit(Command::Cc { users }, at);
            }
        }
    }

    /// Bare `r? @user` anywhere in a comment, no mention needed.
    fn bare_review_request(&mut self) {
        let start = self.span().start;
        self.pos += 1; // r?
        // Deliberately no diagnostic when a user doesn't follow: a rhetorical
        // "who should review this r?" is prose, not a failed command.
        self.skip_filler();
        if let Some(Tok::User(u)) = self.peek().cloned() {
            self.pos += 1;
            self.emit(Command::RequestReview { user: u }, start);
        }
    }

    /// Nullary verbs ignore trailing text, but flag it when it looks like an
    /// argument the user expected to matter.
    fn reject_extra_args(&mut self, verb: &str, _start: usize) {
        let mut sig = None;
        let mut scan = self.pos;
        while let Some(t) = self.toks.get(scan) {
            if matches!(t.tok, Tok::Bot | Tok::Semi | Tok::Newline) {
                break;
            }
            if matches!(t.tok, Tok::User(_) | Tok::Plus(_) | Tok::Minus(_)) {
                sig = Some(t.span.clone());
                break;
            }
            scan += 1;
        }
        if let Some(span) = sig {
            if let Some(v) = VERBS.iter().find(|v| **v == verb) {
                self.diagnostics
                    .push(Diagnostic::ExtraArguments { verb: v, span });
            }
        }
    }
}
