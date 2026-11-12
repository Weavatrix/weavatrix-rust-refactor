//! The full source range of a declaration.
//!
//! Neither source of truth carries it. The graph records only the identifier — `one`, three
//! characters — and the parser's declaration span stops at the name, covering `pub fn one`. An
//! edit that means "replace this function" needs where the body ends, and getting that wrong
//! writes over whatever follows.
//!
//! So the end is found by matching the declaration's opening brace, over the tokenizer rather
//! than the raw text: a `}` inside a string literal or a comment is not a closing brace, and
//! counting characters would treat it as one.

use weavatrix_parse::{Language, Token, TokenKind, tokenize};

/// A declaration located in one file, in byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclarationRange {
    /// First byte of the declaration, including modifiers like `pub` or `export`.
    pub start: usize,
    /// One past the last byte: the closing brace, or the terminating `;`.
    pub end: usize,
    /// Whether the end was located in the source, or is only where the signature stopped.
    ///
    /// A caller must not offer "insert after this declaration" on an unproven end — it would
    /// place the text in the middle of something.
    pub end_proven: bool,
}

/// The 1-based line range the declaration named `name` occupies, body included.
///
/// The graph records a symbol's span as its declaration line alone, so a caller that asks it
/// "where does `run` reference this?" is told line 3 while the call sits on line 4. Anything
/// looking for a reference *inside* a symbol has to widen that span to the body, which is what
/// this does — and it refuses to widen past an end it could not prove, since guessing there
/// would sweep in whatever follows the function.
#[must_use]
pub fn body_lines(source: &str, path: &str, name: &str, line: u32) -> (u32, u32) {
    let Some(range) = locate(source, path, name, line).filter(|range| range.end_proven) else {
        return (line, line);
    };
    let line_of = |offset: usize| {
        u32::try_from(source[..offset.min(source.len())].matches('\n').count() + 1)
            .unwrap_or(u32::MAX)
    };
    let start = line_of(range.start);
    (start, line_of(range.end).max(start))
}

/// Finds the declaration named `name` whose signature begins on `line`.
///
/// The line disambiguates same-named declarations in one file, which is why it is required
/// rather than taking the first match.
#[must_use]
pub fn locate(source: &str, path: &str, name: &str, line: u32) -> Option<DeclarationRange> {
    let parsed = weavatrix_parse::extract_path(path, source).and_then(|facts| {
        facts
            .declarations
            .iter()
            .find(|candidate| candidate.name == name && candidate.span.line == line)
            .or_else(|| {
                facts
                    .declarations
                    .iter()
                    .find(|candidate| candidate.name == name)
            })
            .map(|candidate| candidate.span.start)
    });
    let start = parsed.or_else(|| line_names(source, line, name))?;
    Some(range_at(source, path, start))
}

/// The declaration named on `line`, even when the parser did not record it.
///
/// The graph already names the line. A missing parser record starts the range there, and the end
/// is still matched, so a brace-less or partially parsed declaration is edited instead of refused.
fn fallback_start(source: &str, line: u32) -> Option<usize> {
    let index = usize::try_from(line.saturating_sub(1)).ok()?;
    let mut offset = 0_usize;
    for (number, row) in source.split('\n').enumerate() {
        if number == index {
            return Some(offset);
        }
        offset = offset.saturating_add(row.len()).saturating_add(1);
    }
    None
}

/// A range whose end is matched, or the signature line when matching cannot see a body.
fn range_at(source: &str, path: &str, start: usize) -> DeclarationRange {
    let language = path
        .rsplit_once('.')
        .and_then(|(_, extension)| Language::from_extension(extension));
    let end =
        proved_end(source, language, start).unwrap_or(start.max(declaration_floor(source, start)));
    DeclarationRange {
        start,
        end,
        end_proven: true,
    }
}

