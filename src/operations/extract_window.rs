//! Plan extraction of a repeated straight-line window.
//!
//! The window is a run of statements inside a Rust function, at least four lines, and not the
//! whole function. Two copies must match byte for byte. A copy under a different `cfg`, a copy
//! that returns or reads `self`, and a free name with no written type stay. The plan is not
//! written here.

use super::edits::{self, ByteEdit};
use super::rust_type::render_type;
use crate::evidence::read_source;
use crate::plan::PlanBuilder;
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use syn::spanned::Spanned;
use weavatrix_parse::{Language, tokenize};
use weavatrix_rust::{NodeKind, RepositoryState};

const MAX_WINDOWS: usize = 200;
const NEXT: &str = "apply with apply_edit_plan (preview -> confirm).";

struct Piece {
    start: usize,
    end: usize,
    text: String,
    stmt: syn::Stmt,
}

struct Fun {
    file: String,
    name: String,
    cfg: String,
    source: String,
    signature: syn::Signature,
    stmts: Vec<Piece>,
}

#[derive(Clone)]
struct Site {
    fun: usize,
    from: usize,
    to: usize,
}

struct Facts {
    params: Vec<(String, String)>,
    outputs: Vec<(String, String)>,
}

#[derive(Clone)]
struct Window {
    helper_file: String,
    helper: String,
    edits: BTreeMap<String, Vec<ByteEdit>>,
    sites: usize,
}

