//! Plan extraction of a used exact duplicate function.
//!
//! `remove_duplicates` keeps a copy another symbol still calls. This planner keeps the earliest
//! non-exported copy, deletes the later ones, and retargets the calls the graph proved. Methods
//! of different types are not interchangeable; those stay for `share_method_body`. A family whose
//! copies are all unused stays with `remove_duplicates`. The plan is not written here.

use super::edits::{self, ByteEdit};
use super::rename_symbol::referencing_lines;
use crate::declaration::locate;
use crate::evidence::{Risk, read_source};
use crate::plan::PlanBuilder;
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_parse::{Language, TokenKind, tokenize};
use weavatrix_rust::{EdgeKind, NodeKind, RepositoryState};

const MAX_DELETIONS: usize = 200;
const NEXT: &str = "apply with apply_edit_plan (preview -> confirm).";

#[derive(Clone)]
struct Copy {
    id: String,
    label: String,
    kind: String,
    file: String,
    line: u32,
    start: usize,
    end: usize,
    text: String,
    used: bool,
    blocked: Option<&'static str>,
}

#[derive(Clone)]
struct Family {
    keeper: usize,
    removed: Vec<usize>,
    edits: BTreeMap<String, Vec<ByteEdit>>,
}

pub(super) fn extract_duplicate(state: &RepositoryState, arguments: &Value) -> Value {
    let selected = match selected(state, arguments) {
        Ok(text) => text,
        Err(refusal) => return refusal,
    };
    let copies = declarations(state);
    let mut kept = Vec::new();
    let mut families = Vec::new();
    for members in groups(&copies, selected.as_deref()) {
        consider(state, &copies, &members, &mut families, &mut kept);
    }
    if selected.is_none() {
        method_hints(&copies, &mut kept);
    }
    let families = accepting(state, &copies, families, &mut kept);
    let removed = families
        .iter()
        .map(|family| family.removed.len())
        .sum::<usize>();
    if removed > MAX_DELETIONS {
        return json!({
            "status": "BLOCKED",
            "reason": format!(
                "{MAX_DELETIONS} is the deletion cap; narrow the call with symbol. Nothing was planned."
            ),
        });
    }
    if families.is_empty() {
        return json!({
            "status": "NO_EDITS",
            "reason": if kept.iter().any(|row| {
                row.get("reason").and_then(Value::as_str) == Some(
                    "identical methods stay on their types; use share_method_body",
                )
            }) {
                "no used exact duplicate function; identical methods use share_method_body"
            } else {
                "no used exact duplicate function"
            },
            "kept": kept,
        });
    }
    let plan = match plan_for(state, &families) {
        Ok(plan) => plan,
        Err(refusal) => return refusal,
    };
    let deleted = families
        .iter()
        .flat_map(|family| {
            family
                .removed
                .iter()
                .map(|index| row(&copies[*index], "exact duplicate"))
        })
        .collect::<Vec<_>>();
    json!({
        "status": "PLANNED",
        "completeness": "PARTIAL",
        "deleted": deleted,
        "kept": kept,
        "plan": plan,
        "warnings": ["GRAPH_PROVEN_USES_ONLY"],
        "next": NEXT,
    })
}

fn selected(state: &RepositoryState, arguments: &Value) -> Result<Option<String>, Value> {
    let Some(value) = arguments.get("symbol") else {
        return Ok(None);
    };
    let Some(symbol) = value.as_str() else {
        return Err(super::invalid_args("extract_duplicate", &["symbol"]));
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    if node.kind == NodeKind::Method {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "extract_duplicate moves a function; identical methods of different types use share_method_body",
            "symbol": symbol,
        }));
    }
    if node.kind != NodeKind::Function {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "extract_duplicate moves a function",
            "symbol": symbol,
        }));
    }
    let Some(span) = node.span.as_ref() else {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the symbol has no source range",
            "symbol": symbol,
        }));
    };
    let Some(source) = read_source(state.root(), &span.file) else {
        return Err(json!({
            "status": "SOURCE_UNAVAILABLE",
            "reason": format!("{}: the file is missing, too large, or not valid UTF-8", span.file),
        }));
    };
    let name = node.label.trim_end_matches("()");
    let Some(range) = locate(&source, &span.file, name, span.start.line) else {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the parser does not report this declaration",
            "symbol": symbol,
        }));
    };
    if !range.end_proven {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the declaration end is not proven, so the duplicate span cannot be moved",
            "symbol": symbol,
        }));
    }
    source.get(range.start..range.end).map_or_else(
        || Err(super::stale_graph(&span.file)),
        |text| Ok(Some(text.to_owned())),
    )
}

