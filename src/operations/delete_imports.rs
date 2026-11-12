//! Import statements that would dangle after a deletion.
//!
//! A deleted file, or a named binding whose symbol is deleted, is removed from the importing
//! file in the same plan. A relative specifier is the only kind resolved: a package name is not
//! a path, and rewriting one would delete an import that still points somewhere else.

use crate::specifier::{is_relative, resolve};
use std::collections::BTreeSet;
use weavatrix_graph::{SourcePosition, SourceSpan};
use weavatrix_parse::Import;
use weavatrix_rust::{NodeKind, RepositoryState};

/// One symbol the closure deletes, by its source name and declaring file.
pub(super) struct RemovedSymbol {
    pub(super) name: String,
    pub(super) file: String,
}

/// Import ranges to delete, skipping any range that already overlaps a planned edit.
#[must_use]
pub(super) fn statement_spans(
    state: &RepositoryState,
    deleted_files: &[String],
    removed: &[RemovedSymbol],
    occupied: &[SourceSpan],
) -> Vec<SourceSpan> {
    let mut spans = Vec::new();
    for file in indexed_files(state) {
        if deleted_files.iter().any(|path| path == &file) {
            continue;
        }
        let Some(source) = crate::evidence::read_source(state.root(), &file) else {
            continue;
        };
        let Some(facts) = weavatrix_parse::extract_path(&file, &source) else {
            continue;
        };
        for import in &facts.imports {
            spans.extend(ranges_for(
                &file,
                &source,
                import,
                deleted_files,
                removed,
                occupied,
            ));
        }
    }
    spans.sort();
    spans.dedup();
    spans
}

fn ranges_for(
    file: &str,
    source: &str,
    import: &Import,
    deleted_files: &[String],
    removed: &[RemovedSymbol],
    occupied: &[SourceSpan],
) -> Vec<SourceSpan> {
    let selected = selected_bindings(file, import, removed);
    let whole = points_at_deleted(file, &import.specifier, deleted_files)
        || (!import.bindings.is_empty() && selected.len() == import.bindings.len())
        || (import.bindings.is_empty()
            && !import.names.is_empty()
            && selected.len() == import.names.len());
    let mut spans = Vec::new();
    if whole {
        if let Some(span) = whole_statement(file, source, import)
            && !hits(occupied, &span)
        {
            spans.push(span);
        }
        return spans;
    }
    for range in binding_ranges(source, import, &selected) {
        let Some(span) = span_from(file, source, range) else {
            continue;
        };
        if hits(occupied, &span) {
            continue;
        }
        spans.push(span);
    }
    spans
}

fn selected_bindings(file: &str, import: &Import, removed: &[RemovedSymbol]) -> Vec<String> {
    let pairs = if import.bindings.is_empty() {
        import
            .names
            .iter()
            .map(|name| (name.clone(), name.clone()))
            .collect::<Vec<_>>()
    } else {
        import
            .bindings
            .iter()
            .map(|binding| (binding.imported.clone(), binding.local.clone()))
            .collect()
    };
    pairs
        .into_iter()
        .filter(|(imported, local)| {
            removed.iter().any(|symbol| {
                (symbol.name == *imported || symbol.name == *local)
                    && same_file(file, &import.specifier, &symbol.file)
            })
        })
        .map(|(_, local)| local)
        .collect()
}

fn points_at_deleted(importer: &str, specifier: &str, deleted_files: &[String]) -> bool {
    deleted_files
        .iter()
        .any(|file| same_file(importer, specifier, file))
}

fn same_file(importer: &str, specifier: &str, target: &str) -> bool {
    if !is_relative(specifier) {
        return false;
    }
    let Some(resolved) = resolve(importer, specifier) else {
        return false;
    };
    resolved == target
        || target
            .rsplit_once('.')
            .is_some_and(|(stem, _)| resolved == stem)
        || resolved
            .rsplit_once('.')
            .is_some_and(|(stem, _)| stem == target)
}

fn whole_statement(file: &str, source: &str, import: &Import) -> Option<SourceSpan> {
    let (start, end) = line_bounds(source, import.span.start, import.span.end)?;
    span_from(file, source, (start, end))
}