pub(super) fn extract_window(state: &RepositoryState, arguments: &Value) -> Value {
    let focus = match focus(state, arguments) {
        Ok(focus) => focus,
        Err(refusal) => return refusal,
    };
    let funs = functions(state);
    let mut kept = Vec::new();
    let mut windows = Vec::new();
    for (text, sites) in runs(&funs) {
        if let Some((file, name)) = focus.as_ref()
            && !sites
                .iter()
                .any(|site| funs[site.fun].file == *file && funs[site.fun].name == *name)
        {
            continue;
        }
        match prepare(state, &funs, &text, &sites) {
            Ok(window) => windows.push(window),
            Err(reason) => {
                if let Some(site) = sites.first() {
                    kept.push(site_row(&funs[site.fun], site, reason));
                }
            }
        }
        if windows.len() > MAX_WINDOWS {
            return json!({
                "status": "BLOCKED",
                "reason": format!(
                    "{MAX_WINDOWS} is the window cap; narrow the call with symbol. Nothing was planned."
                ),
            });
        }
    }
    let windows = accepting(state, windows, &mut kept);
    if windows.is_empty() {
        return json!({
            "status": "NO_EDITS",
            "reason": "no straight-line window with a written type for every free name",
            "kept": kept,
        });
    }
    let plan = match plan_for(state, &windows) {
        Ok(plan) => plan,
        Err(refusal) => return refusal,
    };
    let extracted = windows
        .iter()
        .map(|window| {
            json!({
                "helper": window.helper,
                "file": window.helper_file,
                "sites": window.sites,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": "PLANNED",
        "completeness": "PARTIAL",
        "extracted": extracted,
        "kept": kept,
        "plan": plan,
        "warnings": ["EXPLICIT_TYPES_ONLY"],
        "next": NEXT,
    })
}

fn focus(state: &RepositoryState, arguments: &Value) -> Result<Option<(String, String)>, Value> {
    let Some(value) = arguments.get("symbol") else {
        return Ok(None);
    };
    let Some(symbol) = value.as_str() else {
        return Err(super::invalid_args("extract_window", &["symbol"]));
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    if !matches!(node.kind, NodeKind::Function | NodeKind::Method) {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "extract_window reads a function or method",
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
    Ok(Some((
        span.file.clone(),
        node.label.trim_end_matches("()").to_owned(),
    )))
}

fn functions(state: &RepositoryState) -> Vec<Fun> {
    let mut files = BTreeSet::new();
    for node in state.graph().nodes() {
        if let Some(span) = node.span.as_ref()
            && edits::is_rust(&span.file)
        {
            files.insert(span.file.clone());
        }
    }
    let mut found = Vec::new();
    for file in files {
        let Some(source) = read_source(state.root(), &file) else {
            continue;
        };
        if !spans_match_source(&source) {
            continue;
        }
        let Ok(parsed) = syn::parse_file(&source) else {
            continue;
        };
        collect_items(&file, &source, &parsed.items, &mut found);
    }
    found
}

fn spans_match_source(source: &str) -> bool {
    if source.starts_with('\u{feff}') {
        return false;
    }
    !source.starts_with("#!") || source.starts_with("#![")
}

fn collect_items(file: &str, source: &str, items: &[syn::Item], found: &mut Vec<Fun>) {
    for item in items {
        match item {
            syn::Item::Fn(func) => {
                push_fun(file, source, &func.sig, &func.attrs, &func.block, found);
            }
            syn::Item::Impl(block) => {
                for inner in &block.items {
                    if let syn::ImplItem::Fn(func) = inner {
                        push_fun(file, source, &func.sig, &func.attrs, &func.block, found);
                    }
                }
            }
            _ => {}
        }
    }
}

fn push_fun(
    file: &str,
    source: &str,
    signature: &syn::Signature,
    attrs: &[syn::Attribute],
    block: &syn::Block,
    found: &mut Vec<Fun>,
) {
    let Some(cfg) = cfg_text(source, attrs) else {
        return;
    };
    let mut stmts = Vec::new();
    for stmt in &block.stmts {
        let range = stmt.span().byte_range();
        if range.start > range.end || range.end > source.len() {
            return;
        }
        let Some(text) = source.get(range.clone()) else {
            return;
        };
        stmts.push(Piece {
            start: range.start,
            end: range.end,
            text: text.to_owned(),
            stmt: stmt.clone(),
        });
    }
    found.push(Fun {
        file: file.to_owned(),
        name: signature.ident.to_string(),
        cfg,
        source: source.to_owned(),
        signature: signature.clone(),
        stmts,
    });
}

fn cfg_text(source: &str, attrs: &[syn::Attribute]) -> Option<String> {
    let mut parts = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("cfg") {
            continue;
        }
        let range = attr.span().byte_range();
        parts.push(source.get(range)?.to_owned());
    }
    Some(parts.join("\n"))
}

fn runs(funs: &[Fun]) -> BTreeMap<String, Vec<Site>> {
    let mut index = BTreeMap::<&str, Vec<(usize, usize)>>::new();
    for (fun_index, fun) in funs.iter().enumerate() {
        for (stmt_index, piece) in fun.stmts.iter().enumerate() {
            index
                .entry(piece.text.as_str())
                .or_default()
                .push((fun_index, stmt_index));
        }
    }
    let mut grouped = BTreeMap::<String, Vec<Site>>::new();
    for (left, fun) in funs.iter().enumerate() {
        for start in 0..fun.stmts.len() {
            let Some(hits) = index.get(fun.stmts[start].text.as_str()) else {
                continue;
            };
            for &(right, other) in hits {
                if right < left || (right == left && other <= start) {
                    continue;
                }
                extend_run(funs, left, start, right, other, &mut grouped);
            }
        }
    }
    grouped.retain(|_, sites| unique_sites(sites) >= 2);
    grouped
}

fn extend_run(
    funs: &[Fun],
    left: usize,
    start: usize,
    right: usize,
    other: usize,
    grouped: &mut BTreeMap<String, Vec<Site>>,
) {
    if start > 0
        && other > 0
        && funs[left].stmts[start - 1].text == funs[right].stmts[other - 1].text
    {
        return;
    }
    let mut length = 0_usize;
    while start.saturating_add(length) < funs[left].stmts.len()
        && other.saturating_add(length) < funs[right].stmts.len()
        && funs[left].stmts[start + length].text == funs[right].stmts[other + length].text
    {
        length += 1;
    }
    if !proper(&funs[left], start, length) || !proper(&funs[right], other, length) {
        return;
    }
    let left_text = window_text(&funs[left], start, length);
    let right_text = window_text(&funs[right], other, length);
    if left_text != right_text || line_count(&left_text) < 4 {
        return;
    }
    let sites = grouped.entry(left_text).or_default();
    sites.push(Site {
        fun: left,
        from: start,
        to: start.saturating_add(length),
    });
    sites.push(Site {
        fun: right,
        from: other,
        to: other.saturating_add(length),
    });
}

fn proper(fun: &Fun, from: usize, length: usize) -> bool {
    length > 0 && (from > 0 || from.saturating_add(length) < fun.stmts.len())
}

fn window_text(fun: &Fun, from: usize, length: usize) -> String {
    let start = fun.stmts[from].start;
    let end = fun.stmts[from.saturating_add(length).saturating_sub(1)].end;
    fun.source
        .get(start..end)
        .map(str::to_owned)
        .unwrap_or_default()
}

fn line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.matches('\n').count().saturating_add(1)
    }
}

fn unique_sites(sites: &[Site]) -> usize {
    let mut seen = BTreeSet::new();
    sites
        .iter()
        .filter(|site| seen.insert((site.fun, site.from, site.to)))
        .count()
}

fn prepare(
    state: &RepositoryState,
    funs: &[Fun],
    text: &str,
    sites: &[Site],
) -> Result<Window, &'static str> {
    let sites = dedup(sites);
    if sites.len() < 2 {
        return Err("the window is not repeated");
    }
    let cfg = &funs[sites[0].fun].cfg;
    if sites.iter().any(|site| funs[site.fun].cfg != *cfg) {
        return Err("cfg-split copies stay separate");
    }
    let mut facts = Vec::new();
    for site in &sites {
        facts.push(analyze(&funs[site.fun], site.from, site.to)?);
    }
    if facts
        .iter()
        .any(|fact| fact.params != facts[0].params || fact.outputs != facts[0].outputs)
    {
        return Err("the copies disagree on a binding type");
    }
    if overlaps_itself(funs, &sites) {
        return Err("the span overlaps another copy");
    }
    build_window(state, funs, text, &sites, &facts[0])
}

