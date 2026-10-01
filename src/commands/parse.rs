//! Parse candidates without applying trigger policy, deduplication or conflicts.
//! Ordinary bare words require an entirely valid command block. If that check
//! fails, only explicit mentions and existing symbolic entry points are scanned.

use std::ops::Range;

use super::diag::{Diagnostic, Expected};
use super::lex::{is_valid_label, Tok, Token};
use super::Command;
use crate::config::repository::CommandId;

/// How the command was written, independent of whether policy permits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceForm {
    ExplicitMention,
    BareWord,
    Symbol,
}

/// All offsets are byte ranges into the original, unmodified comment.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCommand {
    pub command: Command,
    pub span: Range<usize>,
    pub source: SourceForm,
    /// The actual bot mention governing this candidate; never crosses a newline.
    pub mention_span: Option<Range<usize>>,
    /// Shared range of the `?r ...` expression that generated these candidates.
    pub compound_span: Option<Range<usize>>,
}

impl ParsedCommand {
    pub fn id(&self) -> CommandId {
        self.command.id()
    }

    pub fn is_explicit(&self) -> bool {
        self.mention_span.is_some()
    }

    pub fn requires_pr(&self) -> bool {
        self.command.requires_pr()
    }
}

fn nullary(word: &str) -> Option<Command> {
    Some(match CommandId::from_name(word)? {
        CommandId::Ping => Command::Ping,
        CommandId::Help => Command::Help,
        CommandId::Review => Command::Review,
        CommandId::Codeql => Command::Codeql,
        CommandId::Ready => Command::Ready,
        CommandId::Author => Command::Author,
        CommandId::Blocked => Command::Blocked,
        CommandId::Claim => Command::Claim,
        CommandId::Unclaim => Command::Unclaim,
        CommandId::Queue => Command::Queue,
        _ => return None,
    })
}

pub use crate::config::repository::WORD_ALIASES as VERBS;

pub struct Parsed {
    pub commands: Vec<ParsedCommand>,
    pub diagnostics: Vec<Diagnostic>,
}

pub(super) fn parse(tokens: &[Token], text: &str, unmasked: bool) -> Parsed {
    if unmasked {
        let mut block = Parser::new(tokens, text, true);
        block.program();
        if block.complete && block.diagnostics.is_empty() {
            return block.finish();
        }
    }
    let mut scan = Parser::new(tokens, text, false);
    scan.program();
    scan.finish()
}

