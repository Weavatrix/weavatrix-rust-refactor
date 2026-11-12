//! Cascading deletion plan.
//!
//! `exclusive` deletes the selected symbol and the callees that nothing outside the closure
//! uses. A caller outside that set blocks the plan, so another endpoint or contract stays.
//! `connected` also deletes the selection's direct referrers and the callees exclusive to that
//! wider set. Callers of those referrers are reported and are not walked further.
//!
//! An endpoint pulls in the handler it alone serves. A handler another function still calls stays
//! in `exclusive`, and only the selected route is planned; `connected` takes that handler and its
//! direct callers too. A function deletion also removes the one route that passes it as a handler.
//! Two such routes block `exclusive`. A repository is refused. A file nothing else uses is
//! deleted as a file; exclusive mode refuses a file another file still imports. Connected mode
//! deletes that file and the relative import statements that name it. A named import of a
//! deleted symbol is removed in the same plan. A symbol with no source span is `NOT_A_SYMBOL`.
//! The operation returns an edit plan. It does not write.

use crate::coordinates::{slice_between, utf16_offset};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_graph::{Graph, NodeIndex, SourcePosition, SourceSpan};
use weavatrix_rust::{Edge, EdgeKind, NodeKind, RepositoryState};

const MAX_CLOSURE: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Exclusive,
    Connected,
}

struct RouteRegion {
    file: String,
    start: u32,
    end: u32,
}

struct Outsider {
    source: NodeIndex,
    relation: String,
    file: Option<String>,
    line: Option<u32>,
}

struct Verdict {
    blocking: Vec<Outsider>,
    absorbable: Vec<SourceSpan>,
    unresolved: bool,
}

struct Closure {
    order: Vec<NodeIndex>,
    set: BTreeSet<NodeIndex>,
    roles: BTreeMap<NodeIndex, &'static str>,
    sites: Vec<SourceSpan>,
}

impl Closure {
    fn seed(index: NodeIndex) -> Self {
        let mut closure = Self {
            order: Vec::new(),
            set: BTreeSet::new(),
            roles: BTreeMap::new(),
            sites: Vec::new(),
        };
        closure.insert(index, "seed");
        closure
    }

    fn insert(&mut self, index: NodeIndex, role: &'static str) -> bool {
        if self.set.insert(index) {
            self.order.push(index);
            self.roles.insert(index, role);
            true
        } else {
            false
        }
    }
}

struct Walk<'a> {
    graph: &'a Graph,
    region: Option<RouteRegion>,
    seed: NodeIndex,
    handlers: Vec<NodeIndex>,
}

pub(super) fn delete_closure(state: &RepositoryState, arguments: &Value) -> Value {
    let Some(symbol) = arguments.get("symbol").and_then(Value::as_str) else {
        return super::invalid_args("delete_closure", &["symbol"]);
    };
    let scope = match arguments.get("scope").and_then(Value::as_str) {
        None | Some("exclusive") => Scope::Exclusive,
        Some("connected") => Scope::Connected,
        Some(_) => return super::invalid_args("delete_closure", &["scope"]),
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return super::not_found(state.graph(), symbol);
    };
    let Some(node) = state.graph().node_at(index) else {
        return super::not_found(state.graph(), symbol);
    };
    if node.kind == NodeKind::Repository {
        return blocked(
            state.graph(),
            scope,
            node.label.as_str(),
            "delete_closure will not delete a repository",
            &[],
        );
    }
    if node.kind == NodeKind::File {
        return delete_contained_files(state, node.label.as_str(), scope);
    }
    if node.span.is_none() {
        return json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the selected symbol has no source range, so no deletion was planned",
            "symbol": node.label.as_str(),
            "scope": scope_name(scope),
        });
    }

    let graph = state.graph();
    let region = route_region(graph, index);
    let source = node
        .span
        .as_ref()
        .and_then(|span| crate::evidence::read_source(state.root(), &span.file));
    let handlers = handler_candidates(graph, index, region.as_ref(), source.as_deref());
    let walk = Walk {
        graph,
        region,
        seed: index,
        handlers,
    };
    let mut closure = Closure::seed(index);
    let seed_uses = classify(&walk, index, &closure.set);
    if seed_uses.unresolved {
        return blocked(
            graph,
            scope,
            node.label.as_str(),
            "a use of this symbol has no source in the graph; exclusive mode will not guess",
            &[],
        );
    }
    if scope == Scope::Exclusive && !seed_uses.blocking.is_empty() {
        return blocked(
            graph,
            scope,
            node.label.as_str(),
            "another symbol still uses this selection; exclusive mode refuses so other contracts stay intact",
            &seed_uses.blocking,
        );
    }
    closure.sites.extend(seed_uses.absorbable);
    if scope == Scope::Connected {
        for outsider in &seed_uses.blocking {
            if closure.set.len() >= MAX_CLOSURE {
                return too_large(scope, node.label.as_str());
            }
            closure.insert(outsider.source, "referrer");
        }
    }
    if let Err(refusal) = attach_function_routes(
        graph,
        &mut closure,
        index,
        &node.kind,
        node.label.as_str(),
        source.as_deref(),
        scope,
    ) {
        return refusal;
    }
    let mut kept: BTreeMap<NodeIndex, &'static str> = BTreeMap::new();
    if let Err(refusal) = grow(&walk, &mut closure, &mut kept, scope, node.label.as_str()) {
        return refusal;
    }
    for index in contained_symbols(&walk, &closure) {
        closure.insert(index, "contained");
    }
    finish(state, &walk, &closure, &kept, scope, node.label.as_str())
}