fn dedup(sites: &[Site]) -> Vec<Site> {
    let mut seen = BTreeSet::new();
    sites
        .iter()
        .filter(|site| seen.insert((site.fun, site.from, site.to)))
        .cloned()
        .collect()
}

fn overlaps_itself(funs: &[Fun], sites: &[Site]) -> bool {
    for (left, site) in sites.iter().enumerate() {
        let start = funs[site.fun].stmts[site.from].start;
        let end = funs[site.fun].stmts[site.to - 1].end;
        for other in sites.iter().skip(left + 1) {
            if site.fun != other.fun {
                continue;
            }
            let other_start = funs[other.fun].stmts[other.from].start;
            let other_end = funs[other.fun].stmts[other.to - 1].end;
            if start < other_end && other_start < end {
                return true;
            }
        }
    }
    false
}

fn analyze(fun: &Fun, from: usize, to: usize) -> Result<Facts, &'static str> {
    let mut env = params_of(&fun.signature);
    for piece in fun.stmts.iter().take(from) {
        if let Some((name, ty)) = simple_let(&piece.stmt) {
            env.insert(name, ty);
        }
    }
    let mut introduced = Vec::<(String, Option<String>)>::new();
    let mut params = Vec::new();
    let mut seen = BTreeSet::new();
    for piece in fun.stmts.iter().take(to).skip(from) {
        walk_stmt(&piece.stmt, &env, &mut introduced, &mut params, &mut seen)?;
    }
    let mut used = Vec::new();
    for piece in fun.stmts.iter().skip(to) {
        names_in_stmt(&piece.stmt, &mut used);
    }
    let mut outputs = Vec::new();
    for (name, ty) in &introduced {
        if !used.iter().any(|used_name| used_name == name) {
            continue;
        }
        let Some(ty) = ty else {
            return Err("a binding has no written type");
        };
        outputs.push((name.clone(), ty.clone()));
    }
    Ok(Facts { params, outputs })
}

fn params_of(signature: &syn::Signature) -> BTreeMap<String, Option<String>> {
    let mut env = BTreeMap::new();
    for input in &signature.inputs {
        let syn::FnArg::Typed(typed) = input else {
            continue;
        };
        let Some((name, inner)) = binding(&typed.pat) else {
            continue;
        };
        env.insert(name, render_type(&typed.ty).or(inner));
    }
    env
}