struct Parser<'a> {
    toks: &'a [Token],
    text: &'a str,
    pos: usize,
    strict: bool,
    complete: bool,
    mention: Option<Range<usize>>,
    commands: Vec<ParsedCommand>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Token], text: &'a str, strict: bool) -> Self {
        Self {
            toks,
            text,
            pos: 0,
            strict,
            complete: true,
            mention: None,
            commands: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn finish(self) -> Parsed {
        Parsed {
            commands: self.commands,
            diagnostics: self.diagnostics,
        }
    }

    fn program(&mut self) {
        while self.pos < self.toks.len() {
            let before = self.pos;
            match self.peek() {
                Some(Tok::Bot) => self.mention_command(),
                Some(Tok::ShortReady) => self.short_ready(),
                Some(Tok::ReviewReq) if !self.strict => self.bare_review_request(),
                Some(Tok::Semi) => {
                    if self.strict && self.segment_start() {
                        self.complete = false;
                    }
                    self.pos += 1;
                }
                Some(Tok::Newline) => self.pos += 1,
                Some(Tok::Approve | Tok::ApproveAs | Tok::Reject)
                    if self.strict || self.segment_start() =>
                {
                    let before = self.commands.len();
                    self.verb_tail(false);
                    // Outside a complete block, approval symbols still warrant
                    // syntax diagnostics but cannot produce bare candidates.
                    if !self.strict {
                        self.commands.truncate(before);
                    }
                }
                _ if self.strict => self.verb_tail(false),
                _ => self.pos += 1,
            }
            if self.pos == before {
                self.complete = false;
                self.pos += 1;
            }
        }
    }

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

    fn segment_start(&self) -> bool {
        self.pos == 0 || matches!(self.toks[self.pos - 1].tok, Tok::Newline | Tok::Semi)
    }

    fn addressed_at(&self, mention: usize) -> bool {
        self.toks[..mention]
            .iter()
            .rev()
            .take_while(|t| !matches!(t.tok, Tok::Newline))
            .all(|t| matches!(t.tok, Tok::Punct))
    }

    fn at_boundary(&self) -> bool {
        matches!(
            self.peek(),
            None | Some(Tok::Bot | Tok::Semi | Tok::Newline)
        )
    }

    fn skip_tail(&mut self) {
        while !self.at_boundary() {
            self.pos += 1;
        }
    }

    /// Conversational mention tails may still contain the legacy anywhere
    /// shortcuts. Parameter errors use skip_tail instead, so malformed arguments
    /// cannot manufacture another command.
    fn scan_symbolic_tail(&mut self) {
        while !self.at_boundary() {
            match self.peek() {
                Some(Tok::ShortReady) => self.short_ready(),
                Some(Tok::ReviewReq) => self.bare_review_request(),
                _ => self.pos += 1,
            }
        }
    }

    fn skip_filler(&mut self) -> bool {
        let mut prose = false;
        while let Some(t @ (Tok::Punct | Tok::Prose)) = self.peek() {
            prose |= matches!(t, Tok::Prose);
            self.pos += 1;
        }
        prose
    }

    fn emit(&mut self, command: Command, start: usize, source: SourceForm) {
        let end = self.toks[self.pos.saturating_sub(1)].span.end;
        self.commands.push(ParsedCommand {
            command,
            span: start..end,
            source,
            mention_span: self.mention.clone(),
            compound_span: None,
        });
    }

    fn missing(&mut self, verb: &'static str, expected: Expected) {
        self.complete = false;
        self.diagnostics.push(Diagnostic::MissingArgument {
            verb,
            expected,
            span: self.span(),
        });
    }

    fn extra(&mut self, verb: &'static str) {
        self.complete = false;
        self.diagnostics.push(Diagnostic::ExtraArguments {
            verb,
            span: self.span(),
        });
        self.skip_tail();
    }

    /// Parameters never skip prose/punctuation to find a later login.
    fn user_arg(&mut self, verb: &'static str) -> Option<String> {
        match self.peek().cloned() {
            Some(Tok::User(u)) => {
                self.pos += 1;
                Some(u)
            }
            Some(Tok::RawUser(raw)) => {
                self.complete = false;
                self.diagnostics.push(Diagnostic::InvalidLogin {
                    raw,
                    span: self.span(),
                });
                self.pos += 1;
                None
            }
            _ => {
                self.missing(verb, Expected::User);
                None
            }
        }
    }

    /// Preserve the existing sentence-punctuation suffix, but never discard
    /// words, extra users, markup, or malformed login continuations.
    fn end_arguments(&mut self, verb: &'static str) -> bool {
        while matches!(self.peek(), Some(Tok::Punct))
            && self.text[self.span()]
                .chars()
                .all(|c| ".,!:?。，！：？、".contains(c))
        {
            self.pos += 1;
        }
        if self.at_boundary() {
            true
        } else {
            self.extra(verb);
            false
        }
    }

    /// Exactly one or more logins, optionally separated by commas.
    fn user_list(&mut self) -> Option<Vec<String>> {
        let mut users: Vec<String> = Vec::new();
        loop {
            let Some(user) = self.user_arg("cc") else {
                self.skip_tail();
                return None;
            };
            if !users.iter().any(|u| u.eq_ignore_ascii_case(&user)) {
                users.push(user);
            }
            if self.at_boundary() {
                return Some(users);
            }
            if matches!(self.peek(), Some(Tok::Punct)) && &self.text[self.span()] == "," {
                self.pos += 1;
            } else if !matches!(self.peek(), Some(Tok::User(_) | Tok::RawUser(_)))
                || self.toks[self.pos - 1].span.end == self.span().start
            {
                self.extra("cc");
                return None;
            }
        }
    }

    fn mention_command(&mut self) {
        let addressed = self.addressed_at(self.pos);
        self.mention = Some(self.span());
        self.pos += 1;
        loop {
            self.verb_tail(addressed);
            if matches!(self.peek(), Some(Tok::Semi)) {
                self.pos += 1;
                if self.at_boundary() {
                    break;
                }
            } else {
                break;
            }
        }
        self.mention = None;
    }

    fn verb_tail(&mut self, addressed: bool) {
        let after_prose = if self.strict {
            false
        } else {
            self.skip_filler()
        };
        let start = self.span().start;
        match self.peek().cloned() {
            Some(Tok::Word(word)) => {
                self.pos += 1;
                let source = if self.mention.is_some() {
                    SourceForm::ExplicitMention
                } else {
                    SourceForm::BareWord
                };
                if let Some(cmd) = nullary(&word) {
                    if self.strict
                        && (!self.at_boundary()
                            || (self.mention.is_none() && matches!(self.peek(), Some(Tok::Bot))))
                    {
                        self.complete = false;
                        self.skip_tail();
                        return;
                    }
                    // Explicit nullary commands retain their conversational syntax.
                    // The span covers the command, not the ignored prose following it.
                    self.emit(cmd, start, source);
                    if !self.strict {
                        if self.toks[self.pos..]
                            .iter()
                            .take_while(|t| !matches!(t.tok, Tok::Bot | Tok::Semi | Tok::Newline))
                            .any(|t| matches!(t.tok, Tok::User(_) | Tok::Plus(_) | Tok::Minus(_)))
                        {
                            self.diagnostics.push(Diagnostic::IgnoredArguments {
                                verb: VERBS.iter().copied().find(|v| *v == word).unwrap(),
                                span: self.span(),
                            });
                        }
                        self.scan_symbolic_tail();
                    }
                    return;
                }
                // A word command needs whitespace before its first argument:
                // an email-shaped `cc@alice` is not an instruction.
                if matches!(
                    CommandId::from_name(&word),
                    Some(CommandId::Cc | CommandId::Assign | CommandId::Label)
                ) && !self.at_boundary()
                    && self.toks[self.pos - 1].span.end == self.span().start
                {
                    self.extra(CommandId::from_name(&word).unwrap().name());
                    return;
                }
                match CommandId::from_name(&word) {
                    Some(CommandId::Cc) => {
                        if let Some(users) = self.user_list() {
                            self.emit(Command::Cc { users }, start, source);
                        }
                    }
                    Some(CommandId::Assign) => self.single_user("assign", start, source),
                    Some(CommandId::Label) => self.label_args(start, source),
                    _ => {
                        self.complete = false;
                        if addressed && !after_prose {
                            self.diagnostics.push(Diagnostic::unknown_verb(
                                word,
                                self.toks[self.pos - 1].span.clone(),
                            ));
                        }
                        if self.strict {
                            self.skip_tail();
                        } else {
                            self.scan_symbolic_tail();
                        }
                    }
                }
            }
            Some(Tok::ReviewReq) => {
                self.pos += 1;
                self.single_user("r?", start, SourceForm::Symbol);
            }
            Some(Tok::Approve | Tok::ApproveAs) => self.approve_args(),
            Some(Tok::Reject) => {
                self.pos += 1;
                if self.end_arguments("r-") {
                    self.emit(Command::Reject, start, SourceForm::Symbol);
                }
            }
            Some(Tok::ShortReady) => self.short_ready(),
            _ => {
                self.complete = false;
            }
        }
    }

    fn single_user(&mut self, verb: &'static str, start: usize, source: SourceForm) {
        let user = self.user_arg(verb);
        let valid_end = self.end_arguments(verb);
        if let (Some(user), true) = (user, valid_end) {
            let cmd = if verb == "assign" {
                Command::Assign { user }
            } else {
                Command::RequestReview { user }
            };
            self.emit(cmd, start, source);
        }
    }

    fn label_args(&mut self, start: usize, source: SourceForm) {
        let mut add = Vec::new();
        let mut remove = Vec::new();
        let mut valid = true;
        while !self.at_boundary() {
            match self.peek().cloned() {
                Some(Tok::Plus(name) | Tok::Minus(name)) => {
                    if !is_valid_label(&name) {
                        valid = false;
                        self.diagnostics.push(Diagnostic::InvalidLabel {
                            raw: name,
                            span: self.span(),
                        });
                    } else if matches!(self.peek(), Some(Tok::Plus(_))) {
                        add.push(name);
                    } else {
                        remove.push(name);
                    }
                    self.pos += 1;
                }
                _ => {
                    self.extra("label");
                    valid = false;
                }
            }
        }
        if add.is_empty() && remove.is_empty() {
            self.missing("label", Expected::Labels);
        } else if valid {
            self.emit(Command::Label { add, remove }, start, source);
        }
        self.complete &= valid;
    }

    /// All three spellings share exactly the same target/tail validation.
    fn approve_args(&mut self) {
        let start = self.span().start;
        let required = matches!(self.peek(), Some(Tok::ApproveAs));
        let verb = if required { "r=" } else { "r+" };
        self.pos += 1;
        let has_as = matches!(self.peek(), Some(Tok::As));
        if has_as && !required {
            self.pos += 1;
        }
        let target = if required || has_as || !self.at_boundary() {
            // Plain r+ is valid only when it really has no target arguments.
            let user = self.user_arg(verb);
            let valid_end = self.end_arguments(verb);
            match (user, valid_end) {
                (Some(user), true) => Some(user),
                _ => return,
            }
        } else {
            None
        };
        self.emit(
            Command::Approve {
                on_behalf_of: target,
            },
            start,
            SourceForm::Symbol,
        );
    }

    fn short_ready(&mut self) {
        let start = self.span().start;
        let first = self.commands.len();
        self.pos += 1;
        self.emit(Command::Ready, start, SourceForm::Symbol);
        if !self.strict {
            self.skip_filler();
        }
        if let Some(Tok::User(user)) = self.peek().cloned() {
            let at = self.span().start;
            self.pos += 1;
            self.emit(Command::RequestReview { user }, at, SourceForm::Symbol);
        }
        if !self.strict {
            self.skip_filler();
        }
        if matches!(self.peek(), Some(Tok::Word(w)) if w == "cc") {
            let at = self.span().start;
            self.pos += 1;
            if let Some(users) = self.user_list() {
                self.emit(Command::Cc { users }, at, SourceForm::Symbol);
            }
        }
        let end = self.toks[self.pos - 1].span.end;
        for c in &mut self.commands[first..] {
            c.compound_span = Some(start..end);
        }
        if self.strict && !self.at_boundary() {
            self.complete = false;
        }
    }

    /// Preserve r?'s existing anywhere-in-prose position rule.
    fn bare_review_request(&mut self) {
        let start = self.span().start;
        self.pos += 1;
        self.skip_filler();
        if let Some(Tok::User(user)) = self.peek().cloned() {
            self.pos += 1;
            self.emit(Command::RequestReview { user }, start, SourceForm::Symbol);
        }
    }
}