fn declarations(state: &RepositoryState) -> Vec<Copy> {
    let graph = state.graph();
    let mut sources = BTreeMap::<String, String>::new();
    let mut seen = BTreeSet::<(String, usize, usize)>::new();
    let mut found = Vec::new();
    for node in graph.nodes() {
        if !matches!(node.kind, NodeKind::Function | NodeKind::Method) {
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
        let blocked = block_reason(node, text);
        let used = graph
            .incoming_at(index)
            .any(|edge| edge.kind != EdgeKind::Contains);
        found.push(Copy {
            id: node.id.as_str().to_owned(),
            label: name.to_owned(),
            kind: node.kind.as_str().to_owned(),
            file,
            line: span.start.line,
            start: range.start,
            end: range.end,
            text: text.to_owned(),
            used,
            blocked,
        });
    }
    drop_nested(&found)
}

fn block_reason(node: &weavatrix_graph::Node, text: &str) -> Option<&'static str> {
    let blocking = Risk::assess(node, Some(text)).blocking();
    if blocking.contains(&"EXTERNAL_CONSUMERS") {
        return Some("exported symbol may have callers outside this repository");
    }
    if blocking.contains(&"DYNAMIC_LOADING") || blocking.contains(&"REFLECTION") {
        return Some("the body can load code dynamically");
    }
    None
}

fn drop_nested(copies: &[Copy]) -> Vec<Copy> {
    copies
        .iter()
        .filter(|copy| {
            !copies.iter().any(|other| {
                other.file == copy.file
                    && other.start <= copy.start
                    && other.end >= copy.end
                    && (other.start != copy.start || other.end != copy.end)
            })
        })
        .cloned()
        .collect()
}

fn groups(copies: &[Copy], only: Option<&str>) -> Vec<Vec<usize>> {
    let mut grouped = BTreeMap::<&str, Vec<usize>>::new();
    for (index, copy) in copies.iter().enumerate() {
        if only.is_some_and(|text| text != copy.text) {
            continue;
        }
        grouped.entry(copy.text.as_str()).or_default().push(index);
    }
    grouped
        .into_values()
        .filter(|members| members.len() >= 2)
        .collect()
}

fn consider(
    state: &RepositoryState,
    copies: &[Copy],
    members: &[usize],
    families: &mut Vec<Family>,
    kept: &mut Vec<Value>,
) {
    let functions = members
        .iter()
        .copied()
        .filter(|index| copies[*index].kind == "function")
        .collect::<Vec<_>>();
    if functions.len() < 2 {
        return;
    }
    if functions.iter().all(|index| !copies[*index].used) {
        return;
    }
    if functions
        .iter()
        .any(|index| !edits::is_rust(&copies[*index].file))
    {
        for index in functions {
            kept.push(row(&copies[index], "call retarget is implemented for Rust"));
        }
        return;
    }
    let mut eligible = functions
        .iter()
        .copied()
        .filter(|index| copies[*index].used && copies[*index].blocked.is_none())
        .collect::<Vec<_>>();
    sort_by_place(copies, &mut eligible);
    if eligible.len() < 2 {
        for index in functions {
            if let Some(reason) = copies[index].blocked {
                kept.push(row(&copies[index], reason));
            }
        }
        return;
    }
    let keeper = eligible.remove(0);
    let Some(module) = edits::module_path(&copies[keeper].file) else {
        kept.push(row(&copies[keeper], "the module path is not a Rust path"));
        return;
    };
    let callee = format!("{module}::{}", copies[keeper].label);
    let mut removed = Vec::new();
    let mut edits = BTreeMap::<String, Vec<ByteEdit>>::new();
    for index in eligible {
        match retarget(state, &copies[index], &callee) {
            Ok(sites) => {
                removed.push(index);
                for (file, edit) in sites {
                    edits.entry(file).or_default().push(edit);
                }
                edits
                    .entry(copies[index].file.clone())
                    .or_default()
                    .push(ByteEdit::new(copies[index].start, copies[index].end, ""));
            }
            Err(reason) => kept.push(row(&copies[index], reason)),
        }
    }
    if removed.is_empty() {
        return;
    }
    let cross_file = edits.keys().any(|file| file != &copies[keeper].file);
    if cross_file && let Some(edit) = qualify(&copies[keeper]) {
        edits
            .entry(copies[keeper].file.clone())
            .or_default()
            .push(edit);
    }
    if !family_parses(state, &edits) {
        kept.push(row(&copies[keeper], "the edit does not parse"));
        return;
    }
    kept.push(row(&copies[keeper], "kept as the remaining copy"));
    families.push(Family {
        keeper,
        removed,
        edits,
    });
}