fn finish(
    state: &RepositoryState,
    walk: &Walk<'_>,
    closure: &Closure,
    kept: &BTreeMap<NodeIndex, &'static str>,
    scope: Scope,
    label: &str,
) -> Value {
    let spans = match concrete_spans(state, &deletion_spans(walk, closure)) {
        Ok(spans) => spans,
        Err(refusal) => return refusal,
    };
    if spans.iter().any(|span| damages_kept(walk, span, closure)) {
        return blocked(
            walk.graph,
            scope,
            label,
            "the recorded range also covers a symbol this mode keeps, so nothing was planned",
            &[],
        );
    }
    let affected = if scope == Scope::Connected {
        affected_uses(walk, closure)
    } else {
        Vec::new()
    };
    let extra =
        super::delete_imports::statement_spans(state, &[], &removed_symbols(walk, closure), &spans);
    let mut spans = spans;
    spans.extend(extra);
    spans.sort();
    spans.dedup();
    let spans = without_nested(&spans);
    match plan_for(state, &spans) {
        Ok(plan) if plan_files(&plan) == 0 => json!({
            "status": "NO_CHANGE",
            "reason": "the closure produced no edit",
            "scope": scope_name(scope),
            "symbol": label,
        }),
        Ok(plan) => json!({
            "status": "PLANNED",
            "scope": scope_name(scope),
            "completeness": "PARTIAL",
            "symbol": label,
            "deleted": member_rows(walk, closure),
            "kept": kept_rows(walk, kept),
            "affected": outsider_rows(walk, &affected),
            "plan": plan,
            "warnings": ["GRAPH_PROVEN_USES_ONLY"],
            "next": "apply with apply_edit_plan (preview -> confirm). Review kept and affected before confirming.",
        }),
        Err(refusal) => refusal,
    }
}

fn grow(
    walk: &Walk<'_>,
    closure: &mut Closure,
    kept: &mut BTreeMap<NodeIndex, &'static str>,
    scope: Scope,
    label: &str,
) -> Result<(), Value> {
    loop {
        let snapshot = closure.order.clone();
        let mut grew = false;
        for index in snapshot {
            for target in candidates(walk, index) {
                if closure.set.contains(&target) {
                    continue;
                }
                let uses = classify(walk, target, &closure.set);
                if uses.unresolved || !uses.blocking.is_empty() {
                    if scope == Scope::Connected
                        && index == walk.seed
                        && walk.handlers.contains(&target)
                        && !uses.unresolved
                    {
                        attach_connected_handler(closure, target, &uses, scope, label)?;
                        grew = true;
                        continue;
                    }
                    kept.entry(target).or_insert("used outside the closure");
                    continue;
                }
                if closure.set.len() >= MAX_CLOSURE {
                    return Err(too_large(scope, label));
                }
                let role = if walk.handlers.contains(&target) {
                    "handler"
                } else {
                    "callee"
                };
                closure.insert(target, role);
                closure.sites.extend(uses.absorbable);
                grew = true;
            }
        }
        if !grew {
            return Ok(());
        }
    }
}

fn attach_function_routes(
    graph: &Graph,
    closure: &mut Closure,
    index: NodeIndex,
    kind: &NodeKind,
    label: &str,
    source: Option<&str>,
    scope: Scope,
) -> Result<(), Value> {
    if !is_handler_kind(kind) {
        return Ok(());
    }
    let Some(text) = source else {
        return Ok(());
    };
    let routes = lexical_routes(graph, index, text);
    if scope == Scope::Exclusive && routes.len() > 1 {
        return Err(blocked(
            graph,
            scope,
            label,
            "other endpoints still pass this symbol as a handler; exclusive mode leaves them",
            &route_outsiders(graph, &routes),
        ));
    }
    if routes.len() == 1 || scope == Scope::Connected {
        for route in routes {
            if closure.set.len() >= MAX_CLOSURE {
                return Err(too_large(scope, label));
            }
            closure.insert(route, "route");
        }
    }
    Ok(())
}

/// `connected` takes the endpoint's handler even when another function calls it, and that caller
/// comes along one hop. Callers of the caller stay out of the closure.
fn attach_connected_handler(
    closure: &mut Closure,
    handler: NodeIndex,
    uses: &Verdict,
    scope: Scope,
    label: &str,
) -> Result<(), Value> {
    if closure.set.len() >= MAX_CLOSURE {
        return Err(too_large(scope, label));
    }
    closure.insert(handler, "handler");
    for outsider in &uses.blocking {
        if closure.set.len() >= MAX_CLOSURE {
            return Err(too_large(scope, label));
        }
        closure.insert(outsider.source, "referrer");
    }
    closure.sites.extend(uses.absorbable.iter().cloned());
    Ok(())
}

fn candidates(walk: &Walk<'_>, index: NodeIndex) -> Vec<NodeIndex> {
    let mut found = BTreeSet::new();
    for edge in walk.graph.outgoing_at(index) {
        if let Some(target) = use_target(walk.graph, edge) {
            found.insert(target);
        }
    }
    if index == walk.seed {
        found.extend(walk.handlers.iter().copied());
    }
    found
        .into_iter()
        .filter(|target| *target != index)
        .collect()
}

