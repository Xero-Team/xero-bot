//! Stage 1: blank out regions of a comment that must not be read as commands.
//!
//! Markdown that *renders* as code or as a quotation isn't an instruction. The
//! bot's own help text puts every command in backticks, so failing to mask
//! inline code is what let a single `help` execute six commands.
//!
//! The output is the same byte length as the input — masked bytes become
//! spaces. Every later stage reports spans, and those spans index the original
//! text, so this invariant is what keeps them valid.

/// Replace non-command regions with spaces, preserving byte offsets.
pub fn mask_noncommand_regions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    // Open fence, as (delimiter char, run length). A fence closes only on the
    // same character with a run at least as long, so ``` and ~~~ can't close
    // each other.
    let mut fence: Option<(char, usize)> = None;
    let mut prev_line_blank = true;

    for line in text.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();

        // Newlines always survive masking: the lexer treats one as a hard
        // command terminator, so turning it into a space would let a command's
        // arguments run past the end of its line.
        let mut mask_whole_line = false;

        if let Some((fence_char, fence_len)) = fence {
            mask_whole_line = true;
            if let Some((c, n)) = fence_delimiter(trimmed) {
                if c == fence_char && n >= fence_len {
                    fence = None;
                }
            }
        } else if let Some((c, n)) = fence_delimiter(trimmed) {
            // An unterminated fence masks the rest of the comment, which is
            // correct: CommonMark runs an unclosed fenced block to the end of
            // the document and GitHub renders it that way, so agreeing with the
            // renderer is the conservative choice.
            fence = Some((c, n));
            mask_whole_line = true;
        } else if trimmed.starts_with('>') {
            // GitHub's "Quote reply" prefixes quoted text with `> `, so without
            // this the bot re-executes commands quoted back at it — including
            // its own help table.
            mask_whole_line = true;
        } else if prev_line_blank && is_indented_code(body) {
            // Indented code only starts after a blank line. The trade-off is
            // that a 4-space-indented list continuation mentioning a command
            // becomes a false negative; that beats today's false positive,
            // where a pasted indented example actually runs.
            mask_whole_line = true;
        }

        if mask_whole_line {
            blank_into(&mut out, body);
            prev_line_blank = false;
        } else {
            mask_inline_code(&mut out, body);
            prev_line_blank = trimmed.is_empty();
        }
        if has_newline {
            out.push('\n');
        }
    }

    debug_assert_eq!(
        out.len(),
        text.len(),
        "masking must preserve byte length or every downstream span breaks"
    );
    out
}

/// A fence opener/closer: three or more backticks or tildes.
fn fence_delimiter(trimmed: &str) -> Option<(char, usize)> {
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let n = trimmed.chars().take_while(|c| *c == first).count();
    (n >= 3).then_some((first, n))
}

fn is_indented_code(body: &str) -> bool {
    if body.trim().is_empty() {
        return false;
    }
    body.starts_with('\t') || body.starts_with("    ")
}

/// Mask inline code spans within one line, writing the result to `out`.
///
/// A run of N backticks opens a span that only a run of exactly N closes, per
/// CommonMark. An unmatched backtick is literal text, so it stays.
fn mask_inline_code(out: &mut String, body: &str) {
    let bytes_before = out.len();
    let mut rest = body;

    while let Some(open_at) = rest.find('`') {
        out.push_str(&rest[..open_at]);
        let after_open = &rest[open_at..];
        let ticks = after_open.chars().take_while(|c| *c == '`').count();
        let delim = &after_open[..ticks];
        let content = &after_open[ticks..];

        match find_closing_run(content, ticks) {
            Some(close_at) => {
                // blank the delimiters and the content between them
                let span_len = ticks + close_at + ticks;
                blank_into(out, &after_open[..span_len]);
                rest = &after_open[span_len..];
            }
            None => {
                // no closer on this line: the backticks are literal
                out.push_str(delim);
                rest = content;
            }
        }
    }
    out.push_str(rest);

    debug_assert_eq!(out.len() - bytes_before, body.len());
}

