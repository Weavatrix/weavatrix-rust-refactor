//! Plan a shared helper for identical method bodies on different types.
//!
//! Both methods stay. The helper takes the fields the body reads, in the order they appear, and
//! each method becomes a wrapper. Same-type copies stay with `remove_duplicates`. The plan is
//! not written here.

use super::edits::{self, ByteEdit};
use super::rust_type::render_type;
use crate::declaration::locate;
use crate::evidence::read_source;
use crate::plan::PlanBuilder;
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_parse::{Language, tokenize};
use weavatrix_rust::{EdgeKind, NodeKind, RepositoryState};

const MAX_METHODS: usize = 200;
const NEXT: &str = "apply with apply_edit_plan (preview -> confirm).";

struct Copy {
    id: String,
    label: String,
    file: String,
    line: u32,
    owner_id: Option<String>,
    owner_name: String,
    owner_file: String,
    interior: String,
    body_start: usize,
    body_end: usize,
    ret: Option<String>,
}

#[derive(Clone)]
struct Family {
    helper_file: String,
    helper: String,
    edits: BTreeMap<String, Vec<ByteEdit>>,
    methods: Vec<usize>,
}

pub(super) fn share_method_body(state: &RepositoryState, arguments: &Value) -> Value {
    let selected = match selected(state, arguments) {
        Ok(text) => text,
        Err(refusal) => return refusal,
    };
    let copies = methods(state);
    let mut kept = Vec::new();
    let mut families = Vec::new();
    let mut reserved = BTreeMap::<String, BTreeSet<String>>::new();
    for members in groups(&copies, selected.as_deref()) {
        consider(
            state,
            &copies,
            &members,
            &mut reserved,
            &mut families,
            &mut kept,
        );
        let queued = families
            .iter()
            .map(|family| family.methods.len())
            .sum::<usize>();
        if queued > MAX_METHODS {
            return json!({
                "status": "BLOCKED",
                "reason": format!(
                    "{MAX_METHODS} is the method cap; narrow the call with symbol. Nothing was planned."
                ),
            });
        }
    }
    let families = accepting(state, &copies, families, &mut kept);
    if families.is_empty() {
        return json!({
            "status": "NO_EDITS",
            "reason": "no identical method body on different types",
            "kept": kept,
        });
    }
    let plan = match plan_for(state, &families) {
        Ok(plan) => plan,
        Err(refusal) => return refusal,
    };
    let helpers = families
        .iter()
        .map(|family| helper_row(&copies, family))
        .collect::<Vec<_>>();
    json!({
        "status": "PLANNED",
        "completeness": "PARTIAL",
        "helpers": helpers,
        "kept": kept,
        "plan": plan,
        "warnings": ["FIELD_READS_ONLY"],
        "next": NEXT,
    })
}