fn classify(walk: &Walk<'_>, target: NodeIndex, selected: &BTreeSet<NodeIndex>) -> Verdict {
    let mut verdict = Verdict {
        blocking: Vec::new(),
        absorbable: Vec::new(),
        unresolved: false,
    };
    let mut seen = BTreeSet::new();
    for edge in walk.graph.incoming_at(target) {
        if !is_use(&edge.kind) {
            continue;
        }
        let Some(source) = walk.graph.node_index(edge.source.as_str()) else {
            verdict.unresolved = true;
            continue;
        };
        if source == target || selected.contains(&source) {
            continue;
        }
        let Some(node) = walk.graph.node_at(source) else {
            verdict.unresolved = true;
            continue;
        };
        if is_container(&node.kind) || encloses(walk.graph, source, target) {
            continue;
        }
        if let Some(span) = absorbable_span(walk, edge, target) {
            verdict.absorbable.push(span);
            continue;
        }
        if seen.insert(source) {
            verdict.blocking.push(outsider(walk.graph, edge, source));
        }
    }
    verdict
}

fn absorbable_span(walk: &Walk<'_>, edge: &Edge, target: NodeIndex) -> Option<SourceSpan> {
    let region = walk.region.as_ref()?;
    let span = edge.provenance.span.clone()?;
    if span.file != region.file || span.start.line < region.start || span.start.line >= region.end {
        return None;
    }
    if covers_other_endpoint(walk, &span) || covers_other_function(walk.graph, &span, target) {
        return None;
    }
    Some(span)
}

fn covers_other_endpoint(walk: &Walk<'_>, span: &SourceSpan) -> bool {
    walk.graph.nodes().iter().any(|node| {
        node.kind == NodeKind::Endpoint
            && walk.graph.node_index(node.id.as_str()) != Some(walk.seed)
            && node
                .span
                .as_ref()
                .is_some_and(|other| spans_overlap(span, other))
    })
}

fn covers_other_function(graph: &Graph, span: &SourceSpan, target: NodeIndex) -> bool {
    graph.nodes().iter().any(|node| {
        if !is_handler_kind(&node.kind) {
            return false;
        }
        let Some(index) = graph.node_index(node.id.as_str()) else {
            return false;
        };
        index != target
            && node
                .span
                .as_ref()
                .is_some_and(|other| span_contains(span, other))
    })
}

fn handler_candidates(
    graph: &Graph,
    endpoint: NodeIndex,
    region: Option<&RouteRegion>,
    source: Option<&str>,
) -> Vec<NodeIndex> {
    let Some(region) = region else {
        return Vec::new();
    };
    let mut found = BTreeSet::new();
    if let Some(dedicated) = dedicated_handler(graph, endpoint) {
        found.insert(dedicated);
    }
    if let Some(text) = source {
        found.extend(lexical_handlers(graph, endpoint, text));
    }
    for edge in graph.edges() {
        if !is_use(&edge.kind) {
            continue;
        }
        let Some(span) = edge.provenance.span.as_ref() else {
            continue;
        };
        if span.file != region.file
            || span.start.line < region.start
            || span.start.line >= region.end
        {
            continue;
        }
        let Some(target) = use_target(graph, edge) else {
            continue;
        };
        if target == endpoint || encloses(graph, target, endpoint) {
            continue;
        }
        let Some(node) = graph.node_at(target) else {
            continue;
        };
        if is_handler_kind(&node.kind) {
            found.insert(target);
        }
    }
    found.into_iter().collect()
}

/// Route arguments are often not call edges. A function named on this route's line, and not on
/// another route's line, is the handler. The registration callee (`get(`, `post(`, ...) is not.
fn lexical_handlers(graph: &Graph, endpoint: NodeIndex, text: &str) -> Vec<NodeIndex> {
    let Some(span) = graph.node_at(endpoint).and_then(|node| node.span.clone()) else {
        return Vec::new();
    };
    let statement = route_statement(text, span.start.line);
    let others = other_route_lines(graph, endpoint, &span.file, text);
    let mut found = BTreeSet::new();
    let mut same_file_names = BTreeSet::new();
    let mut elsewhere: BTreeMap<String, Vec<NodeIndex>> = BTreeMap::new();
    for node in graph.nodes() {
        if !is_handler_kind(&node.kind) {
            continue;
        }
        let Some(body) = node.span.as_ref() else {
            continue;
        };
        let name = node.label.trim_end_matches("()");
        if !is_identifier(name)
            || !handler_mention(&statement, name)
            || route_callee_only(&statement, name)
        {
            continue;
        }
        if others.iter().any(|other| mentioned(other, name)) {
            continue;
        }
        let Some(index) = graph.node_index(node.id.as_str()) else {
            continue;
        };
        if index == endpoint || encloses(graph, index, endpoint) {
            continue;
        }
        if body.file == span.file {
            same_file_names.insert(name.to_owned());
            found.insert(index);
        } else {
            elsewhere.entry(name.to_owned()).or_default().push(index);
        }
    }
    for (name, indexes) in elsewhere {
        if same_file_names.contains(&name) || indexes.len() != 1 {
            continue;
        }
        if let Some(index) = indexes.first() {
            found.insert(*index);
        }
    }
    found.into_iter().collect()
}

