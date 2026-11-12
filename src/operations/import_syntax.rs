//! Import statements for languages the original counters did not read.
//!
//! Java, C# and C++ are tokenised by `weavatrix-parse`. Kotlin is not a language in parse 0.3.5,
//! so its imports are read from the line. A star, a header and a namespace stay in place: an
//! occurrence count cannot prove them unused. A name that appears only inside a string or a
//! comment is not a use.

use super::organize_imports::{Binding, Import, Unnamed, is_code};
use super::text_scan::trivia_spans;
use weavatrix_parse::{Token, TokenKind};

const STAR: &str = "a star import has no single name an occurrence count can prove";
const HEADER: &str = "a header can be required for macros or types that do not name the file, \
     so it was not removed";
const STATIC_USING: &str = "a static using brings members into scope without the type name \
     appearing, so it was not removed";
const NAMESPACE: &str = "a using-directive names a namespace, so it was not removed";

pub(super) fn java_imports(source: &str, tokens: &[Token]) -> Vec<Import> {
    let code = code_tokens(tokens);
    let mut found = Vec::new();
    let mut at = 0;
    while at < code.len() {
        if code[at].text(source) != "import" {
            at += 1;
            continue;
        }
        let start = code[at].start;
        at += 1;
        if code
            .get(at)
            .is_some_and(|token| token.text(source) == "static")
        {
            at += 1;
        }
        let (last, star, end) = walk_import(source, &code, &mut at);
        if star {
            found.push(unnamed(start, end, "*", STAR));
        } else if let Some(name) = last {
            found.push(named(start, end, name.text(source), name.start, name.end));
        }
    }
    found
}

pub(super) fn csharp_imports(source: &str, tokens: &[Token]) -> Vec<Import> {
    read_usings(source, tokens, true)
}

pub(super) fn cpp_imports(source: &str, tokens: &[Token]) -> Vec<Import> {
    let code = code_tokens(tokens);
    let mut found = Vec::new();
    let mut at = 0;
    while at < code.len() {
        if code[at].text(source) == "#"
            && code
                .get(at + 1)
                .is_some_and(|token| token.text(source) == "include")
        {
            found.push(read_include(source, &code, &mut at));
            continue;
        }
        if code[at].text(source) == "using" {
            found.push(read_using(source, &code, &mut at, false));
            continue;
        }
        at += 1;
    }
    found
}

pub(super) fn kotlin_imports(source: &str) -> Vec<Import> {
    let trivia = trivia_spans(source, false);
    let mut found = Vec::new();
    let mut offset = 0;
    for line in source.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let Some(import_at) = line.find("import") else {
            continue;
        };
        let start = line_start + import_at;
        if in_span(&trivia, start) {
            continue;
        }
        let after = &line[import_at + "import".len()..];
        if !after.chars().next().is_some_and(char::is_whitespace) {
            continue;
        }
        let body = after
            .split("//")
            .next()
            .unwrap_or(after)
            .trim()
            .trim_end_matches(';')
            .trim();
        if body.is_empty() {
            continue;
        }
        let end = line_start + line.trim_end_matches(['\n', '\r']).len();
        if body.ends_with(".*") {
            found.push(unnamed(start, end, "*", STAR));
            continue;
        }
        let local = body.split_once(" as ").map_or_else(
            || body.rsplit('.').next().unwrap_or(body),
            |(_, alias)| alias.trim(),
        );
        if local.is_empty() {
            continue;
        }
        found.push(named(start, end, local, start, end));
    }
    found
}

/// Identifiers outside import statements, comments and strings.
pub(super) fn textual_names(source: &str, statements: &[Import]) -> Vec<String> {
    let trivia = trivia_spans(source, false);
    let mut names = Vec::new();
    let mut chars = source.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if in_span(&trivia, index) || in_statement(statements, index) {
            continue;
        }
        if !is_ident_start(ch) {
            continue;
        }
        let mut end = index + ch.len_utf8();
        while let Some((next, next_ch)) = chars.peek().copied() {
            if !is_ident_continue(next_ch) {
                break;
            }
            end = next + next_ch.len_utf8();
            chars.next();
        }
        if let Some(name) = source.get(index..end) {
            names.push(name.to_owned());
        }
    }
    names
}

