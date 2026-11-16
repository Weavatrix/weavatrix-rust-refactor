//! Byte ranges turned into one edit plan.
//!
//! Extract planners locate a change in bytes, then convert it here. A range that misses a
//! character boundary is `STALE_GRAPH` rather than a write shifted onto the next character.

use crate::coordinates::utf16_offset;
use crate::plan::{PlanBuilder, sha256_of};
use blazingly_json::Value;

#[derive(Clone)]
pub(super) struct ByteEdit {
    pub start: usize,
    pub end: usize,
    pub after: String,
}

impl ByteEdit {
    pub(super) fn new(start: usize, end: usize, after: impl Into<String>) -> Self {
        Self {
            start,
            end,
            after: after.into(),
        }
    }
}

/// The 1-based line and 1-based byte column of an absolute byte offset.
#[must_use]
pub(super) fn position(source: &str, offset: usize) -> Option<(u32, u32)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let before = source.get(..offset)?;
    let line = u32::try_from(before.matches('\n').count().saturating_add(1)).ok()?;
    let line_start = before
        .rfind('\n')
        .map_or(0, |index| index.saturating_add(1));
    let column = u32::try_from(offset.saturating_sub(line_start).saturating_add(1)).ok()?;
    Some((line, column))
}

/// Absolute byte offset of a 1-based line and 1-based byte column.
#[must_use]
pub(super) fn offset_at(source: &str, line: u32, column: u32) -> Option<usize> {
    let mut offset = 0_usize;
    for (index, row) in source.split('\n').enumerate() {
        let number = u32::try_from(index).ok()?.saturating_add(1);
        if number != line {
            offset = offset.saturating_add(row.len()).saturating_add(1);
            continue;
        }
        let row = row.strip_suffix('\r').unwrap_or(row);
        let column = usize::try_from(column.saturating_sub(1)).ok()?;
        if column > row.len() || !row.is_char_boundary(column) {
            return None;
        }
        return Some(offset.saturating_add(column));
    }
    None
}

/// `src/subtitle/srt.rs` becomes `crate::subtitle::srt`. `src/lib.rs` becomes `crate`.
#[must_use]
pub(super) fn module_path(file: &str) -> Option<String> {
    let path = file.replace('\\', "/");
    let rest = path.strip_suffix(".rs")?;
    let rest = rest.strip_prefix("./").unwrap_or(rest);
    let rest = rest.strip_prefix("src/").unwrap_or(rest);
    let rest = rest.strip_suffix("/mod").unwrap_or(rest);
    if rest.is_empty() || rest == "lib" || rest == "main" {
        return Some("crate".to_owned());
    }
    let parts = rest.split('/').collect::<Vec<_>>();
    if parts.iter().any(|part| !is_ident(part)) {
        return None;
    }
    Some(format!("crate::{}", parts.join("::")))
}

fn is_ident(part: &str) -> bool {
    let mut chars = part.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// Whether two edits would land on the same bytes.
#[must_use]
pub(super) fn clashes(edits: &[ByteEdit]) -> bool {
    let mut ordered = edits.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|edit| (edit.start, edit.end));
    ordered.windows(2).any(|pair| {
        let (left, right) = (pair[0], pair[1]);
        (left.start == right.start && left.end == right.end)
            || (left.start < right.end && right.start < left.end)
    })
}

/// Applies edits from the end so earlier offsets stay valid.
#[must_use]
pub(super) fn rewrite(source: &str, edits: &[ByteEdit]) -> Option<String> {
    if clashes(edits) {
        return None;
    }
    let mut ordered = edits.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|edit| std::cmp::Reverse(edit.start));
    let mut text = source.to_owned();
    for edit in ordered {
        if edit.start > edit.end
            || edit.end > text.len()
            || !text.is_char_boundary(edit.start)
            || !text.is_char_boundary(edit.end)
        {
            return None;
        }
        let before = text.get(edit.start..edit.end)?;
        if before == edit.after {
            return None;
        }
        text.replace_range(edit.start..edit.end, &edit.after);
    }
    Some(text)
}

/// Whether `file` is Rust source, ignoring the extension's case.
#[must_use]
pub(super) fn is_rust(file: &str) -> bool {
    std::path::Path::new(file)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
}

pub(super) fn parses(path: &str, source: &str) -> bool {
    matches!(
        super::syntax_gate::check(path, source),
        super::syntax_gate::Syntax::Proven
    )
}

pub(super) fn push_edits(
    mut builder: PlanBuilder,
    file: &str,
    source: &str,
    edits: &[ByteEdit],
) -> Result<PlanBuilder, Value> {
    builder = builder.file(file, &sha256_of(source));
    for edit in edits {
        let Some(before) = source.get(edit.start..edit.end) else {
            return Err(super::stale_graph(file));
        };
        let Some((start_line, start_column)) = position(source, edit.start) else {
            return Err(super::stale_graph(file));
        };
        let Some((end_line, end_column)) = position(source, edit.end) else {
            return Err(super::stale_graph(file));
        };
        let Ok(start_char) = utf16_offset(source, start_line, start_column) else {
            return Err(super::stale_graph(file));
        };
        let Ok(end_char) = utf16_offset(source, end_line, end_column) else {
            return Err(super::stale_graph(file));
        };
        builder = builder.edit(
            start_line,
            start_char,
            end_line,
            end_char,
            before,
            edit.after.clone(),
            "EXTRACTED",
        );
    }
    Ok(builder)
}
