//! Plan removal of exact duplicate functions and methods.
//!
//! Two declarations are duplicates when their source, including the name, is the same bytes.
//! One copy stays. A copy another symbol uses, an exported copy, and a copy whose own body can
//! load code dynamically stay as well. The plan is not written here.

use super::syntax_gate::{self, Syntax};
use crate::coordinates::utf16_offset;
use crate::declaration::locate;
use crate::evidence::{Risk, read_source};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_rust::{EdgeKind, NodeKind, RepositoryState};

const MAX_DELETIONS: usize = 200;

#[derive(Clone)]
struct Decl {
    id: String,
    label: String,
    kind: String,
    file: String,
    line: u32,
    start: usize,
    end: usize,
    text: String,
    reason: Option<&'static str>,
}

pub(super) fn remove_duplicates(state: &RepositoryState, arguments: &Value) -> Value {
    let selected = match selected_text(state, arguments) {
        Ok(text) => text,
        Err(refusal) => return refusal,
    };
    let decls = declarations(state);
    let mut deleted = choose(&decls, selected.as_deref());
    let mut kept = Vec::new();
    retain_safe(state, &decls, &mut deleted, &mut kept);
    if deleted.len() > MAX_DELETIONS {
        return json!({
            "status": "BLOCKED",
            "reason": format!(
                "{MAX_DELETIONS} is the deletion cap; narrow the call with symbol. Nothing was planned."
            ),
        });
    }
    if deleted.is_empty() {
        return json!({
            "status": "NO_EDITS",
            "reason": "no exact unused duplicate function or method",
            "kept": kept,
        });
    }
    let plan = match plan_for(state, &decls, &deleted) {
        Ok(plan) => plan,
        Err(refusal) => return refusal,
    };
    json!({
        "status": "PLANNED",
        "completeness": "PARTIAL",
        "deleted": rows(&decls, &deleted, None),
        "kept": kept,
        "plan": plan,
        "warnings": ["GRAPH_PROVEN_USES_ONLY"],
        "next": "apply with apply_edit_plan (preview -> confirm).",
    })
}

fn selected_text(state: &RepositoryState, arguments: &Value) -> Result<Option<String>, Value> {
    let Some(value) = arguments.get("symbol") else {
        return Ok(None);
    };
    let Some(symbol) = value.as_str() else {
        return Err(super::invalid_args("remove_duplicates", &["symbol"]));
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    if !is_callable(&node.kind) {
        return Err(not_symbol(
            symbol,
            "remove_duplicates removes a function or method",
        ));
    }
    let Some(span) = node.span.as_ref() else {
        return Err(not_symbol(symbol, "the symbol has no source range"));
    };
    let Some(source) = read_source(state.root(), &span.file) else {
        return Err(json!({
            "status": "SOURCE_UNAVAILABLE",
            "reason": format!("{}: the file is missing, too large, or not valid UTF-8", span.file),
        }));
    };
    let name = node.label.trim_end_matches("()");
    let Some(range) = locate(&source, &span.file, name, span.start.line) else {
        return Err(not_symbol(
            symbol,
            "the parser does not report this declaration",
        ));
    };
    if !range.end_proven {
        return Err(not_symbol(
            symbol,
            "the declaration end is not proven, so the duplicate span cannot be removed",
        ));
    }
    source.get(range.start..range.end).map_or_else(
        || Err(super::stale_graph(&span.file)),
        |text| Ok(Some(text.to_owned())),
    )
}

fn not_symbol(symbol: &str, reason: &str) -> Value {
    json!({
        "status": "NOT_A_SYMBOL",
        "reason": reason,
        "symbol": symbol,
    })
}

fn declarations(state: &RepositoryState) -> Vec<Decl> {
    let graph = state.graph();
    let mut sources = BTreeMap::<String, String>::new();
    let mut seen = BTreeSet::<(String, usize, usize)>::new();
    let mut found = Vec::new();
    for node in graph.nodes() {
        if !is_callable(&node.kind) {
            continue;
        }
        let Some(span) = node.span.as_ref() else {
            continue;
        };
        let file = span.file.clone();
        if !sources.contains_key(&file) {
            let Some(text) = read_source(state.root(), &file) else {
                continue;
            };
            sources.insert(file.clone(), text);
        }
        let Some(source) = sources.get(&file) else {
            continue;
        };
        let name = node.label.trim_end_matches("()");
        let Some(range) = locate(source, &file, name, span.start.line) else {
            continue;
        };
        if !range.end_proven || !seen.insert((file.clone(), range.start, range.end)) {
            continue;
        }
        let Some(text) = source.get(range.start..range.end) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Some(index) = graph.node_index(node.id.as_str()) else {
            continue;
        };
        let reason = keep_reason(graph, index, node, text);
        found.push(Decl {
            id: node.id.as_str().to_owned(),
            label: name.to_owned(),
            kind: node.kind.as_str().to_owned(),
            file,
            line: span.start.line,
            start: range.start,
            end: range.end,
            text: text.to_owned(),
            reason,
        });
    }
    drop_nested(&found)
}

fn keep_reason(
    graph: &weavatrix_graph::Graph,
    index: weavatrix_graph::NodeIndex,
    node: &weavatrix_graph::Node,
    text: &str,
) -> Option<&'static str> {
    let blocking = Risk::assess(node, Some(text)).blocking();
    if blocking.contains(&"EXTERNAL_CONSUMERS") {
        return Some("exported symbol may have callers outside this repository");
    }
    if blocking.contains(&"DYNAMIC_LOADING") || blocking.contains(&"REFLECTION") {
        return Some("the body can load code dynamically");
    }
    let used = graph
        .incoming_at(index)
        .any(|edge| edge.kind != EdgeKind::Contains);
    used.then_some("another symbol still uses this copy")
}