fn selected(state: &RepositoryState, arguments: &Value) -> Result<Option<String>, Value> {
    let Some(value) = arguments.get("symbol") else {
        return Ok(None);
    };
    let Some(symbol) = value.as_str() else {
        return Err(super::invalid_args("share_method_body", &["symbol"]));
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    if node.kind != NodeKind::Method {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "share_method_body reads a method",
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
    let Some(text) = source.get(range.start..range.end) else {
        return Err(super::stale_graph(&span.file));
    };
    let Some((_, _, interior)) = interior_at(text) else {
        return Err(json!({
            "status": "NOT_A_SYMBOL",
            "reason": "the declaration end is not proven, so the body cannot be shared",
            "symbol": symbol,
        }));
    };
    Ok(Some(interior))
}

fn methods(state: &RepositoryState) -> Vec<Copy> {
    let graph = state.graph();
    let mut sources = BTreeMap::<String, String>::new();
    let mut found = Vec::new();
    for node in graph.nodes() {
        if node.kind != NodeKind::Method {
            continue;
        }
        let Some(span) = node.span.as_ref() else {
            continue;
        };
        if !sources.contains_key(&span.file) {
            let Some(text) = read_source(state.root(), &span.file) else {
                continue;
            };
            sources.insert(span.file.clone(), text);
        }
        let Some(source) = sources.get(&span.file) else {
            continue;
        };
        let Some(copy) = one_method(state, node, source) else {
            continue;
        };
        found.push(copy);
    }
    found
}

fn one_method(state: &RepositoryState, node: &weavatrix_graph::Node, source: &str) -> Option<Copy> {
    let span = node.span.as_ref()?;
    let name = node.label.trim_end_matches("()");
    let range = locate(source, &span.file, name, span.start.line)?;
    if !range.end_proven {
        return None;
    }
    let text = source.get(range.start..range.end)?;
    let (relative_start, relative_end, interior) = interior_at(text)?;
    let method: syn::ImplItemFn = syn::parse_str(text).ok()?;
    let (owner_id, owner_name, owner_file) = owner_of(state, node.id.as_str()).map_or_else(
        || (None, String::new(), String::new()),
        |(id, owner_name, owner_file)| (Some(id), owner_name, owner_file),
    );
    Some(Copy {
        id: node.id.as_str().to_owned(),
        label: name.to_owned(),
        file: span.file.clone(),
        line: span.start.line,
        owner_id,
        owner_name,
        owner_file,
        interior,
        body_start: range.start.saturating_add(relative_start),
        body_end: range.start.saturating_add(relative_end),
        ret: render_return(&method.sig),
    })
}

fn owner_of(state: &RepositoryState, id: &str) -> Option<(String, String, String)> {
    let graph = state.graph();
    let index = graph.node_index(id)?;
    let edge = graph
        .incoming_at(index)
        .find(|edge| edge.kind == EdgeKind::Method)?;
    let owner_index = graph.node_index(edge.source.as_str())?;
    let owner = graph.node_at(owner_index)?;
    let file = owner
        .span
        .as_ref()
        .map(|span| span.file.clone())
        .unwrap_or_default();
    Some((
        edge.source.as_str().to_owned(),
        owner.label.trim_end_matches("()").to_owned(),
        file,
    ))
}

fn interior_at(text: &str) -> Option<(usize, usize, String)> {
    let (from, to) = brace_interior(text)?;
    Some((from, to, text.get(from..to)?.to_owned()))
}

fn brace_interior(text: &str) -> Option<(usize, usize)> {
    let tokens = tokenize(text, Language::Rust);
    let mut depth = 0_i32;
    let mut open = None;
    for token in &tokens {
        match token.text(text) {
            "{" => {
                if depth == 0 {
                    open = Some(token.end);
                }
                depth += 1;
            }
            "}" => {
                depth -= 1;
                if depth == 0 {
                    return Some((open?, token.start));
                }
            }
            _ => {}
        }
    }
    None
}

fn render_return(signature: &syn::Signature) -> Option<String> {
    match &signature.output {
        syn::ReturnType::Default => Some(String::new()),
        syn::ReturnType::Type(_, ty) => render_type(ty),
    }
}

fn groups(copies: &[Copy], only: Option<&str>) -> Vec<Vec<usize>> {
    let mut grouped = BTreeMap::<&str, Vec<usize>>::new();
    for (index, copy) in copies.iter().enumerate() {
        if only.is_some_and(|text| text != copy.interior) {
            continue;
        }
        grouped
            .entry(copy.interior.as_str())
            .or_default()
            .push(index);
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
    reserved: &mut BTreeMap<String, BTreeSet<String>>,
    families: &mut Vec<Family>,
    kept: &mut Vec<Value>,
) {
    let mut ordered = members.to_vec();
    sort_by_place(copies, &mut ordered);
    match assemble(state, copies, &ordered, reserved) {
        Ok(family) => {
            reserved
                .entry(family.helper_file.clone())
                .or_default()
                .insert(family.helper.clone());
            families.push(family);
        }
        Err(reason) => kept.push(row(&copies[ordered[0]], reason)),
    }
}

fn assemble(
    state: &RepositoryState,
    copies: &[Copy],
    members: &[usize],
    reserved: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Family, &'static str> {
    if members
        .iter()
        .any(|index| !edits::is_rust(&copies[*index].file))
    {
        return Err("a shared method body is implemented for Rust");
    }
    distinct_parents(copies, members)?;
    let ret = same_return(copies, members)?;
    let fields = field_order(&copies[members[0]].interior)?;
    let typed = shared_fields(state, copies, members, &fields)?;
    build_family(state, copies, members, &typed, &ret, reserved)
}

fn distinct_parents(copies: &[Copy], members: &[usize]) -> Result<(), &'static str> {
    let mut seen = BTreeSet::new();
    for index in members {
        let Some(owner) = &copies[*index].owner_id else {
            return Err("the owning type is not in the graph");
        };
        if !seen.insert(owner.clone()) {
            return Err("same-type methods stay with remove_duplicates");
        }
    }
    Ok(())
}

fn same_return(copies: &[Copy], members: &[usize]) -> Result<String, &'static str> {
    let first = copies[members[0]]
        .ret
        .as_ref()
        .ok_or("the return type is not a plain path")?;
    for index in members.iter().skip(1) {
        let ret = copies[*index]
            .ret
            .as_ref()
            .ok_or("the return type is not a plain path")?;
        if ret != first {
            return Err("the copies disagree on a return type");
        }
    }
    Ok(first.clone())
}

fn field_order(interior: &str) -> Result<Vec<String>, &'static str> {
    let wrapped = format!("{{{interior}}}");
    let block: syn::Block = syn::parse_str(&wrapped).map_err(|_| "the body is not a field read")?;
    let mut fields = Vec::new();
    for stmt in &block.stmts {
        let syn::Stmt::Expr(expr, _) = stmt else {
            return Err("the body reads a local");
        };
        walk(expr, &mut fields)?;
    }
    if fields.is_empty() {
        return Err("the body has no field read");
    }
    Ok(fields)
}