fn line_bounds(source: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    if start > end || end > source.len() {
        return None;
    }
    let line_start = source
        .get(..start)?
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let start = if source
        .get(line_start..start)?
        .chars()
        .all(char::is_whitespace)
    {
        line_start
    } else {
        start
    };
    let rest = source.get(end..)?;
    let trailing = rest
        .find('\n')
        .filter(|index| {
            rest.get(..*index)
                .is_some_and(|tail| tail.chars().all(char::is_whitespace))
        })
        .map_or(0, |index| index + 1);
    Some((start, end + trailing))
}

fn binding_ranges(source: &str, import: &Import, selected: &[String]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let pairs = if import.bindings.is_empty() {
        import
            .names
            .iter()
            .map(|name| (name.as_str(), name.as_str()))
            .collect::<Vec<_>>()
    } else {
        import
            .bindings
            .iter()
            .map(|binding| (binding.imported.as_str(), binding.local.as_str()))
            .collect()
    };
    for (imported, local) in pairs {
        if !selected.iter().any(|name| name == local) {
            continue;
        }
        let Some(statement) = source.get(import.span.start..import.span.end) else {
            continue;
        };
        let Some(found) = binding_at(statement, imported, local) else {
            continue;
        };
        let start = import.span.start + found.0;
        let end = import.span.start + found.1;
        ranges.push(comma_around(
            source,
            (import.span.start, import.span.end),
            start,
            end,
        ));
    }
    ranges
}

fn binding_at(statement: &str, imported: &str, local: &str) -> Option<(usize, usize)> {
    if imported != local {
        let phrase = format!("{imported} as {local}");
        if let Some(start) = statement.find(&phrase) {
            return Some((start, start + phrase.len()));
        }
    }
    let start = word_at(statement, local)?;
    Some((start, start + local.len()))
}

fn word_at(text: &str, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let mut from = 0_usize;
    while let Some(found) = text.get(from..)?.find(name) {
        let start = from + found;
        let end = start + name.len();
        let before = text
            .get(..start)
            .and_then(|prefix| prefix.chars().next_back());
        let after = text.get(end..).and_then(|rest| rest.chars().next());
        let boundary = |character: Option<char>| {
            character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
        };
        if boundary(before) && boundary(after) {
            return Some(start);
        }
        from = start + 1;
    }
    None
}

fn comma_around(source: &str, limit: (usize, usize), start: usize, end: usize) -> (usize, usize) {
    let (floor, ceil) = limit;
    let Some(rest) = source.get(end..ceil) else {
        return (start, end);
    };
    let spaces = rest
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .map(char::len_utf8)
        .sum::<usize>();
    if source
        .get(end + spaces..ceil)
        .is_some_and(|tail| tail.starts_with(','))
    {
        return (start, end + spaces + 1);
    }
    let Some(before) = source.get(floor..start) else {
        return (start, end);
    };
    let Some(comma) = before.rfind(',') else {
        return (start, end);
    };
    if before
        .get(comma + 1..)
        .is_some_and(|gap| gap.chars().all(char::is_whitespace))
    {
        return (floor + comma, end);
    }
    (start, end)
}

fn span_from(file: &str, source: &str, range: (usize, usize)) -> Option<SourceSpan> {
    Some(SourceSpan::new(
        file.to_owned(),
        at(source, range.0)?,
        at(source, range.1)?,
    ))
}

fn at(source: &str, offset: usize) -> Option<SourcePosition> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let before = source.get(..offset)?;
    let line = u32::try_from(before.matches('\n').count().saturating_add(1)).ok()?;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = u32::try_from(offset.saturating_sub(line_start).saturating_add(1)).ok()?;
    Some(SourcePosition::new(line, column))
}

fn hits(occupied: &[SourceSpan], span: &SourceSpan) -> bool {
    occupied
        .iter()
        .any(|other| other.file == span.file && other.start < span.end && span.start < other.end)
}

fn indexed_files(state: &RepositoryState) -> BTreeSet<String> {
    state
        .graph()
        .nodes()
        .iter()
        .filter(|node| node.kind == NodeKind::File && !node.label.is_empty())
        .map(|node| node.label.clone())
        .collect()
}
