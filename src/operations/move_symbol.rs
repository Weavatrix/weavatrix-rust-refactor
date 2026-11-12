//! What moving a declaration to another file would do, and the edit that does it.
//!
//! The cycle verdict is a projection from simulated edges, not a rebuild. The plan is the
//! mechanical half: cut the declaration out of its file, insert it at the end of `to_file`,
//! retarget imports that named it, and carry the imports the declaration itself uses.

use crate::coordinates::utf16_offset;
use crate::declaration::locate;
use crate::evidence::{declaring_file, read_source};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use weavatrix_rust::{EdgeKind, RepositoryState};

/// Which file each symbol lives in, by node id.
fn owners(state: &RepositoryState) -> BTreeMap<String, String> {
    state
        .graph()
        .nodes()
        .iter()
        .filter_map(|node| {
            let file = node
                .span
                .as_ref()
                .map_or_else(|| node.label.clone(), |span| span.file.clone());
            (!file.is_empty()).then(|| (node.id.as_str().to_owned(), file))
        })
        .collect()
}

/// File-level dependency edges, with the moved symbol's own edges redirected.
///
/// Only relations that make one file need another take part. Containment says where a symbol
/// lives, which is exactly what the move changes, so counting it would report the move as its
/// own consequence.
fn file_edges(
    state: &RepositoryState,
    owners: &BTreeMap<String, String>,
    moved: Option<(&str, &str)>,
) -> BTreeSet<(String, String)> {
    let owner = |id: &str| -> Option<String> {
        // The moved symbol is treated as already living in its destination.
        match moved {
            Some((moved_id, destination)) if id == moved_id => Some(destination.to_owned()),
            _ => owners.get(id).cloned(),
        }
    };
    let mut edges = BTreeSet::new();
    for edge in state.graph().edges() {
        if matches!(edge.kind, EdgeKind::Contains) {
            continue;
        }
        let (Some(source), Some(target)) =
            (owner(edge.source.as_str()), owner(edge.target.as_str()))
        else {
            continue;
        };
        if source != target {
            edges.insert((source, target));
        }
    }
    edges
}

/// Files reachable from `start`, for cycle detection.
fn reaches(edges: &BTreeSet<(String, String)>, start: &str, goal: &str) -> bool {
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (source, target) in edges {
        adjacency
            .entry(source.as_str())
            .or_default()
            .push(target.as_str());
    }
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start]);
    while let Some(current) = queue.pop_front() {
        if current == goal && !seen.is_empty() {
            return true;
        }
        if !seen.insert(current.to_owned()) {
            continue;
        }
        for next in adjacency.get(current).into_iter().flatten() {
            if *next == goal {
                return true;
            }
            queue.push_back(next);
        }
    }
    false
}

/// Every file-to-file cycle the edge set contains, as ordered pairs.
fn cycles(edges: &BTreeSet<(String, String)>) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (source, target) in edges {
        if source < target && edges.contains(&(target.clone(), source.clone())) {
            found.push((source.clone(), target.clone()));
        } else if source != target && reaches(edges, target, source) {
            let pair = (source.clone(), target.clone());
            if !found.contains(&pair) {
                found.push(pair);
            }
        }
    }
    found
}