fn walk(expr: &syn::Expr, fields: &mut Vec<String>) -> Result<(), &'static str> {
    match expr {
        syn::Expr::Lit(_) => Ok(()),
        syn::Expr::Paren(inner) => walk(&inner.expr, fields),
        syn::Expr::Group(inner) => walk(&inner.expr, fields),
        syn::Expr::Unary(inner) => walk(&inner.expr, fields),
        syn::Expr::Binary(binary) if arithmetic(&binary.op) => {
            walk(&binary.left, fields)?;
            walk(&binary.right, fields)
        }
        syn::Expr::Field(field) => note_field(field, fields),
        syn::Expr::MethodCall(call) => note_call(call, fields),
        syn::Expr::Path(path) if path.qself.is_none() && path.path.is_ident("self") => {
            Err("the body reads self")
        }
        _ => Err("the body reads more than a field"),
    }
}

fn note_field(field: &syn::ExprField, fields: &mut Vec<String>) -> Result<(), &'static str> {
    let base_is_self = matches!(
        field.base.as_ref(),
        syn::Expr::Path(path) if path.qself.is_none() && path.path.is_ident("self")
    );
    if !base_is_self {
        return Err("the body reads more than a field");
    }
    let syn::Member::Named(name) = &field.member else {
        return Err("the body reads more than a field");
    };
    let name = name.to_string();
    if !fields.iter().any(|field| field == &name) {
        fields.push(name);
    }
    Ok(())
}

fn note_call(call: &syn::ExprMethodCall, fields: &mut Vec<String>) -> Result<(), &'static str> {
    let receiver_is_self = matches!(
        call.receiver.as_ref(),
        syn::Expr::Path(path) if path.qself.is_none() && path.path.is_ident("self")
    );
    if receiver_is_self {
        return Err("the body calls a method on self");
    }
    walk(&call.receiver, fields)?;
    for argument in &call.args {
        walk(argument, fields)?;
    }
    Ok(())
}

fn arithmetic(op: &syn::BinOp) -> bool {
    matches!(
        op,
        syn::BinOp::Add(_)
            | syn::BinOp::Sub(_)
            | syn::BinOp::Mul(_)
            | syn::BinOp::Div(_)
            | syn::BinOp::Rem(_)
    )
}

fn shared_fields(
    state: &RepositoryState,
    copies: &[Copy],
    members: &[usize],
    names: &[String],
) -> Result<Vec<(String, String)>, &'static str> {
    let mut maps = Vec::new();
    for index in members {
        maps.push(fields_of(
            state,
            &copies[*index].owner_file,
            &copies[*index].owner_name,
        )?);
    }
    agree(&maps, names)
}

fn fields_of(
    state: &RepositoryState,
    file: &str,
    name: &str,
) -> Result<BTreeMap<String, Option<String>>, &'static str> {
    if let Some(map) = read_struct(state, file, name) {
        return Ok(map);
    }
    let mut files = BTreeSet::new();
    for node in state.graph().nodes() {
        if let Some(span) = node.span.as_ref()
            && edits::is_rust(&span.file)
            && span.file != file
        {
            files.insert(span.file.clone());
        }
    }
    let mut found = None;
    for candidate in files {
        let Some(map) = read_struct(state, &candidate, name) else {
            continue;
        };
        if found.is_some() {
            return Err("the owning type is not in the graph");
        }
        found = Some(map);
    }
    found.ok_or("the owning type is not in the graph")
}