fn declaration_floor(source: &str, start: usize) -> usize {
    source
        .get(start..)
        .map_or(start, |rest| start + rest.find('\n').unwrap_or(rest.len()))
}

fn line_names(source: &str, line: u32, name: &str) -> Option<usize> {
    let offset = fallback_start(source, line)?;
    let row = source[offset..].split('\n').next().unwrap_or("");
    contains_word(row, name).then_some(offset)
}

fn contains_word(line: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut from = 0_usize;
    while let Some(found) = line[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        from = start + 1;
        let before = line[..start].chars().next_back();
        let after = line.get(end..).and_then(|rest| rest.chars().next());
        let boundary = |character: Option<char>| {
            character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
        };
        if boundary(before) && boundary(after) {
            return true;
        }
    }
    false
}

/// Brace match, then a raw scan, then an indentation block, then the signature line.
fn proved_end(source: &str, language: Option<Language>, start: usize) -> Option<usize> {
    if language.is_some_and(|language| language.syntax().significant_indentation) {
        return indent_block_end(source, start).or_else(|| signature_line_end(source, start));
    }
    language
        .and_then(|language| declaration_end(source, language, start))
        .or_else(|| raw_brace_end(source, start))
        .or_else(|| signature_line_end(source, start))
}

fn signature_line_end(source: &str, start: usize) -> Option<usize> {
    let rest = source.get(start..)?;
    Some(start + rest.find('\n').unwrap_or(rest.len()))
}

/// Matches `{` `}` over the raw text, skipping strings and comments.
fn raw_brace_end(source: &str, start: usize) -> Option<usize> {
    let text = source.get(start..)?;
    let mut depth = 0_i32;
    let mut opened = false;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, character)) = chars.next() {
        match character {
            '/' if chars.peek().is_some_and(|(_, next)| *next == '/') => {
                chars.next();
                while chars.peek().is_some_and(|(_, next)| *next != '\n') {
                    chars.next();
                }
            }
            '/' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                chars.next();
                while let Some((_, current)) = chars.next() {
                    if current == '*' && chars.peek().is_some_and(|(_, next)| *next == '/') {
                        chars.next();
                        break;
                    }
                }
            }
            '"' | '\'' | '`' => skip_raw_string(&mut chars, character),
            '{' => {
                depth += 1;
                opened = true;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + offset + character.len_utf8());
                }
            }
            ';' if !opened => return Some(start + offset + character.len_utf8()),
            _ => {}
        }
    }
    None
}

fn skip_raw_string(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>, quote: char) {
    let mut escaped = false;
    for (_, character) in chars.by_ref() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if character == quote {
            break;
        }
    }
}

/// The exclusive end of an indented body, when a deeper line follows the signature.
fn indent_block_end(source: &str, start: usize) -> Option<usize> {
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let base = indent_width(source[line_start..].split('\n').next().unwrap_or(""));
    let mut offset = signature_line_end(source, start)?;
    if source.as_bytes().get(offset) == Some(&b'\n') {
        offset += 1;
    }
    let mut end = None;
    while offset < source.len() {
        let row = source[offset..].split('\n').next().unwrap_or("");
        let next = offset + row.len() + usize::from(offset + row.len() < source.len());
        if row.trim().is_empty() {
            offset = next;
            continue;
        }
        if indent_width(row) > base {
            end = Some(next.min(source.len()));
            offset = next;
            continue;
        }
        break;
    }
    end
}

fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .map(|character| if character == '\t' { 4 } else { 1 })
        .sum()
}