fn other_route_lines(graph: &Graph, endpoint: NodeIndex, file: &str, text: &str) -> Vec<String> {
    graph
        .nodes()
        .iter()
        .filter(|node| {
            node.kind == NodeKind::Endpoint && graph.node_index(node.id.as_str()) != Some(endpoint)
        })
        .filter_map(|node| {
            let span = node.span.as_ref()?;
            (span.file == file).then(|| route_statement(text, span.start.line))
        })
        .collect()
}

/// The route call, including the following lines while `(` is still open.
fn route_statement(text: &str, start: u32) -> String {
    let mut lines = Vec::new();
    let mut debt = 0_i32;
    let mut line_no = start;
    for _ in 0..8 {
        let Some(line) = nth_line(text, line_no) else {
            break;
        };
        lines.push(line.to_owned());
        debt += paren_debt(line);
        line_no = line_no.saturating_add(1);
        if debt <= 0 {
            break;
        }
    }
    lines.join("\n")
}

fn paren_debt(line: &str) -> i32 {
    let mut debt = 0_i32;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '(' => debt += 1,
            ')' => debt -= 1,
            '/' if chars.peek() == Some(&'/') => break,
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                while let Some(current) = chars.next() {
                    if current == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            '"' | '\'' | '`' => skip_quoted(&mut chars, character),
            _ => {}
        }
    }
    debt
}

fn nth_line(text: &str, line: u32) -> Option<&str> {
    let index = usize::try_from(line.saturating_sub(1)).ok()?;
    text.split('\n')
        .nth(index)
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
}

fn mentioned(line: &str, name: &str) -> bool {
    word_hits(line, name).next().is_some()
}

/// Endpoints whose line passes `function` as a handler, not merely as a quoted path segment.
fn lexical_routes(graph: &Graph, function: NodeIndex, text: &str) -> Vec<NodeIndex> {
    let Some(node) = graph.node_at(function) else {
        return Vec::new();
    };
    if !is_handler_kind(&node.kind) {
        return Vec::new();
    }
    let name = node.label.trim_end_matches("()");
    if !is_identifier(name) {
        return Vec::new();
    }
    let Some(body) = node.span.as_ref() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for endpoint in graph.nodes() {
        if endpoint.kind != NodeKind::Endpoint {
            continue;
        }
        let Some(span) = endpoint.span.as_ref() else {
            continue;
        };
        if span.file != body.file {
            continue;
        }
        let statement = route_statement(text, span.start.line);
        if !handler_mention(&statement, name) {
            continue;
        }
        let Some(index) = graph.node_index(endpoint.id.as_str()) else {
            continue;
        };
        found.push(index);
    }
    found.sort();
    found.dedup();
    found
}

fn route_outsiders(graph: &Graph, routes: &[NodeIndex]) -> Vec<Outsider> {
    routes
        .iter()
        .filter_map(|index| {
            let node = graph.node_at(*index)?;
            let span = node.span.as_ref();
            Some(Outsider {
                source: *index,
                relation: "exposes".to_owned(),
                file: span.map(|item| item.file.clone()),
                line: span.map(|item| item.start.line),
            })
        })
        .collect()
}

/// A handler argument, as opposed to the same word inside the route string.
fn handler_mention(line: &str, name: &str) -> bool {
    word_hits(line, name).any(|start| !quoted(line, start) && !call_shaped(line, start, name))
}

fn quoted(line: &str, at: usize) -> bool {
    let mut single = false;
    let mut double = false;
    let mut template = false;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        if index >= at {
            break;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && (single || double || template) {
            escaped = true;
            continue;
        }
        match character {
            '\'' if !double && !template => single = !single,
            '"' if !single && !template => double = !double,
            '`' if !single && !double => template = !template,
            _ => {}
        }
    }
    single || double || template
}

fn route_callee_only(line: &str, name: &str) -> bool {
    const VERBS: &[&str] = &[
        "get",
        "post",
        "put",
        "patch",
        "delete",
        "head",
        "options",
        "all",
        "use",
        "route",
        "handle",
        "handlefunc",
    ];
    if !VERBS.iter().any(|verb| verb.eq_ignore_ascii_case(name)) {
        return false;
    }
    word_hits(line, name).all(|start| call_shaped(line, start, name))
}

fn word_hits<'line>(line: &'line str, name: &'line str) -> impl Iterator<Item = usize> + 'line {
    let mut from = 0_usize;
    std::iter::from_fn(move || {
        while let Some(found) = line[from..].find(name) {
            let start = from + found;
            let end = start + name.len();
            from = start + 1;
            let before = line[..start].chars().next_back();
            let after = line[end..].chars().next();
            let boundary = |character: Option<char>| {
                character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
            };
            if boundary(before) && boundary(after) {
                return Some(start);
            }
        }
        None
    })
}

fn call_shaped(line: &str, start: usize, name: &str) -> bool {
    line[start + name.len()..].trim_start().starts_with('(')
}

fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
}