fn drop_nested(decls: &[Decl]) -> Vec<Decl> {
    decls
        .iter()
        .filter(|decl| {
            !decls.iter().any(|other| {
                other.file == decl.file
                    && other.start <= decl.start
                    && other.end >= decl.end
                    && (other.start != decl.start || other.end != decl.end)
            })
        })
        .cloned()
        .collect()
}

fn choose(decls: &[Decl], only: Option<&str>) -> Vec<usize> {
    let mut groups = BTreeMap::<&str, Vec<usize>>::new();
    for (index, decl) in decls.iter().enumerate() {
        if only.is_some_and(|text| text != decl.text) {
            continue;
        }
        groups.entry(decl.text.as_str()).or_default().push(index);
    }
    let mut deleted = Vec::new();
    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        let mut removable = members
            .iter()
            .copied()
            .filter(|index| decls[*index].reason.is_none())
            .collect::<Vec<_>>();
        let anchored = members.iter().any(|index| decls[*index].reason.is_some());
        sort_by_place(decls, &mut removable);
        if !anchored && !removable.is_empty() {
            removable.remove(0);
        }
        deleted.extend(removable);
    }
    deleted
}

fn sort_by_place(decls: &[Decl], indexes: &mut [usize]) {
    indexes.sort_by(|&left, &right| {
        (&decls[left].file, decls[left].line, &decls[left].id).cmp(&(
            &decls[right].file,
            decls[right].line,
            &decls[right].id,
        ))
    });
}

fn retain_safe(
    state: &RepositoryState,
    decls: &[Decl],
    deleted: &mut Vec<usize>,
    kept: &mut Vec<Value>,
) {
    let mut dropped = BTreeMap::<usize, &'static str>::new();
    drop_overlaps(decls, deleted, &mut dropped);
    drop_unparsed(state, decls, deleted, &mut dropped);
    for (index, decl) in decls.iter().enumerate() {
        let family = decls.iter().filter(|other| other.text == decl.text).count();
        if family < 2 {
            continue;
        }
        if deleted.contains(&index) {
            continue;
        }
        let reason = dropped
            .get(&index)
            .copied()
            .or(decl.reason)
            .unwrap_or("kept as the remaining copy");
        kept.push(row(decl, reason));
    }
}

fn drop_overlaps(
    decls: &[Decl],
    deleted: &mut Vec<usize>,
    dropped: &mut BTreeMap<usize, &'static str>,
) {
    let mut blocked = Vec::new();
    for index in deleted.iter().copied() {
        let decl = &decls[index];
        let overlaps = deleted.iter().any(|other| {
            if *other == index {
                return false;
            }
            let right = &decls[*other];
            decl.file == right.file && decl.start < right.end && right.start < decl.end
        });
        if overlaps {
            blocked.push(index);
        }
    }
    deleted.retain(|index| !blocked.contains(index));
    for index in blocked {
        dropped.insert(index, "the span overlaps another copy");
    }
}