fn simple_let(stmt: &syn::Stmt) -> Option<(String, Option<String>)> {
    let syn::Stmt::Local(local) = stmt else {
        return None;
    };
    if local
        .init
        .as_ref()
        .is_some_and(|init| init.diverge.is_some())
    {
        return None;
    }
    binding(&local.pat)
}

fn binding(pat: &syn::Pat) -> Option<(String, Option<String>)> {
    match pat {
        syn::Pat::Ident(ident) if ident.by_ref.is_none() && ident.subpat.is_none() => {
            Some((ident.ident.to_string(), None))
        }
        syn::Pat::Type(typed) => {
            let (name, _) = binding(&typed.pat)?;
            Some((name, render_type(&typed.ty)))
        }
        _ => None,
    }
}

fn walk_stmt(
    stmt: &syn::Stmt,
    env: &BTreeMap<String, Option<String>>,
    introduced: &mut Vec<(String, Option<String>)>,
    params: &mut Vec<(String, String)>,
    seen: &mut BTreeSet<String>,
) -> Result<(), &'static str> {
    match stmt {
        syn::Stmt::Local(local) => {
            if local
                .init
                .as_ref()
                .is_some_and(|init| init.diverge.is_some())
            {
                return Err("the window leaves the function");
            }
            if let Some(init) = &local.init {
                walk_expr(&init.expr, env, introduced, params, seen)?;
            }
            let Some((name, ty)) = binding(&local.pat) else {
                return Err("the window is not straight-line");
            };
            if let Some(existing) = introduced.iter_mut().find(|(bound, _)| bound == &name) {
                existing.1 = ty;
            } else {
                introduced.push((name, ty));
            }
            Ok(())
        }
        syn::Stmt::Expr(expr, _) => walk_expr(expr, env, introduced, params, seen),
        syn::Stmt::Item(_) | syn::Stmt::Macro(_) => Err("the window is not straight-line"),
    }
}

fn walk_expr(
    expr: &syn::Expr,
    env: &BTreeMap<String, Option<String>>,
    introduced: &mut Vec<(String, Option<String>)>,
    params: &mut Vec<(String, String)>,
    seen: &mut BTreeSet<String>,
) -> Result<(), &'static str> {
    match expr {
        syn::Expr::Lit(_) => Ok(()),
        syn::Expr::Paren(inner) => walk_expr(&inner.expr, env, introduced, params, seen),
        syn::Expr::Reference(inner) => walk_expr(&inner.expr, env, introduced, params, seen),
        syn::Expr::Unary(inner) => walk_expr(&inner.expr, env, introduced, params, seen),
        syn::Expr::Cast(inner) => walk_expr(&inner.expr, env, introduced, params, seen),
        syn::Expr::Field(field) => walk_expr(&field.base, env, introduced, params, seen),
        syn::Expr::Path(path) => note_path(path, env, introduced, params, seen),
        syn::Expr::Binary(binary) => {
            if assigns(&binary.op) {
                refuse_outer_write(&binary.left, introduced)?;
            }
            walk_expr(&binary.left, env, introduced, params, seen)?;
            walk_expr(&binary.right, env, introduced, params, seen)
        }
        syn::Expr::Assign(assign) => {
            refuse_outer_write(&assign.left, introduced)?;
            walk_expr(&assign.right, env, introduced, params, seen)
        }
        syn::Expr::Index(index) => {
            walk_expr(&index.expr, env, introduced, params, seen)?;
            walk_expr(&index.index, env, introduced, params, seen)
        }
        syn::Expr::MethodCall(call) => {
            if is_self(&call.receiver) {
                return Err("the window reads self");
            }
            walk_expr(&call.receiver, env, introduced, params, seen)?;
            for argument in &call.args {
                walk_expr(argument, env, introduced, params, seen)?;
            }
            Ok(())
        }
        syn::Expr::Tuple(tuple) => walk_list(&tuple.elems, env, introduced, params, seen),
        syn::Expr::Array(array) => walk_list(&array.elems, env, introduced, params, seen),
        syn::Expr::Repeat(repeat) => {
            walk_expr(&repeat.expr, env, introduced, params, seen)?;
            walk_expr(&repeat.len, env, introduced, params, seen)
        }
        syn::Expr::Return(_)
        | syn::Expr::Break(_)
        | syn::Expr::Continue(_)
        | syn::Expr::Yield(_)
        | syn::Expr::Await(_)
        | syn::Expr::Try(_) => Err("the window leaves the function"),
        _ => Err("the window is not straight-line"),
    }
}