fn dedicated_handler(graph: &Graph, endpoint: NodeIndex) -> Option<NodeIndex> {
    let inner = graph.node_at(endpoint)?.span.clone()?;
    let mut best: Option<NodeIndex> = None;
    for node in graph.nodes() {
        if !is_handler_kind(&node.kind) {
            continue;
        }
        let Some(body) = node.span.clone() else {
            continue;
        };
        if !span_contains(&body, &inner) {
            continue;
        }
        let Some(index) = graph.node_index(node.id.as_str()) else {
            continue;
        };
        let replace = match best {
            None => true,
            Some(current) => graph
                .node_at(current)
                .and_then(|node| node.span.clone())
                .is_some_and(|current_span| span_contains(&current_span, &body)),
        };
        if replace {
            best = Some(index);
        }
    }
    let index = best?;
    let body = graph.node_at(index)?.span.clone()?;
    let shares = graph.nodes().iter().any(|node| {
        node.kind == NodeKind::Endpoint
            && graph.node_index(node.id.as_str()) != Some(endpoint)
            && node
                .span
                .as_ref()
                .is_some_and(|span| span_contains(&body, span))
    });
    (!shares).then_some(index)
}

fn route_region(graph: &Graph, index: NodeIndex) -> Option<RouteRegion> {
    let node = graph.node_at(index)?;
    if node.kind != NodeKind::Endpoint {
        return None;
    }
    let span = node.span.clone()?;
    let end = container_end(graph, &span);
    Some(RouteRegion {
        file: span.file.clone(),
        start: span.start.line,
        end,
    })
}

fn container_end(graph: &Graph, endpoint: &SourceSpan) -> u32 {
    let Some(container) = tightest_span(graph, endpoint) else {
        return exclusive_end_line(endpoint);
    };
    graph
        .nodes()
        .iter()
        .filter(|node| node.kind == NodeKind::Endpoint)
        .filter_map(|node| node.span.as_ref())
        .filter(|span| {
            span.file == endpoint.file
                && span.start > endpoint.start
                && span_contains(&container, span)
        })
        .map(|span| span.start.line)
        .min()
        .unwrap_or_else(|| exclusive_end_line(&container))
}

fn tightest_span(graph: &Graph, inner: &SourceSpan) -> Option<SourceSpan> {
    let mut best: Option<SourceSpan> = None;
    for node in graph.nodes() {
        if !is_handler_kind(&node.kind) {
            continue;
        }
        let Some(body) = node.span.clone() else {
            continue;
        };
        if !span_contains(&body, inner) {
            continue;
        }
        let replace = match &best {
            None => true,
            Some(current) => span_contains(current, &body),
        };
        if replace {
            best = Some(body);
        }
    }
    best
}

fn exclusive_end_line(span: &SourceSpan) -> u32 {
    if span.end.line > span.start.line && span.end.column <= 1 {
        span.end.line
    } else {
        span.end.line.saturating_add(1)
    }
}

fn contained_symbols(walk: &Walk<'_>, closure: &Closure) -> Vec<NodeIndex> {
    let mut found = Vec::new();
    for node in walk.graph.nodes() {
        if is_container(&node.kind) {
            continue;
        }
        let Some(index) = walk.graph.node_index(node.id.as_str()) else {
            continue;
        };
        if closure.set.contains(&index) {
            continue;
        }
        let Some(span) = node.span.as_ref() else {
            continue;
        };
        if closure.order.iter().any(|selected| {
            walk.graph
                .node_at(*selected)
                .and_then(|node| node.span.as_ref())
                .is_some_and(|outer| span_contains(outer, span))
        }) {
            found.push(index);
        }
    }
    found
}

fn deletion_spans(walk: &Walk<'_>, closure: &Closure) -> Vec<SourceSpan> {
    let mut spans = Vec::new();
    for index in &closure.order {
        if let Some(span) = walk
            .graph
            .node_at(*index)
            .and_then(|node| node.span.clone())
        {
            spans.push(span);
        }
    }
    spans.extend(closure.sites.iter().cloned());
    spans.sort();
    spans.dedup();
    without_nested(&spans)
}

fn without_nested(spans: &[SourceSpan]) -> Vec<SourceSpan> {
    spans
        .iter()
        .enumerate()
        .filter(|(index, span)| {
            !spans
                .iter()
                .enumerate()
                .any(|(other, candidate)| other != *index && span_contains(candidate, span))
        })
        .map(|(_, span)| span.clone())
        .collect()
}

fn damages_kept(walk: &Walk<'_>, span: &SourceSpan, closure: &Closure) -> bool {
    walk.graph.nodes().iter().any(|node| {
        if is_container(&node.kind) {
            return false;
        }
        let Some(index) = walk.graph.node_index(node.id.as_str()) else {
            return false;
        };
        if closure.set.contains(&index) {
            return false;
        }
        let Some(other) = node.span.as_ref() else {
            return false;
        };
        if closure.order.iter().any(|selected| {
            walk.graph
                .node_at(*selected)
                .and_then(|node| node.span.as_ref())
                .is_some_and(|outer| span_contains(outer, other))
        }) {
            return false;
        }
        spans_overlap(span, other) && !span_contains(other, span)
    })
}

fn affected_uses(walk: &Walk<'_>, closure: &Closure) -> Vec<Outsider> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for index in &closure.order {
        for outsider in classify(walk, *index, &closure.set).blocking {
            if seen.insert(outsider.source) {
                rows.push(outsider);
            }
        }
    }
    rows
}

struct DraftEdit {
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
    before: String,
}