pub(super) fn move_symbol(state: &RepositoryState, arguments: &Value) -> Value {
    let symbol = arguments.get("symbol").and_then(Value::as_str);
    let to_file = arguments.get("to_file").and_then(Value::as_str);
    let (Some(symbol), Some(to_file)) = (symbol, to_file) else {
        return super::invalid_args("move_symbol", &["symbol", "to_file"]);
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return super::not_found(state.graph(), symbol);
    };
    let Some(node) = state.graph().node_at(index) else {
        return super::not_found(state.graph(), symbol);
    };
    let Some(from) = declaring_file(node) else {
        return json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the selected node has no declaring file, so there is nothing to move",
            "symbol": symbol,
        });
    };
    if from == to_file {
        return json!({
            "status": "NO_CHANGE",
            "reason": format!("{symbol} already lives in {to_file}"),
        });
    }

    let id = node.id.as_str().to_owned();
    let owners = owners(state);
    let before = file_edges(state, &owners, None);
    let after = file_edges(state, &owners, Some((&id, to_file)));
    let cycles_before = cycles(&before);
    let cycles_after = cycles(&after);
    let introduced = cycles_after
        .iter()
        .filter(|cycle| !cycles_before.contains(cycle))
        .collect::<Vec<_>>();
    let removed = cycles_before
        .iter()
        .filter(|cycle| !cycles_after.contains(cycle))
        .collect::<Vec<_>>();

    let new_dependencies = after
        .iter()
        .filter(|edge| !before.contains(*edge))
        .map(|(source, target)| json!({"from": source, "to": target}))
        .collect::<Vec<_>>();
    let importers = state
        .graph()
        .edges()
        .iter()
        .filter(|edge| edge.kind != EdgeKind::Contains && edge.target.as_str() == id)
        .filter_map(|edge| owners.get(edge.source.as_str()).cloned())
        .filter(|file| *file != from)
        .collect::<BTreeSet<_>>();

    let verdict = if introduced.is_empty() {
        "FEASIBLE"
    } else {
        "WOULD_VIOLATE"
    };
    let name = node.label.trim_end_matches("()");
    let relocation = relocation_plan(state, node, &from, to_file, name);
    let uncertain = importers
        .iter()
        .filter(|file| {
            file.as_str() != to_file && !relocation.rewritten.iter().any(|done| done == *file)
        })
        .map(|file| {
            json!({
                "file": file,
                "reason": "this file depends on the symbol; its import path was not rewritten",
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": "EVALUATED",
        "verdict": verdict,
        "move": {"symbol": node.label, "from": from, "to": to_file},
        "cycles": {
            "introduced": introduced.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "removed": removed.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "before": cycles_before.len(),
            "after": cycles_after.len(),
        },
        "blastRadius": {
            "importers": importers.iter().collect::<Vec<_>>(),
            "newDependencies": new_dependencies,
        },
        "fidelity": "PROJECTED_FROM_GRAPH_EDGES",
        "plan": relocation.plan,
        "uncertainReferences": uncertain,
        "next": "review the cycle verdict, then apply the plan with apply_edit_plan \
                 (preview -> confirm). The plan moves the declaration and retargets the imports \
                 it can prove.",
    })
}

struct Relocation {
    plan: Option<Value>,
    rewritten: Vec<String>,
}

/// Cuts the declaration and appends it to `to_file`.
fn relocation_plan(
    state: &RepositoryState,
    node: &weavatrix_rust::Node,
    from: &str,
    to_file: &str,
    name: &str,
) -> Relocation {
    let Some(moved) = moved_text(state, node, from, name) else {
        return Relocation {
            plan: None,
            rewritten: Vec::new(),
        };
    };
    let destination = read_source(state.root(), to_file).unwrap_or_default();
    let origin = super::move_imports::Origin {
        source: &moved.source,
        from,
        to_file,
        name,
        declaration: &moved.declaration,
        at: (moved.range_start, moved.range_end),
        destination: &destination,
    };
    let repairs = super::move_imports::repairs(state, &origin);
    let mut builder = PlanBuilder::new("move_symbol")
        .file(from, &sha256_of(&moved.source))
        .edit(
            moved.start_line,
            moved.start_char,
            moved.end_line,
            moved.end_char,
            moved.before,
            repairs.leftover.clone(),
            "EXTRACTED",
        );
    if !destination.is_empty() || read_source(state.root(), to_file).is_some() {
        builder = append_declaration(
            builder,
            to_file,
            &destination,
            name,
            &moved.declaration,
            &repairs.carried,
        );
    }
    for edit in &repairs.edits {
        builder = push_import(builder, state, edit);
    }
    Relocation {
        plan: Some(builder.build()),
        rewritten: repairs.rewritten,
    }
}

struct MovedText {
    source: String,
    declaration: String,
    before: String,
    range_start: usize,
    range_end: usize,
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
}

fn moved_text(
    state: &RepositoryState,
    node: &weavatrix_rust::Node,
    from: &str,
    name: &str,
) -> Option<MovedText> {
    let source = read_source(state.root(), from)?;
    let line = node.span.as_ref().map_or(1, |span| span.start.line);
    let range = locate(&source, from, name, line)?;
    let mut end = range.end;
    if source.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }
    let before = source.get(range.start..end)?.to_owned();
    let declaration = source
        .get(range.start..range.end)?
        .trim_end_matches(['\n', '\r'])
        .to_owned();
    let (start_line, start_column) = byte_position(&source, range.start)?;
    let (end_line, end_column) = byte_position(&source, end)?;
    let start_char = utf16_offset(&source, start_line, start_column).ok()?;
    let end_char = utf16_offset(&source, end_line, end_column).ok()?;
    Some(MovedText {
        source,
        declaration,
        before,
        range_start: range.start,
        range_end: range.end,
        start_line,
        start_char,
        end_line,
        end_char,
    })
}