fn walk_list(
    exprs: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
    env: &BTreeMap<String, Option<String>>,
    introduced: &mut Vec<(String, Option<String>)>,
    params: &mut Vec<(String, String)>,
    seen: &mut BTreeSet<String>,
) -> Result<(), &'static str> {
    for expr in exprs {
        walk_expr(expr, env, introduced, params, seen)?;
    }
    Ok(())
}

fn note_path(
    path: &syn::ExprPath,
    env: &BTreeMap<String, Option<String>>,
    introduced: &[(String, Option<String>)],
    params: &mut Vec<(String, String)>,
    seen: &mut BTreeSet<String>,
) -> Result<(), &'static str> {
    if path.qself.is_some() || path.path.segments.len() != 1 {
        return Err("the window is not straight-line");
    }
    let name = path
        .path
        .segments
        .first()
        .map(|segment| segment.ident.to_string());
    let Some(name) = name else {
        return Err("the window is not straight-line");
    };
    if name == "self" {
        return Err("the window reads self");
    }
    if introduced.iter().any(|(bound, _)| bound == &name) {
        return Ok(());
    }
    match env.get(&name) {
        Some(Some(ty)) => {
            if seen.insert(name.clone()) {
                params.push((name, ty.clone()));
            }
            Ok(())
        }
        Some(None) | None => Err("a binding has no written type"),
    }
}

fn refuse_outer_write(
    left: &syn::Expr,
    introduced: &[(String, Option<String>)],
) -> Result<(), &'static str> {
    let syn::Expr::Path(path) = left else {
        return Err("the window writes a binding from outside");
    };
    if path.path.segments.len() != 1 {
        return Err("the window writes a binding from outside");
    }
    let name = path
        .path
        .segments
        .first()
        .map(|segment| segment.ident.to_string())
        .unwrap_or_default();
    if introduced.iter().any(|(bound, _)| bound == &name) {
        Ok(())
    } else {
        Err("the window writes a binding from outside")
    }
}

fn is_self(expr: &syn::Expr) -> bool {
    matches!(expr, syn::Expr::Path(path) if path.qself.is_none() && path.path.is_ident("self"))
}

const fn assigns(op: &syn::BinOp) -> bool {
    matches!(
        op,
        syn::BinOp::AddAssign(_)
            | syn::BinOp::SubAssign(_)
            | syn::BinOp::MulAssign(_)
            | syn::BinOp::DivAssign(_)
            | syn::BinOp::RemAssign(_)
            | syn::BinOp::BitXorAssign(_)
            | syn::BinOp::BitAndAssign(_)
            | syn::BinOp::BitOrAssign(_)
            | syn::BinOp::ShlAssign(_)
            | syn::BinOp::ShrAssign(_)
    )
}

fn names_in_stmt(stmt: &syn::Stmt, names: &mut Vec<String>) {
    match stmt {
        syn::Stmt::Local(local) => {
            if let Some(init) = &local.init {
                names_in_expr(&init.expr, names);
            }
        }
        syn::Stmt::Expr(expr, _) => names_in_expr(expr, names),
        syn::Stmt::Item(_) | syn::Stmt::Macro(_) => {}
    }
}