fn read_struct(
    state: &RepositoryState,
    file: &str,
    name: &str,
) -> Option<BTreeMap<String, Option<String>>> {
    let source = read_source(state.root(), file)?;
    let parsed = syn::parse_file(&source).ok()?;
    let mut found = None;
    for item in parsed.items {
        let syn::Item::Struct(item) = item else {
            continue;
        };
        if item.ident != name {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(named_fields(&item));
    }
    found
}

fn named_fields(item: &syn::ItemStruct) -> BTreeMap<String, Option<String>> {
    let mut fields = BTreeMap::new();
    let syn::Fields::Named(named) = &item.fields else {
        return fields;
    };
    for field in &named.named {
        let Some(name) = &field.ident else {
            continue;
        };
        fields.insert(name.to_string(), render_type(&field.ty));
    }
    fields
}

fn agree(
    maps: &[BTreeMap<String, Option<String>>],
    names: &[String],
) -> Result<Vec<(String, String)>, &'static str> {
    let mut out = Vec::new();
    for name in names {
        let mut seen = None;
        for map in maps {
            let ty = match map.get(name) {
                Some(Some(ty)) => ty.clone(),
                Some(None) => return Err("a field type is not a plain path"),
                None => return Err("the owning type has no such field"),
            };
            if let Some(previous) = &seen
                && previous != &ty
            {
                return Err("the copies disagree on a field type");
            }
            seen = Some(ty);
        }
        let Some(ty) = seen else {
            return Err("the owning type has no such field");
        };
        out.push((name.clone(), ty));
    }
    Ok(out)
}

fn build_family(
    state: &RepositoryState,
    copies: &[Copy],
    members: &[usize],
    fields: &[(String, String)],
    ret: &str,
    reserved: &BTreeMap<String, BTreeSet<String>>,
) -> Result<Family, &'static str> {
    let helper_file = copies[members[0]].file.clone();
    let Some(source) = read_source(state.root(), &helper_file) else {
        return Err("the edit does not parse");
    };
    let base = format!("{}_of", copies[members[0]].label);
    let helper = fresh_name(&source, reserved.get(&helper_file), &base);
    let public = members
        .iter()
        .any(|index| copies[*index].file != helper_file);
    let names = fields
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let mut edits = BTreeMap::<String, Vec<ByteEdit>>::new();
    for index in members {
        let copy = &copies[*index];
        let callee = callee(&helper_file, &copy.file, &helper)?;
        add_edit(
            edits.entry(copy.file.clone()).or_default(),
            &ByteEdit::new(
                copy.body_start,
                copy.body_end,
                invocation(&callee, &names, &copy.interior),
            ),
        );
    }
    let body = indent_body(&strip_self(&copies[members[0]].interior, &names));
    add_edit(
        edits.entry(helper_file.clone()).or_default(),
        &ByteEdit::new(
            source.len(),
            source.len(),
            helper_source(public, &helper, fields, ret, &body),
        ),
    );
    if !files_parse(state, &edits) {
        return Err("the edit does not parse");
    }
    Ok(Family {
        helper_file,
        helper,
        edits,
        methods: members.to_vec(),
    })
}

fn callee(helper_file: &str, method_file: &str, name: &str) -> Result<String, &'static str> {
    if helper_file == method_file {
        return Ok(name.to_owned());
    }
    let module = edits::module_path(helper_file).ok_or("the module path is not a Rust path")?;
    Ok(format!("{module}::{name}"))
}

fn invocation(callee: &str, fields: &[String], interior: &str) -> String {
    let args = fields
        .iter()
        .map(|field| format!("self.{field}"))
        .collect::<Vec<_>>()
        .join(", ");
    let call = format!("{callee}({args})");
    if !interior.contains('\n') {
        return if interior.starts_with(' ') {
            format!(" {call}")
        } else {
            call
        };
    }
    let indent = first_indent(interior);
    let newline = if interior.ends_with('\n') { "\n" } else { "" };
    format!("\n{indent}{call}{newline}")
}

fn first_indent(text: &str) -> String {
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let spaces = line.len().saturating_sub(line.trim_start().len());
        return " ".repeat(spaces);
    }
    "    ".to_owned()
}