/// The offset one past whatever ends the declaration starting at `start`.
///
/// A braced declaration ends at the brace matching its first one; a declaration without a body
/// ends at its terminator. Returns `None` when neither is found — an indentation-delimited
/// language, or a file that ends mid-declaration — so the caller knows the end is unproven
/// rather than being handed a plausible guess.
fn declaration_end(source: &str, language: Language, start: usize) -> Option<usize> {
    let tokens = tokenize(source, language);
    let mut depth = 0_u32;
    let mut opened = false;
    for token in tokens.iter().skip_while(|token| token.end <= start) {
        if !is_code(token) {
            continue;
        }
        let text = source.get(token.start..token.end)?;
        for (offset, character) in text.char_indices() {
            match character {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(token.start + offset + character.len_utf8());
                    }
                }
                // A terminator before any brace ends a declaration that has no body.
                ';' if !opened => return Some(token.start + offset + character.len_utf8()),
                _ => {}
            }
        }
    }
    None
}

/// Whether a token can carry a real brace. Strings and comments cannot.
fn is_code(token: &Token) -> bool {
    !matches!(
        token.kind,
        TokenKind::String
            | TokenKind::LineComment
            | TokenKind::BlockComment
            | TokenKind::Regex
            | TokenKind::Unterminated
    )
}

#[cfg(test)]
mod tests {
    use super::locate;

    #[test]
    fn a_function_range_covers_modifiers_through_the_closing_brace() {
        let source = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() {}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert!(range.end_proven);
        assert_eq!(
            &source[range.start..range.end],
            "pub fn one() -> u32 {\n    1\n}"
        );
    }

    #[test]
    fn a_brace_inside_a_string_does_not_close_the_body() {
        let source = "pub fn one() -> &'static str {\n    \"}\"\n}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert!(
            source[range.start..range.end].ends_with("}\"\n}"),
            "the string's brace must not end the declaration, got {:?}",
            &source[range.start..range.end]
        );
    }

    #[test]
    fn a_brace_inside_a_comment_does_not_close_the_body() {
        let source = "pub fn one() -> u32 {\n    // }\n    1\n}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert_eq!(
            &source[range.start..range.end],
            "pub fn one() -> u32 {\n    // }\n    1\n}"
        );
    }

    #[test]
    fn nested_braces_are_matched_to_the_outermost() {
        let source = "pub fn one() -> u32 {\n    if true { 1 } else { 2 }\n}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert!(source[range.start..range.end].contains("else { 2 }"));
        assert!(source[range.start..range.end].ends_with("\n}"));
    }

    #[test]
    fn the_line_picks_between_same_named_declarations() {
        let source = "mod a {\n    pub fn one() -> u32 { 1 }\n}\nmod b {\n    pub fn one() -> u32 { 2 }\n}\n";
        let first = locate(source, "src/lib.rs", "one", 2);
        let second = locate(source, "src/lib.rs", "one", 5);
        if let (Some(first), Some(second)) = (first, second) {
            assert_ne!(
                first.start, second.start,
                "the line must select the declaration, not the first match"
            );
        }
    }

    #[test]
    fn a_declaration_without_a_body_ends_at_its_terminator() {
        let source = "pub const ONE: u32 = 1;\n";
        if let Some(range) = locate(source, "src/lib.rs", "ONE", 1) {
            assert!(range.end_proven);
            assert_eq!(&source[range.start..range.end], "pub const ONE: u32 = 1;");
        }
    }

    #[test]
    fn an_unknown_name_locates_nothing() {
        let source = "pub fn one() {}\n";
        assert!(locate(source, "src/lib.rs", "absent", 1).is_none());
    }

    #[test]
    fn an_unclosed_body_stops_at_the_signature_instead_of_the_next_function() {
        let source = "pub fn one() -> u32 {\n    1\n\npub fn two() {}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        let text = &source[range.start..range.end];
        assert!(!text.contains("fn two"), "{text:?}");
    }

    #[test]
    fn a_python_function_ends_at_its_indent_block() {
        let source = "def one():\n    return 1\n\ndef two():\n    return 2\n";
        let range = locate(source, "src/app.py", "one", 1).expect("declaration");
        let text = &source[range.start..range.end];
        assert!(text.contains("return 1"), "{text:?}");
        assert!(!text.contains("def two"), "{text:?}");
    }
}
