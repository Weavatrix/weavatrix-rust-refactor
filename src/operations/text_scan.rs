//! Comments and quoted text for extensions the tokenizer does not know.
//!
//! A known language stays on `weavatrix-parse`. This scanner is only for the rest: line and
//! block comments, quotes, and — when asked — a `#` that begins a line. A `#` later on the line
//! stays code, so this never decides what a Rust attribute is.

/// Byte ranges of comments and strings. The end is exclusive.
pub(super) fn trivia_spans(source: &str, hash_lines: bool) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut index = 0;
    let mut line_start = true;
    while index < source.len() {
        let Some(ch) = source[index..].chars().next() else {
            break;
        };
        let width = ch.len_utf8();
        if ch == '\n' {
            line_start = true;
            index += width;
            continue;
        }
        if ch == ' ' || ch == '\t' || ch == '\r' {
            index += width;
            continue;
        }
        if hash_lines && line_start && ch == '#' {
            let end = line_end(source, index);
            spans.push((index, end));
            index = end;
            line_start = false;
            continue;
        }
        line_start = false;
        if ch == '/' && starts_with_at(source, index + width, "/") {
            let end = line_end(source, index);
            spans.push((index, end));
            index = end;
            continue;
        }
        if ch == '/' && starts_with_at(source, index + width, "*") {
            let rest = index + width + 1;
            let end = source[rest..]
                .find("*/")
                .map_or(source.len(), |offset| rest + offset + 2);
            spans.push((index, end));
            index = end;
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            let end = close_quote(source, index, ch);
            spans.push((index, end));
            index = end;
            continue;
        }
        index += width;
    }
    spans
}

fn starts_with_at(source: &str, index: usize, prefix: &str) -> bool {
    source
        .get(index..)
        .is_some_and(|rest| rest.starts_with(prefix))
}

fn line_end(source: &str, index: usize) -> usize {
    source[index..]
        .find('\n')
        .map_or(source.len(), |offset| index + offset)
}

fn close_quote(source: &str, start: usize, quote: char) -> usize {
    if quote == '"' && starts_with_at(source, start, "\"\"\"") {
        let rest = start + 3;
        return source[rest..]
            .find("\"\"\"")
            .map_or(source.len(), |offset| rest + offset + 3);
    }
    let mut index = start + quote.len_utf8();
    while index < source.len() {
        let Some(ch) = source[index..].chars().next() else {
            break;
        };
        let width = ch.len_utf8();
        if ch == '\\' {
            index += width;
            if let Some(next) = source[index..].chars().next() {
                index += next.len_utf8();
            }
            continue;
        }
        if ch == '\n' {
            return index;
        }
        index += width;
        if ch == quote {
            return index;
        }
    }
    source.len()
}