fn helper_source(
    public: bool,
    name: &str,
    fields: &[(String, String)],
    ret: &str,
    body: &str,
) -> String {
    let visibility = if public { "pub(crate) " } else { "" };
    let args = fields
        .iter()
        .map(|(name, ty)| format!("{name}: {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    let arrow = if ret.is_empty() {
        String::new()
    } else {
        format!(" -> {ret}")
    };
    format!("\n{visibility}fn {name}({args}){arrow} {{\n{body}\n}}\n")
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

fn strip_self(source: &str, fields: &[String]) -> String {
    let tokens = tokenize(source, Language::Rust);
    let significant = tokens
        .iter()
        .filter(|token| !token.is_trivia())
        .collect::<Vec<_>>();
    let mut cursor = 0_usize;
    let mut out = String::new();
    let mut index = 0_usize;
    while index < significant.len() {
        let token = significant[index];
        let field = significant
            .get(index.saturating_add(2))
            .filter(|_| index.saturating_add(2) < significant.len());
        let strips = token.text(source) == "self"
            && significant
                .get(index.saturating_add(1))
                .is_some_and(|dot| dot.text(source) == ".")
            && field.is_some_and(|field| fields.iter().any(|name| name == field.text(source)));
        if strips && let Some(field) = field {
            if let Some(prefix) = source.get(cursor..token.start) {
                out.push_str(prefix);
            }
            cursor = field.start;
            index = index.saturating_add(2);
            continue;
        }
        index = index.saturating_add(1);
    }
    if let Some(rest) = source.get(cursor..) {
        out.push_str(rest);
    }
    out
}

fn fresh_name(source: &str, reserved: Option<&BTreeSet<String>>, base: &str) -> String {
    let mut name = base.to_owned();
    let mut suffix = 2_u32;
    while reserved.is_some_and(|taken| taken.contains(&name)) || has_fn(source, &name) {
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

fn add_edit(slot: &mut Vec<ByteEdit>, edit: &ByteEdit) {
    if let Some(existing) = slot
        .iter_mut()
        .find(|item| item.start == edit.start && item.end == edit.end && item.start == item.end)
    {
        existing.after.push_str(&edit.after);
    } else {
        slot.push(edit.clone());
    }
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

fn accepting(
    state: &RepositoryState,
    copies: &[Copy],
    families: Vec<Family>,
    kept: &mut Vec<Value>,
) -> Vec<Family> {
    let mut accepted = Vec::new();
    for family in families {
        let mut trial = accepted.clone();
        trial.push(family);
        if combined_parses(state, &trial) {
            accepted = trial;
        } else if let Some(last) = trial.last()
            && let Some(index) = last.methods.first()
        {
            kept.push(row(&copies[*index], "the edit does not parse"));
        }
    }
    accepted
}

fn combined_parses(state: &RepositoryState, families: &[Family]) -> bool {
    let mut by_file = BTreeMap::<String, Vec<ByteEdit>>::new();
    for family in families {
        for (file, edits) in &family.edits {
            let slot = by_file.entry(file.clone()).or_default();
            for edit in edits {
                add_edit(slot, edit);
            }
        }
    }
    files_parse(state, &by_file)
}

fn plan_for(state: &RepositoryState, families: &[Family]) -> Result<Value, Value> {
    let mut by_file = BTreeMap::<String, Vec<ByteEdit>>::new();
    for family in families {
        for (file, edits) in &family.edits {
            let slot = by_file.entry(file.clone()).or_default();
            for edit in edits {
                add_edit(slot, edit);
            }
        }
    }
    let mut builder = PlanBuilder::new("share_method_body");
    for (file, edits) in by_file {
        let Some(source) = read_source(state.root(), &file) else {
            return Err(super::stale_graph(&file));
        };
        builder = edits::push_edits(builder, &file, &source, &edits)?;
    }
    Ok(builder.build())
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
        "type": copy.owner_name,
        "file": copy.file,
        "line": copy.line,
        "reason": reason,
    })
}

fn helper_row(copies: &[Copy], family: &Family) -> Value {
    let methods = family
        .methods
        .iter()
        .map(|index| {
            json!({
                "symbol": copies[*index].label,
                "type": copies[*index].owner_name,
                "file": copies[*index].file,
                "line": copies[*index].line,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "helper": family.helper,
        "file": family.helper_file,
        "methods": methods,
    })
}

#[cfg(test)]
mod tests {
    use super::share_method_body;
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    #[test]
    fn a_repository_without_shared_methods_plans_nothing() {
        let state = fixture_state();
        let answer = share_method_body(&state, &json!({}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("NO_EDITS"),
            "{answer:?}"
        );
    }

    #[test]
    fn a_non_string_symbol_is_invalid_args() {
        let state = fixture_state();
        let answer = share_method_body(&state, &json!({"symbol": 1}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("INVALID_ARGS")
        );
    }
}