fn read_usings(source: &str, tokens: &[Token], static_is_unnamed: bool) -> Vec<Import> {
    let code = code_tokens(tokens);
    let mut found = Vec::new();
    let mut at = 0;
    while at < code.len() {
        if code[at].text(source) != "using" {
            at += 1;
            continue;
        }
        found.push(read_using(source, &code, &mut at, static_is_unnamed));
    }
    found
}

fn read_using(source: &str, code: &[&Token], at: &mut usize, static_is_unnamed: bool) -> Import {
    let start = code[*at].start;
    *at += 1;
    if static_is_unnamed
        && code
            .get(*at)
            .is_some_and(|token| token.text(source) == "static")
    {
        let end = consume_statement(source, code, at);
        return unnamed(start, end, "static", STATIC_USING);
    }
    if code
        .get(*at)
        .is_some_and(|token| token.text(source) == "namespace")
    {
        let end = consume_statement(source, code, at);
        return unnamed(start, end, "namespace", NAMESPACE);
    }
    let mut alias: Option<&Token> = None;
    let mut last: Option<&Token> = None;
    let mut saw_eq = false;
    while *at < code.len() && code[*at].text(source) != ";" {
        if code[*at].text(source) == "=" {
            saw_eq = true;
            alias = last;
        } else if code[*at].kind == TokenKind::Identifier {
            last = Some(code[*at]);
        }
        *at += 1;
    }
    let end = code.get(*at).map_or(start, |token| token.end);
    if *at < code.len() {
        *at += 1;
    }
    let chosen = if saw_eq { alias } else { last };
    chosen.map_or_else(
        || unnamed(start, end, "using", NAMESPACE),
        |name| named(start, end, name.text(source), name.start, name.end),
    )
}

fn read_include(source: &str, code: &[&Token], at: &mut usize) -> Import {
    let start = code[*at].start;
    let line = code[*at].line;
    *at += 2;
    let mut end = start;
    let mut depth = 0_i32;
    while *at < code.len() {
        let token = code[*at];
        if depth == 0 && token.line != line {
            break;
        }
        let text = token.text(source);
        if text == "<" {
            depth += 1;
        } else if text == ">" && depth > 0 {
            depth -= 1;
        }
        end = token.end;
        *at += 1;
        if depth == 0 && code.get(*at).is_some_and(|next| next.line != line) {
            break;
        }
    }
    unnamed(start, end, "include", HEADER)
}

fn walk_import<'code>(
    source: &str,
    code: &[&'code Token],
    at: &mut usize,
) -> (Option<&'code Token>, bool, usize) {
    let mut last = None;
    let mut star = false;
    let mut end = code.get(*at).map_or(0, |token| token.start);
    while *at < code.len() && code[*at].text(source) != ";" {
        if code[*at].text(source) == "*" {
            star = true;
        } else if code[*at].kind == TokenKind::Identifier {
            last = Some(code[*at]);
        }
        end = code[*at].end;
        *at += 1;
    }
    if let Some(token) = code.get(*at) {
        end = token.end;
        *at += 1;
    }
    (last, star, end)
}

fn consume_statement(source: &str, code: &[&Token], at: &mut usize) -> usize {
    let mut end = code.get(*at).map_or(0, |token| token.start);
    while *at < code.len() && code[*at].text(source) != ";" {
        end = code[*at].end;
        *at += 1;
    }
    if let Some(token) = code.get(*at) {
        end = token.end;
        *at += 1;
    }
    end
}

fn code_tokens(tokens: &[Token]) -> Vec<&Token> {
    tokens.iter().filter(|token| is_code(token)).collect()
}

fn named(start: usize, end: usize, local: &str, local_start: usize, local_end: usize) -> Import {
    Import {
        start,
        end,
        named: vec![Binding {
            local: local.to_owned(),
            start: local_start,
            end: local_end,
        }],
        braces: None,
        unnamed: Vec::new(),
    }
}

fn unnamed(start: usize, end: usize, binding: &str, reason: &'static str) -> Import {
    Import {
        start,
        end,
        named: Vec::new(),
        braces: None,
        unnamed: vec![Unnamed {
            binding: binding.to_owned(),
            reason,
        }],
    }
}

fn in_span(spans: &[(usize, usize)], index: usize) -> bool {
    spans.iter().any(|span| index >= span.0 && index < span.1)
}

fn in_statement(statements: &[Import], index: usize) -> bool {
    statements
        .iter()
        .any(|statement| index >= statement.start && index < statement.end)
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}