fn drop_unparsed(
    state: &RepositoryState,
    decls: &[Decl],
    deleted: &mut Vec<usize>,
    dropped: &mut BTreeMap<usize, &'static str>,
) {
    let mut files = BTreeSet::<String>::new();
    for index in &*deleted {
        files.insert(decls[*index].file.clone());
    }
    let mut blocked = Vec::new();
    for file in files {
        let ranges = deleted
            .iter()
            .filter(|index| decls[**index].file == file)
            .map(|index| (decls[*index].start, decls[*index].end))
            .collect::<Vec<_>>();
        let Some(source) = read_source(state.root(), &file) else {
            blocked.extend(
                deleted
                    .iter()
                    .copied()
                    .filter(|index| decls[*index].file == file),
            );
            continue;
        };
        let Some(rewritten) = without_ranges(&source, &ranges) else {
            blocked.extend(
                deleted
                    .iter()
                    .copied()
                    .filter(|index| decls[*index].file == file),
            );
            continue;
        };
        if !matches!(syntax_gate::check(&file, &rewritten), Syntax::Proven) {
            blocked.extend(
                deleted
                    .iter()
                    .copied()
                    .filter(|index| decls[*index].file == file),
            );
        }
    }
    deleted.retain(|index| !blocked.contains(index));
    for index in blocked {
        dropped
            .entry(index)
            .or_insert("removing it would leave a file that does not parse");
    }
}

fn without_ranges(source: &str, ranges: &[(usize, usize)]) -> Option<String> {
    let mut ordered = ranges.to_vec();
    ordered.sort_by_key(|range| std::cmp::Reverse(range.0));
    let mut text = source.to_owned();
    for (start, end) in ordered {
        if start > end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return None;
        }
        text.replace_range(start..end, "");
    }
    Some(text)
}

fn plan_for(state: &RepositoryState, decls: &[Decl], deleted: &[usize]) -> Result<Value, Value> {
    let mut builder = PlanBuilder::new("remove_duplicates");
    let mut files = BTreeMap::<String, Vec<usize>>::new();
    for index in deleted {
        files
            .entry(decls[*index].file.clone())
            .or_default()
            .push(*index);
    }
    for (file, indexes) in files {
        let Some(source) = read_source(state.root(), &file) else {
            return Err(super::stale_graph(&file));
        };
        builder = builder.file(&file, &sha256_of(&source));
        for index in indexes {
            let decl = &decls[index];
            let (Some((start_line, start_column)), Some((end_line, end_column))) =
                (position(&source, decl.start), position(&source, decl.end))
            else {
                return Err(super::stale_graph(&file));
            };
            let (Ok(start_char), Ok(end_char)) = (
                utf16_offset(&source, start_line, start_column),
                utf16_offset(&source, end_line, end_column),
            ) else {
                return Err(super::stale_graph(&file));
            };
            builder = builder.edit(
                start_line,
                start_char,
                end_line,
                end_char,
                decl.text.clone(),
                "",
                "EXTRACTED",
            );
        }
    }
    Ok(builder.build())
}

fn position(source: &str, offset: usize) -> Option<(u32, u32)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let before = source.get(..offset)?;
    let line = u32::try_from(before.matches('\n').count() + 1).ok()?;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = u32::try_from(offset - line_start + 1).ok()?;
    Some((line, column))
}

fn rows(decls: &[Decl], indexes: &[usize], reason: Option<&str>) -> Vec<Value> {
    indexes
        .iter()
        .map(|index| row(&decls[*index], reason.unwrap_or("exact duplicate")))
        .collect()
}

fn row(decl: &Decl, reason: &str) -> Value {
    json!({
        "id": decl.id,
        "symbol": decl.label,
        "kind": decl.kind,
        "file": decl.file,
        "line": decl.line,
        "reason": reason,
    })
}

const fn is_callable(kind: &NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Method)
}

#[cfg(test)]
mod tests {
    use super::remove_duplicates;
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    #[test]
    fn a_repository_without_exact_copies_plans_nothing() {
        let state = fixture_state();
        let answer = remove_duplicates(&state, &json!({}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("NO_EDITS"),
            "{answer:?}"
        );
    }

    #[test]
    fn a_non_string_symbol_is_invalid_args() {
        let state = fixture_state();
        let answer = remove_duplicates(&state, &json!({"symbol": 1}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("INVALID_ARGS")
        );
    }
}