/// Byte offset in `s` of a backtick run of exactly `n`, or None.
fn find_closing_run(s: &str, n: usize) -> Option<usize> {
    let mut idx = 0usize;
    while let Some(hit) = s[idx..].find('`') {
        let at = idx + hit;
        let run = s[at..].chars().take_while(|c| *c == '`').count();
        if run == n {
            return Some(at);
        }
        idx = at + run;
    }
    None
}

/// Push as many spaces as `s` has bytes. Spaces are one byte each, so this is
/// what keeps the output length equal to the input length.
fn blank_into(out: &mut String, s: &str) {
    for _ in 0..s.len() {
        out.push(' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn masked(s: &str) -> String {
        mask_noncommand_regions(s)
    }

    /// The invariant every span in the later stages depends on.
    #[test]
    fn masking_preserves_byte_length() {
        for s in [
            "plain text",
            "`code`",
            "``a`b``",
            "unmatched ` tick",
            "> quoted @bot review",
            "```\nfenced\n```",
            "~~~\nfenced\n~~~",
            "```\nunterminated",
            "    indented code",
            "中文 `代码` 混排 🎉",
            "| `@bot review` | 说明 |",
            "",
            "\n\n\n",
            "a\r\nb\r\n",
        ] {
            assert_eq!(masked(s).len(), s.len(), "input {s:?}");
        }
    }

    #[test]
    fn inline_code_is_masked() {
        assert_eq!(masked("a `b` c"), "a     c");
        // the help table's shape
        let row = "| `@bot review` | AI 审查 |";
        let m = masked(row);
        assert!(!m.contains("@bot"), "mention must be masked: {m:?}");
        assert!(m.contains("AI 审查"), "prose must survive: {m:?}");
    }

    #[test]
    fn unmatched_backtick_is_literal() {
        assert_eq!(masked("a ` b"), "a ` b");
    }

    #[test]
    fn multi_backtick_spans_need_equal_runs() {
        // ``a`b`` is one span containing a single backtick
        let m = masked("x ``a`b`` y");
        assert_eq!(m, "x         y");
    }

    #[test]
    fn blockquote_is_masked() {
        let m = masked("> @bot review\nreal text");
        assert!(!m.contains("@bot"));
        assert!(m.contains("real text"));
    }

    #[test]
    fn tilde_fence_does_not_close_backtick_fence() {
        let m = masked("```\n~~~\n@bot review\n```\nafter");
        assert!(
            !m.contains("@bot"),
            "still inside the backtick fence: {m:?}"
        );
        assert!(m.contains("after"));
    }

    #[test]
    fn fence_needs_matching_length_to_close() {
        let m = masked("````\n```\n@bot review\n````\nafter");
        assert!(!m.contains("@bot"), "3 ticks can't close 4: {m:?}");
        assert!(m.contains("after"));
    }

    /// Documents intent rather than asserting a fix: CommonMark says an
    /// unclosed fence runs to the end of the document.
    #[test]
    fn unterminated_fence_masks_rest() {
        let m = masked("intro\n```\n@bot review\nmore text");
        assert!(m.contains("intro"));
        assert!(!m.contains("@bot"));
        assert!(!m.contains("more text"));
    }

    #[test]
    fn indented_code_is_masked_only_after_blank_line() {
        let m = masked("intro\n\n    @bot review\n");
        assert!(!m.contains("@bot"), "indented block: {m:?}");

        // A wrapped list continuation is not an indented code block here,
        // because the previous line isn't blank.
        let m = masked("- item\n    @bot review\n");
        assert!(m.contains("@bot"), "continuation line: {m:?}");
    }

    #[test]
    fn fence_inside_blockquote_does_not_leak() {
        let m = masked("> ```\n@bot review");
        assert!(m.contains("@bot"), "quote must not open a fence: {m:?}");
    }
}