/// A point span is the line the extractor marked and not a range. The route text is that line.
///
/// A function span often ends on the line of its closing brace without including that brace.
/// Extending across brace-only lines keeps the deletion from leaving a stray `}`.
fn concrete_spans(state: &RepositoryState, spans: &[SourceSpan]) -> Result<Vec<SourceSpan>, Value> {
    let mut concrete = Vec::with_capacity(spans.len());
    for span in spans {
        let Some(text) = crate::evidence::read_source(state.root(), &span.file) else {
            return Err(json!({
                "status": "SOURCE_UNAVAILABLE",
                "reason": format!("{} could not be read, so no deletion was planned", span.file),
            }));
        };
        let widened = if span.start == span.end {
            let Some(line) = line_span(span, &text) else {
                return Err(super::stale_graph(&span.file));
            };
            line
        } else {
            span.clone()
        };
        concrete.push(cover_closers(&widened, &text));
    }
    Ok(without_nested(&concrete))
}

fn cover_closers(span: &SourceSpan, text: &str) -> SourceSpan {
    let Some(before) = slice_between(
        text,
        (span.start.line, span.start.column),
        (span.end.line, span.end.column),
    ) else {
        return span.clone();
    };
    let mut debt = brace_debt(&before);
    if debt <= 0 {
        return span.clone();
    }
    let mut line_no = span.end.line;
    let mut column = span.end.column;
    let mut advanced = false;
    while debt > 0 {
        let Some(line) = nth_line(text, line_no) else {
            break;
        };
        let Some(rest) = byte_tail(line, column) else {
            break;
        };
        let Some(closers) = closer_count(rest) else {
            break;
        };
        debt -= closers;
        advanced = true;
        if nth_line(text, line_no.saturating_add(1)).is_some() {
            line_no = line_no.saturating_add(1);
            column = 1;
        } else {
            column = u32::try_from(line.len())
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            break;
        }
    }
    if !advanced {
        return span.clone();
    }
    SourceSpan::new(
        span.file.clone(),
        span.start,
        SourcePosition::new(line_no, column),
    )
}

fn brace_debt(text: &str) -> i32 {
    let mut debt = 0_i32;
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' => debt += 1,
            '}' => debt -= 1,
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                while let Some(current) = chars.next() {
                    if current == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        break;
                    }
                }
            }
            '"' | '\'' | '`' => skip_quoted(&mut chars, character),
            _ => {}
        }
    }
    debt
}