fn append_declaration(
    mut builder: PlanBuilder,
    to_file: &str,
    destination: &str,
    name: &str,
    declaration: &str,
    carried: &str,
) -> PlanBuilder {
    let Some((insert_line, insert_column)) = byte_position(destination, destination.len()) else {
        return builder;
    };
    let Ok(insert_char) = utf16_offset(destination, insert_line, insert_column) else {
        return builder;
    };
    let prefix = if destination.is_empty() || destination.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    builder = builder.file(to_file, &sha256_of(destination));
    builder = remove_sole_import(builder, destination, name);
    builder.edit(
        insert_line,
        insert_char,
        insert_line,
        insert_char,
        "",
        format!("{prefix}{carried}{declaration}\n"),
        "EXTRACTED",
    )
}

fn push_import(
    builder: PlanBuilder,
    state: &RepositoryState,
    edit: &super::move_imports::ImportEdit,
) -> PlanBuilder {
    let Some(source) = read_source(state.root(), &edit.file) else {
        return builder;
    };
    let Some(before) = source.get(edit.start..edit.end) else {
        return builder;
    };
    let Some((start_line, start_column)) = byte_position(&source, edit.start) else {
        return builder;
    };
    let Some((end_line, end_column)) = byte_position(&source, edit.end) else {
        return builder;
    };
    let (Ok(start_char), Ok(end_char)) = (
        utf16_offset(&source, start_line, start_column),
        utf16_offset(&source, end_line, end_column),
    ) else {
        return builder;
    };
    builder.file(&edit.file, &sha256_of(&source)).edit(
        start_line,
        start_char,
        end_line,
        end_char,
        before,
        edit.after.clone(),
        "EXTRACTED",
    )
}

fn byte_position(source: &str, offset: usize) -> Option<(u32, u32)> {
    if offset > source.len() || !source.is_char_boundary(offset) {
        return None;
    }
    let before = source.get(..offset)?;
    let line = u32::try_from(before.matches('\n').count() + 1).ok()?;
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let column = u32::try_from(offset - line_start + 1).ok()?;
    Some((line, column))
}

/// Deletes `use ...::name;` when that line imports nothing else.
fn remove_sole_import(mut builder: PlanBuilder, source: &str, name: &str) -> PlanBuilder {
    let mut offset = 0_usize;
    for line in source.split_inclusive('\n') {
        let end = offset + line.len();
        if sole_import(line.trim(), name) {
            builder = delete_bytes(builder, source, offset, end);
        }
        offset = end;
    }
    builder
}

fn sole_import(trimmed: &str, name: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("use ") else {
        return false;
    };
    if rest.contains('{') || rest.contains(',') {
        return false;
    }
    let rest = rest.trim_end_matches(';').trim();
    let imported = rest.rsplit("::").next().unwrap_or(rest).trim();
    let imported = imported
        .rsplit_once(" as ")
        .map_or(imported, |(_, alias)| alias.trim());
    imported == name
}

fn delete_bytes(builder: PlanBuilder, source: &str, start: usize, end: usize) -> PlanBuilder {
    let Some(before) = source.get(start..end) else {
        return builder;
    };
    let Some((start_line, start_column)) = byte_position(source, start) else {
        return builder;
    };
    let Some((end_line, end_column)) = byte_position(source, end) else {
        return builder;
    };
    let (Ok(start_char), Ok(end_char)) = (
        utf16_offset(source, start_line, start_column),
        utf16_offset(source, end_line, end_column),
    ) else {
        return builder;
    };
    builder.edit(
        start_line,
        start_char,
        end_line,
        end_char,
        before,
        "",
        "EXTRACTED",
    )
}