fn retarget(
    state: &RepositoryState,
    copy: &Copy,
    callee: &str,
) -> Result<Vec<(String, ByteEdit)>, &'static str> {
    let mut edits = Vec::new();
    let mut missed = false;
    for (file, lines) in referencing_lines(state, &copy.id) {
        let Some(source) = read_source(state.root(), &file) else {
            return Err("a call site was not located");
        };
        for line in lines {
            if file == copy.file && line_in_copy(&source, line, copy) {
                continue;
            }
            let hits = bare_hits(&source, line, &copy.label);
            if hits.is_empty() {
                missed = true;
                continue;
            }
            for (start, end) in hits {
                let Some(absolute) =
                    edits::offset_at(&source, line, u32::try_from(start).unwrap_or(u32::MAX))
                else {
                    missed = true;
                    continue;
                };
                let Some(finish) =
                    edits::offset_at(&source, line, u32::try_from(end).unwrap_or(u32::MAX))
                else {
                    missed = true;
                    continue;
                };
                if file == copy.file && absolute >= copy.start && absolute < copy.end {
                    continue;
                }
                edits.push((file.clone(), ByteEdit::new(absolute, finish, callee)));
            }
        }
    }
    if missed {
        return Err("a call site was not located");
    }
    Ok(edits)
}

fn line_in_copy(source: &str, line: u32, copy: &Copy) -> bool {
    let Some(start) = edits::offset_at(source, line, 1) else {
        return false;
    };
    let end = source
        .get(start..)
        .and_then(|rest| rest.find('\n').map(|index| start.saturating_add(index)))
        .unwrap_or(source.len());
    start < copy.end && copy.start < end
}

fn bare_hits(source: &str, line: u32, name: &str) -> Vec<(usize, usize)> {
    let Some(text) = source
        .split('\n')
        .nth(usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX))
    else {
        return Vec::new();
    };
    let text = text.strip_suffix('\r').unwrap_or(text);
    let mut hits = Vec::new();
    let mut from = 0_usize;
    while let Some(found) = text.get(from..).and_then(|rest| rest.find(name)) {
        let start = from.saturating_add(found);
        let end = start.saturating_add(name.len());
        from = start.saturating_add(1);
        let before = text
            .get(..start)
            .and_then(|prefix| prefix.chars().next_back());
        let after = text.get(end..).and_then(|rest| rest.chars().next());
        let boundary = |character: Option<char>| {
            character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
        };
        if boundary(before) && boundary(after) && before != Some(':') && before != Some('.') {
            hits.push((start.saturating_add(1), end.saturating_add(1)));
        }
    }
    hits
}

fn qualify(copy: &Copy) -> Option<ByteEdit> {
    let brace = signature_end(&copy.text)?;
    let signature = copy.text.get(..brace)?;
    let (start, end, after) = visibility(signature)?;
    Some(ByteEdit::new(
        copy.start.saturating_add(start),
        copy.start.saturating_add(end),
        after,
    ))
}

fn signature_end(text: &str) -> Option<usize> {
    let tokens = tokenize(text, Language::Rust);
    let mut depth = 0_i32;
    for token in &tokens {
        match token.text(text) {
            "{" if depth == 0 => return Some(token.start),
            "{" => depth += 1,
            "}" => depth -= 1,
            _ => {}
        }
    }
    None
}

fn visibility(signature: &str) -> Option<(usize, usize, String)> {
    let tokens = tokenize(signature, Language::Rust);
    let significant = tokens
        .iter()
        .filter(|token| !token.is_trivia())
        .collect::<Vec<_>>();
    let fn_at = significant
        .iter()
        .position(|token| token.text(signature) == "fn")?;
    let mut index = fn_at;
    while index > 0 {
        let previous = significant[index - 1].text(signature);
        if matches!(previous, "const" | "async" | "unsafe" | "extern") {
            index -= 1;
            continue;
        }
        if index >= 2
            && significant[index - 1].kind == TokenKind::String
            && significant[index - 2].text(signature) == "extern"
        {
            index -= 2;
            continue;
        }
        break;
    }
    if index > 0 && significant[index - 1].text(signature) == "pub" {
        return None;
    }
    if index > 0 && significant[index - 1].text(signature) == ")" {
        let replaced = restricted(&significant, index, signature);
        if replaced.is_some() {
            return replaced;
        }
    }
    let anchor = significant.get(index)?;
    Some((
        anchor.start,
        anchor.end,
        format!("pub(crate) {}", anchor.text(signature)),
    ))
}