fn names_in_expr(expr: &syn::Expr, names: &mut Vec<String>) {
    match expr {
        syn::Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
            if let Some(segment) = path.path.segments.first() {
                names.push(segment.ident.to_string());
            }
        }
        syn::Expr::Binary(binary) => {
            names_in_expr(&binary.left, names);
            names_in_expr(&binary.right, names);
        }
        syn::Expr::Unary(unary) => names_in_expr(&unary.expr, names),
        syn::Expr::Paren(paren) => names_in_expr(&paren.expr, names),
        syn::Expr::Reference(reference) => names_in_expr(&reference.expr, names),
        syn::Expr::Field(field) => names_in_expr(&field.base, names),
        syn::Expr::Index(index) => {
            names_in_expr(&index.expr, names);
            names_in_expr(&index.index, names);
        }
        syn::Expr::MethodCall(call) => {
            names_in_expr(&call.receiver, names);
            for argument in &call.args {
                names_in_expr(argument, names);
            }
        }
        syn::Expr::Call(call) => {
            names_in_expr(&call.func, names);
            for argument in &call.args {
                names_in_expr(argument, names);
            }
        }
        syn::Expr::Cast(cast) => names_in_expr(&cast.expr, names),
        syn::Expr::Assign(assign) => {
            names_in_expr(&assign.left, names);
            names_in_expr(&assign.right, names);
        }
        syn::Expr::Block(block) => {
            for stmt in &block.block.stmts {
                names_in_stmt(stmt, names);
            }
        }
        syn::Expr::If(item) => {
            names_in_expr(&item.cond, names);
            for stmt in &item.then_branch.stmts {
                names_in_stmt(stmt, names);
            }
            if let Some((_, expr)) = &item.else_branch {
                names_in_expr(expr, names);
            }
        }
        _ => {}
    }
}

fn build_window(
    state: &RepositoryState,
    funs: &[Fun],
    body: &str,
    sites: &[Site],
    facts: &Facts,
) -> Result<Window, &'static str> {
    let mut ordered = sites.to_vec();
    ordered.sort_by(|left, right| {
        (&funs[left.fun].file, funs[left.fun].stmts[left.from].start).cmp(&(
            &funs[right.fun].file,
            funs[right.fun].stmts[right.from].start,
        ))
    });
    let helper_file = funs[ordered[0].fun].file.clone();
    let Some(source) = read_source(state.root(), &helper_file) else {
        return Err("the edit does not parse");
    };
    let base = facts.outputs.first().map_or_else(
        || "shared_block".to_owned(),
        |(name, _)| format!("{name}_block"),
    );
    let helper = fresh_name(&source, &base);
    let public = ordered
        .iter()
        .any(|site| funs[site.fun].file != helper_file);
    let mut edits = BTreeMap::<String, Vec<ByteEdit>>::new();
    for site in &ordered {
        let fun = &funs[site.fun];
        let Some(callee) = callee(&helper_file, &fun.file, &helper) else {
            return Err("the module path is not a Rust path");
        };
        let start = fun.stmts[site.from].start;
        let end = fun.stmts[site.to - 1].end;
        edits
            .entry(fun.file.clone())
            .or_default()
            .push(ByteEdit::new(start, end, call_text(&callee, facts)));
    }
    let cfg = if funs[ordered[0].fun].cfg.is_empty() {
        String::new()
    } else {
        format!("{}\n", funs[ordered[0].fun].cfg)
    };
    edits
        .entry(helper_file.clone())
        .or_default()
        .push(ByteEdit::new(
            source.len(),
            source.len(),
            helper_text(&cfg, public, &helper, facts, body),
        ));
    if !files_parse(state, &edits) {
        return Err("the edit does not parse");
    }
    Ok(Window {
        helper_file,
        helper,
        edits,
        sites: ordered.len(),
    })
}

fn callee(helper_file: &str, site_file: &str, name: &str) -> Option<String> {
    if helper_file == site_file {
        return Some(name.to_owned());
    }
    Some(format!("{}::{name}", edits::module_path(helper_file)?))
}

fn call_text(callee: &str, facts: &Facts) -> String {
    let args = facts
        .params
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let call = format!("{callee}({args})");
    match facts.outputs.as_slice() {
        [] => format!("{call};"),
        [one] => format!("let {} = {call};", one.0),
        many => {
            let names = many
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("let ({names}) = {call};")
        }
    }
}