fn skip_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, quote: char) {
    let mut escaped = false;
    for character in chars.by_ref() {
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

/// A closer line may carry a trailing `//` or `/* */` comment. `#` stays, so a Rust attribute
/// is not treated as a comment.
fn closer_body(rest: &str) -> String {
    let mut body = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '/' && chars.peek() == Some(&'/') {
            break;
        }
        if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(current) = chars.next() {
                if current == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        body.push(character);
    }
    body
}

fn byte_tail(line: &str, column: u32) -> Option<&str> {
    let offset = usize::try_from(column.saturating_sub(1)).ok()?;
    if offset > line.len() || !line.is_char_boundary(offset) {
        return None;
    }
    Some(&line[offset..])
}

fn closer_count(rest: &str) -> Option<i32> {
    let body = closer_body(rest);
    let trimmed = body.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|character| character == '}' || character == ';')
    {
        return None;
    }
    let count = trimmed
        .chars()
        .filter(|character| *character == '}')
        .count();
    i32::try_from(count).ok().filter(|count| *count > 0)
}

fn line_span(span: &SourceSpan, text: &str) -> Option<SourceSpan> {
    let index = usize::try_from(span.start.line.saturating_sub(1)).ok()?;
    let mut lines = text.split('\n');
    let line = lines.nth(index)?;
    let body = line.strip_suffix('\r').unwrap_or(line);
    if body.trim().is_empty() {
        return None;
    }
    let end = if lines.next().is_some() {
        SourcePosition::new(span.start.line.saturating_add(1), 1)
    } else {
        SourcePosition::new(
            span.start.line,
            u32::try_from(body.len()).ok()?.saturating_add(1),
        )
    };
    Some(SourceSpan::new(
        span.file.clone(),
        SourcePosition::new(span.start.line, 1),
        end,
    ))
}

fn removed_symbols(
    walk: &Walk<'_>,
    closure: &Closure,
) -> Vec<super::delete_imports::RemovedSymbol> {
    closure
        .order
        .iter()
        .filter_map(|index| {
            let node = walk.graph.node_at(*index)?;
            if is_container(&node.kind) {
                return None;
            }
            let file = node.span.as_ref()?.file.clone();
            let name = node.label.trim_end_matches("()").to_owned();
            (!name.is_empty()).then_some(super::delete_imports::RemovedSymbol { name, file })
        })
        .collect()
}

fn plan_for(state: &RepositoryState, spans: &[SourceSpan]) -> Result<Value, Value> {
    Ok(span_edits(PlanBuilder::new("delete_closure"), state, spans)?.build())
}

fn span_edits(
    mut builder: PlanBuilder,
    state: &RepositoryState,
    spans: &[SourceSpan],
) -> Result<PlanBuilder, Value> {
    let mut texts: BTreeMap<String, String> = BTreeMap::new();
    for span in spans {
        if texts.contains_key(span.file.as_str()) {
            continue;
        }
        let Some(text) = crate::evidence::read_source(state.root(), &span.file) else {
            return Err(json!({
                "status": "SOURCE_UNAVAILABLE",
                "reason": format!("{} could not be read, so no deletion was planned", span.file),
            }));
        };
        texts.insert(span.file.clone(), text);
    }
    let mut grouped: BTreeMap<String, Vec<DraftEdit>> = BTreeMap::new();
    for span in spans {
        let Some(text) = texts.get(span.file.as_str()) else {
            return Err(super::stale_graph(&span.file));
        };
        let Some(before) = slice_between(
            text,
            (span.start.line, span.start.column),
            (span.end.line, span.end.column),
        ) else {
            return Err(super::stale_graph(&span.file));
        };
        if before.is_empty() {
            return Err(super::stale_graph(&span.file));
        }
        let (Ok(start_char), Ok(end_char)) = (
            utf16_offset(text, span.start.line, span.start.column),
            utf16_offset(text, span.end.line, span.end.column),
        ) else {
            return Err(super::stale_graph(&span.file));
        };
        grouped
            .entry(span.file.clone())
            .or_default()
            .push(DraftEdit {
                start_line: span.start.line,
                start_char,
                end_line: span.end.line,
                end_char,
                before,
            });
    }
    for (path, edits) in grouped {
        let Some(text) = texts.get(&path) else {
            return Err(super::stale_graph(&path));
        };
        builder = builder.file(&path, &sha256_of(text));
        for edit in edits {
            builder = builder.edit(
                edit.start_line,
                edit.start_char,
                edit.end_line,
                edit.end_char,
                edit.before,
                "",
                "RESOLVED",
            );
        }
    }
    Ok(builder)
}

fn plan_files(plan: &Value) -> usize {
    plan.get("files")
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn member_rows(walk: &Walk<'_>, closure: &Closure) -> Vec<Value> {
    closure
        .order
        .iter()
        .filter_map(|index| {
            let node = walk.graph.node_at(*index)?;
            let span = node.span.as_ref()?;
            Some(json!({
                "id": node.id.as_str(),
                "symbol": node.label.as_str(),
                "kind": node.kind.as_str(),
                "role": closure.roles.get(index).copied().unwrap_or("callee"),
                "file": span.file.as_str(),
                "line": span.start.line,
            }))
        })
        .collect()
}

fn kept_rows(walk: &Walk<'_>, kept: &BTreeMap<NodeIndex, &'static str>) -> Vec<Value> {
    kept.iter()
        .filter_map(|(index, reason)| {
            let node = walk.graph.node_at(*index)?;
            Some(json!({
                "id": node.id.as_str(),
                "symbol": node.label.as_str(),
                "kind": node.kind.as_str(),
                "reason": reason,
                "file": node.span.as_ref().map(|span| span.file.as_str()),
                "line": node.span.as_ref().map(|span| span.start.line),
            }))
        })
        .collect()
}

fn outsider_rows(walk: &Walk<'_>, outsiders: &[Outsider]) -> Vec<Value> {
    outsiders
        .iter()
        .filter_map(|outsider| {
            let node = walk.graph.node_at(outsider.source)?;
            Some(json!({
                "id": node.id.as_str(),
                "symbol": node.label.as_str(),
                "kind": node.kind.as_str(),
                "relation": outsider.relation.as_str(),
                "file": outsider.file.as_deref(),
                "line": outsider.line,
            }))
        })
        .collect()
}

fn outsider(graph: &Graph, edge: &Edge, source: NodeIndex) -> Outsider {
    let span = edge
        .provenance
        .span
        .as_ref()
        .or_else(|| graph.node_at(source).and_then(|node| node.span.as_ref()));
    Outsider {
        source,
        relation: edge.kind.as_str().to_owned(),
        file: span.map(|item| item.file.clone()),
        line: span.map(|item| item.start.line),
    }
}

fn delete_contained_files(state: &RepositoryState, label: &str, scope: Scope) -> Value {
    let graph = state.graph();
    let paths = vec![label.to_owned()];
    let inside = indexes_in_files(graph, &paths);
    let outsiders = external_uses(graph, &inside);
    if scope == Scope::Exclusive && !outsiders.is_empty() {
        return blocked(
            graph,
            scope,
            label,
            "another symbol still uses this file; exclusive mode refuses so other contracts stay intact",
            &outsiders,
        );
    }
    let mut builder = PlanBuilder::new("delete_closure");
    for path in &paths {
        let Some(text) = crate::evidence::read_source(state.root(), path) else {
            return json!({
                "status": "SOURCE_UNAVAILABLE",
                "reason": format!("{path} could not be read, so no deletion was planned"),
            });
        };
        builder = builder.delete_path(path, &sha256_of(&text));
    }
    let imports = super::delete_imports::statement_spans(state, &paths, &[], &[]);
    builder = match span_edits(builder, state, &imports) {
        Ok(builder) => builder,
        Err(refusal) => return refusal,
    };
    let affected = if scope == Scope::Connected {
        outsider_values(graph, &outsiders)
    } else {
        Vec::new()
    };
    json!({
        "status": "PLANNED",
        "scope": scope_name(scope),
        "completeness": "PARTIAL",
        "symbol": label,
        "deleted": file_rows(graph, &inside),
        "kept": [],
        "affected": affected,
        "plan": builder.build(),
        "warnings": ["GRAPH_PROVEN_USES_ONLY"],
        "next": "apply with apply_edit_plan (preview -> confirm). Review kept and affected before confirming.",
    })
}

fn indexes_in_files(graph: &Graph, paths: &[String]) -> BTreeSet<NodeIndex> {
    let mut inside = BTreeSet::new();
    for node in graph.nodes() {
        let listed = (node.kind == NodeKind::File && paths.iter().any(|path| path == &node.label))
            || node
                .span
                .as_ref()
                .is_some_and(|span| paths.iter().any(|path| path == &span.file));
        if !listed {
            continue;
        }
        if let Some(index) = graph.node_index(node.id.as_str()) {
            inside.insert(index);
        }
    }
    inside
}

fn external_uses(graph: &Graph, inside: &BTreeSet<NodeIndex>) -> Vec<Outsider> {
    let mut found = Vec::new();
    for edge in graph.edges() {
        if !is_external_use(&edge.kind) {
            continue;
        }
        let Some(target) = graph.node_index(edge.target.as_str()) else {
            continue;
        };
        if !inside.contains(&target) {
            continue;
        }
        let Some(source) = graph.node_index(edge.source.as_str()) else {
            continue;
        };
        if inside.contains(&source) {
            continue;
        }
        found.push(outsider(graph, edge, source));
    }
    found
}

fn file_rows(graph: &Graph, inside: &BTreeSet<NodeIndex>) -> Vec<Value> {
    inside
        .iter()
        .filter_map(|index| {
            let node = graph.node_at(*index)?;
            Some(json!({
                "id": node.id.as_str(),
                "symbol": node.label.as_str(),
                "kind": node.kind.as_str(),
                "role": if node.kind == NodeKind::File { "file" } else { "contained" },
                "file": node.span.as_ref().map_or(node.label.as_str(), |span| span.file.as_str()),
                "line": node.span.as_ref().map(|span| span.start.line),
            }))
        })
        .collect()
}

fn outsider_values(graph: &Graph, outsiders: &[Outsider]) -> Vec<Value> {
    outsiders
        .iter()
        .filter_map(|item| {
            let node = graph.node_at(item.source)?;
            Some(json!({
                "id": node.id.as_str(),
                "symbol": node.label.as_str(),
                "kind": node.kind.as_str(),
                "relation": item.relation.as_str(),
                "file": item.file.as_deref(),
                "line": item.line,
            }))
        })
        .collect()
}

const fn is_external_use(kind: &EdgeKind) -> bool {
    is_use(kind) || matches!(kind, EdgeKind::Imports | EdgeKind::ReExports)
}

fn blocked(
    graph: &Graph,
    scope: Scope,
    symbol: &str,
    reason: &str,
    outsiders: &[Outsider],
) -> Value {
    json!({
        "status": "BLOCKED",
        "scope": scope_name(scope),
        "symbol": symbol,
        "reason": reason,
        "outsiders": outsiders.iter().filter_map(|outsider| {
            let node = graph.node_at(outsider.source)?;
            Some(json!({
                "id": node.id.as_str(),
                "symbol": node.label.as_str(),
                "kind": node.kind.as_str(),
                "relation": outsider.relation.as_str(),
                "file": outsider.file.as_deref(),
                "line": outsider.line,
            }))
        }).collect::<Vec<_>>(),
    })
}

fn too_large(scope: Scope, symbol: &str) -> Value {
    json!({
        "status": "BLOCKED",
        "scope": scope_name(scope),
        "symbol": symbol,
        "reason": "the closure exceeds 200 symbols; narrow the selection. Nothing was planned.",
        "outsiders": [],
    })
}

fn use_target(graph: &Graph, edge: &Edge) -> Option<NodeIndex> {
    if !is_use(&edge.kind) {
        return None;
    }
    let index = graph.node_index(edge.target.as_str())?;
    let node = graph.node_at(index)?;
    (!is_container(&node.kind)).then_some(index)
}

fn encloses(graph: &Graph, outer: NodeIndex, inner: NodeIndex) -> bool {
    match (
        graph.node_at(outer).and_then(|node| node.span.clone()),
        graph.node_at(inner).and_then(|node| node.span.clone()),
    ) {
        (Some(outer), Some(inner)) => span_contains(&outer, &inner),
        _ => false,
    }
}

fn span_contains(outer: &SourceSpan, inner: &SourceSpan) -> bool {
    outer.file == inner.file
        && outer.start <= inner.start
        && outer.end >= inner.end
        && (outer.start != inner.start || outer.end != inner.end)
}

fn spans_overlap(left: &SourceSpan, right: &SourceSpan) -> bool {
    left.file == right.file && left.start < right.end && right.start < left.end
}

const fn is_use(kind: &EdgeKind) -> bool {
    matches!(
        kind,
        EdgeKind::Calls
            | EdgeKind::References
            | EdgeKind::Exposes
            | EdgeKind::Reads
            | EdgeKind::Writes
    )
}

const fn is_container(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::File | NodeKind::Module | NodeKind::Package | NodeKind::Repository
    )
}

const fn is_handler_kind(kind: &NodeKind) -> bool {
    matches!(kind, NodeKind::Function | NodeKind::Method)
}

const fn scope_name(scope: Scope) -> &'static str {
    match scope {
        Scope::Exclusive => "exclusive",
        Scope::Connected => "connected",
    }
}