fn restricted(
    tokens: &[&weavatrix_parse::Token],
    index: usize,
    signature: &str,
) -> Option<(usize, usize, String)> {
    let close = index.checked_sub(1)?;
    let mut open = close;
    while open > 0 && tokens[open].text(signature) != "(" {
        open -= 1;
    }
    if open == 0 || tokens[open.checked_sub(1)?].text(signature) != "pub" {
        return None;
    }
    let inside = signature.get(tokens[open].end..tokens[close].start)?.trim();
    if inside == "crate" {
        return None;
    }
    Some((
        tokens[open - 1].start,
        tokens[close].end,
        "pub(crate)".to_owned(),
    ))
}

fn family_parses(state: &RepositoryState, edits: &BTreeMap<String, Vec<ByteEdit>>) -> bool {
    for (file, file_edits) in edits {
        let Some(source) = read_source(state.root(), file) else {
            return false;
        };
        let Some(rewritten) = edits::rewrite(&source, file_edits) else {
            return false;
        };
        if !edits::parses(file, &rewritten) {
            return false;
        }
    }
    true
}

fn accepting(
    state: &RepositoryState,
    copies: &[Copy],
    families: Vec<Family>,
    kept: &mut Vec<Value>,
) -> Vec<Family> {
    let mut accepted: Vec<Family> = Vec::new();
    for family in families {
        let mut trial = accepted.clone();
        trial.push(family);
        if combined_parses(state, &trial) {
            accepted = trial;
        } else if let Some(last) = trial.last() {
            kept.push(row(&copies[last.keeper], "the edit does not parse"));
        }
    }
    accepted
}

fn combined_parses(state: &RepositoryState, families: &[Family]) -> bool {
    let mut by_file = BTreeMap::<String, Vec<ByteEdit>>::new();
    for family in families {
        for (file, edits) in &family.edits {
            by_file
                .entry(file.clone())
                .or_default()
                .extend(edits.iter().cloned());
        }
    }
    family_parses(state, &by_file)
}

fn plan_for(state: &RepositoryState, families: &[Family]) -> Result<Value, Value> {
    let mut by_file = BTreeMap::<String, Vec<ByteEdit>>::new();
    for family in families {
        for (file, edits) in &family.edits {
            by_file
                .entry(file.clone())
                .or_default()
                .extend(edits.iter().cloned());
        }
    }
    let mut builder = PlanBuilder::new("extract_duplicate");
    for (file, edits) in by_file {
        let Some(source) = read_source(state.root(), &file) else {
            return Err(super::stale_graph(&file));
        };
        builder = edits::push_edits(builder, &file, &source, &edits)?;
    }
    Ok(builder.build())
}

fn method_hints(copies: &[Copy], kept: &mut Vec<Value>) {
    let mut grouped = BTreeMap::<&str, Vec<usize>>::new();
    for (index, copy) in copies.iter().enumerate() {
        if copy.kind == "method" {
            grouped.entry(copy.text.as_str()).or_default().push(index);
        }
    }
    for members in grouped.values() {
        if members.len() < 2 || kept.len() >= 40 {
            continue;
        }
        kept.push(row(
            &copies[members[0]],
            "identical methods stay on their types; use share_method_body",
        ));
    }
}

fn sort_by_place(copies: &[Copy], indexes: &mut [usize]) {
    indexes.sort_by(|&left, &right| {
        (&copies[left].file, copies[left].line, &copies[left].id).cmp(&(
            &copies[right].file,
            copies[right].line,
            &copies[right].id,
        ))
    });
}

fn row(copy: &Copy, reason: &str) -> Value {
    json!({
        "id": copy.id,
        "symbol": copy.label,
        "kind": copy.kind,
        "file": copy.file,
        "line": copy.line,
        "reason": reason,
    })
}

#[cfg(test)]
mod tests {
    use super::extract_duplicate;
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    #[test]
    fn a_repository_without_used_copies_plans_nothing() {
        let state = fixture_state();
        let answer = extract_duplicate(&state, &json!({}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("NO_EDITS"),
            "{answer:?}"
        );
    }

    #[test]
    fn a_non_string_symbol_is_invalid_args() {
        let state = fixture_state();
        let answer = extract_duplicate(&state, &json!({"symbol": 1}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("INVALID_ARGS")
        );
    }
}
