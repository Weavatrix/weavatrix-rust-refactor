//! Whether source still parses after an edit.
//!
//! Rust files go through `syn`. Every other language this crate can tokenize is checked for an
//! unterminated literal or comment and for an unmatched `()`, `[]`, or `{}`. A language with no
//! tokenizer is reported as unchecked; a plan must not claim that check ran.

use blazingly_json::{Value, json};
use weavatrix_parse::{Language, TokenKind, tokenize};

pub(super) enum Syntax {
    Proven,
    Unchecked,
    Refused(Value),
}

/// The file with `source[start..end]` replaced by `content`.
pub(super) fn rewrite(source: &str, start: usize, end: usize, content: &str) -> Option<String> {
    if start > end
        || end > source.len()
        || !source.is_char_boundary(start)
        || !source.is_char_boundary(end)
    {
        return None;
    }
    let mut rewritten = String::with_capacity(source.len() - (end - start) + content.len());
    rewritten.push_str(&source[..start]);
    rewritten.push_str(content);
    rewritten.push_str(&source[end..]);
    Some(rewritten)
}

/// Parses `source` as the language of `path`.
pub(super) fn check(path: &str, source: &str) -> Syntax {
    let Some(extension) = path.rsplit_once('.').map(|(_, extension)| extension) else {
        return Syntax::Unchecked;
    };
    if extension.eq_ignore_ascii_case("rs") {
        return rust_file(path, source);
    }
    let Some(language) = Language::from_extension(extension) else {
        return Syntax::Unchecked;
    };
    match lexical_error(source, language) {
        Some(message) => Syntax::Refused(refusal(path, &message)),
        None => Syntax::Proven,
    }
}

fn rust_file(path: &str, source: &str) -> Syntax {
    match syn::parse_file(source) {
        Ok(_) => Syntax::Proven,
        Err(error) => {
            let text = error.to_string();
            let line = text.lines().next().unwrap_or("the file does not parse");
            Syntax::Refused(refusal(path, &brief(line)))
        }
    }
}

fn refusal(path: &str, message: &str) -> Value {
    json!({
        "status": "INVALID_PLAN",
        "reason": format!("{path}: the edit does not parse: {message}"),
    })
}

fn brief(line: &str) -> String {
    let mut short = line.chars().take(180).collect::<String>();
    if line.chars().count() > 180 {
        short.push('…');
    }
    short
}

fn lexical_error(source: &str, language: Language) -> Option<String> {
    let tokens = tokenize(source, language);
    if let Some(token) = tokens
        .iter()
        .find(|token| token.kind == TokenKind::Unterminated)
    {
        return Some(format!(
            "line {}: unterminated literal or comment",
            token.line
        ));
    }
    let mut stack = Vec::<&str>::new();
    for token in &tokens {
        if token.kind != TokenKind::Punctuation {
            continue;
        }
        let text = token.text(source);
        match text {
            "(" | "[" | "{" => stack.push(closer(text)),
            ")" | "]" | "}" if stack.pop() != Some(text) => {
                return Some(format!("line {}: unmatched '{text}'", token.line));
            }
            _ => {}
        }
    }
    stack
        .last()
        .map(|open| format!("unclosed delimiter, expected {open}"))
}

fn closer(open: &str) -> &'static str {
    match open {
        "(" => ")",
        "[" => "]",
        "{" => "}",
        _ => "",
    }
}