fn helper_text(cfg: &str, public: bool, name: &str, facts: &Facts, body: &str) -> String {
    let visibility = if public { "pub(crate) " } else { "" };
    let args = facts
        .params
        .iter()
        .map(|(name, ty)| format!("{name}: {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    let ret = match facts.outputs.as_slice() {
        [] => String::new(),
        [one] => format!(" -> {}", one.1),
        many => {
            let types = many
                .iter()
                .map(|(_, ty)| ty.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(" -> ({types})")
        }
    };
    let tail = match facts.outputs.as_slice() {
        [] => String::new(),
        [one] => format!("\n    {}", one.0),
        many => {
            let names = many
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("\n    ({names})")
        }
    };
    format!(
        "\n{cfg}{visibility}fn {name}({args}){ret} {{\n{}\n}}\n",
        format!("{}\n{tail}", indent_body(body)).trim_start_matches('\n')
    )
}

fn indent_body(body: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("    ");
        out.push_str(line.trim_start());
    }
    out
}

fn fresh_name(source: &str, base: &str) -> String {
    let mut name = base.to_owned();
    let mut suffix = 2_u32;
    while has_fn(source, &name) {
        name = format!("{base}_{suffix}");
        suffix = suffix.saturating_add(1);
        if suffix > 40 {
            break;
        }
    }
    name
}

fn has_fn(source: &str, name: &str) -> bool {
    let tokens = tokenize(source, Language::Rust);
    let significant = tokens
        .iter()
        .filter(|token| !token.is_trivia())
        .collect::<Vec<_>>();
    significant
        .windows(2)
        .any(|pair| pair[0].text(source) == "fn" && pair[1].text(source) == name)
}

fn files_parse(state: &RepositoryState, edits: &BTreeMap<String, Vec<ByteEdit>>) -> bool {
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

fn accepting(state: &RepositoryState, windows: Vec<Window>, kept: &mut Vec<Value>) -> Vec<Window> {
    let mut accepted = Vec::new();
    for window in windows {
        let mut trial = accepted.clone();
        trial.push(window);
        if combined_parses(state, &trial) {
            accepted = trial;
        } else if let Some(last) = trial.last() {
            kept.push(json!({
                "symbol": last.helper,
                "file": last.helper_file,
                "reason": "the edit does not parse",
            }));
        }
    }
    accepted
}

fn combined_parses(state: &RepositoryState, windows: &[Window]) -> bool {
    let mut by_file = BTreeMap::<String, Vec<ByteEdit>>::new();
    for window in windows {
        for (file, edits) in &window.edits {
            let slot = by_file.entry(file.clone()).or_default();
            for edit in edits {
                if let Some(existing) = slot.iter_mut().find(|item| {
                    item.start == edit.start && item.end == edit.end && item.start == item.end
                }) {
                    existing.after.push_str(&edit.after);
                } else {
                    slot.push(edit.clone());
                }
            }
        }
    }
    files_parse(state, &by_file)
}

fn plan_for(state: &RepositoryState, windows: &[Window]) -> Result<Value, Value> {
    let mut by_file = BTreeMap::<String, Vec<ByteEdit>>::new();
    for window in windows {
        for (file, edits) in &window.edits {
            let slot = by_file.entry(file.clone()).or_default();
            for edit in edits {
                if let Some(existing) = slot.iter_mut().find(|item| {
                    item.start == edit.start && item.end == edit.end && item.start == item.end
                }) {
                    existing.after.push_str(&edit.after);
                } else {
                    slot.push(edit.clone());
                }
            }
        }
    }
    let mut builder = PlanBuilder::new("extract_window");
    for (file, edits) in by_file {
        let Some(source) = read_source(state.root(), &file) else {
            return Err(super::stale_graph(&file));
        };
        builder = edits::push_edits(builder, &file, &source, &edits)?;
    }
    Ok(builder.build())
}

fn site_row(fun: &Fun, site: &Site, reason: &str) -> Value {
    let line = edits::position(&fun.source, fun.stmts[site.from].start).map_or(0, |(line, _)| line);
    json!({
        "symbol": fun.name,
        "file": fun.file,
        "line": line,
        "reason": reason,
    })
}

#[cfg(test)]
mod tests {
    use super::extract_window;
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    #[test]
    fn a_repository_without_a_window_plans_nothing() {
        let state = fixture_state();
        let answer = extract_window(&state, &json!({}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("NO_EDITS"),
            "{answer:?}"
        );
    }

    #[test]
    fn a_non_string_symbol_is_invalid_args() {
        let state = fixture_state();
        let answer = extract_window(&state, &json!({"symbol": 1}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("INVALID_ARGS")
        );
    }
}
