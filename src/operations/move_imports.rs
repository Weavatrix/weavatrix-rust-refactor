//! Import repairs that travel with a symbol move.
//!
//! A caller whose import named the symbol is retargeted at the new file. The destination
//! receives the imports the declaration itself uses, including siblings that stayed behind.
//! The old file, when it still mentions the name, gains an import of the new location.

use crate::specifier::{between, is_relative, resolve, rewrite};
use weavatrix_parse::Import;
use weavatrix_rust::RepositoryState;

/// One replacement, in byte offsets of the file as it is now.
pub(super) struct ImportEdit {
    pub(super) file: String,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) after: String,
}

/// What the move has to do to imports, apart from cutting the declaration.
pub(super) struct MoveImports {
    pub(super) edits: Vec<ImportEdit>,
    /// Files whose import of the symbol was retargeted or removed.
    pub(super) rewritten: Vec<String>,
    /// Import lines to insert with the declaration.
    pub(super) carried: String,
    /// Replacement for the declaration when the old file still uses the name.
    pub(super) leftover: String,
}

pub(super) struct Origin<'a> {
    pub(super) source: &'a str,
    pub(super) from: &'a str,
    pub(super) to_file: &'a str,
    pub(super) name: &'a str,
    pub(super) declaration: &'a str,
    pub(super) at: (usize, usize),
    pub(super) destination: &'a str,
}

pub(super) fn repairs(state: &RepositoryState, origin: &Origin<'_>) -> MoveImports {
    let carried = carried_lines(origin);
    let leftover = leftover_use(origin);
    let mut edits = Vec::new();
    let mut rewritten = Vec::new();
    for file in indexed_files(state) {
        if file == origin.from {
            continue;
        }
        let Some(text) = crate::evidence::read_source(state.root(), &file) else {
            continue;
        };
        let Some(facts) = weavatrix_parse::extract_path(&file, &text) else {
            continue;
        };
        let mut changed = false;
        for import in &facts.imports {
            if !names_symbol(import, origin.name)
                || !points_at(&file, &import.specifier, origin.from)
            {
                continue;
            }
            if file == origin.to_file && rust_sole_use(&text, import, origin.name) {
                // `remove_sole_import` already deletes this line.
                changed = true;
                continue;
            }
            let Some(edit) = retarget(
                &file,
                &text,
                import,
                origin.from,
                origin.to_file,
                origin.name,
            ) else {
                continue;
            };
            edits.push(edit);
            changed = true;
        }
        if changed {
            rewritten.push(file);
        }
    }
    MoveImports {
        edits,
        rewritten,
        carried,
        leftover,
    }
}

fn carried_lines(origin: &Origin<'_>) -> String {
    let Some(facts) = weavatrix_parse::extract_path(origin.from, origin.source) else {
        return String::new();
    };
    let mut lines = String::new();
    let mut carried_names = Vec::new();
    for import in &facts.imports {
        if points_at(origin.from, &import.specifier, origin.to_file) {
            continue;
        }
        let used = used_locals(import, origin.name, origin.declaration);
        if used.is_empty() {
            continue;
        }
        carried_names.extend(used);
        let Some(statement) = origin.source.get(import.span.start..import.span.end) else {
            continue;
        };
        let rewritten = rewrite_carried(origin.from, origin.to_file, &import.specifier, statement);
        push_line(&mut lines, origin.destination, &rewritten);
    }
    for item in &facts.declarations {
        if item.name.is_empty()
            || item.name == origin.name
            || carried_names.iter().any(|have| have == &item.name)
        {
            continue;
        }
        if item.span.start >= origin.at.0 && item.span.start < origin.at.1 {
            continue;
        }
        if !word_in(origin.declaration, &item.name) {
            continue;
        }
        // The sibling stayed in the old file, so the new file imports it from there.
        push_line(
            &mut lines,
            origin.destination,
            &import_of(origin.from, origin.to_file, &item.name),
        );
    }
    lines
}

fn leftover_use(origin: &Origin<'_>) -> String {
    let (start, end) = origin.at;
    let Some(outside) = outside_text(origin.source, start, end) else {
        return String::new();
    };
    if !word_in(&outside, origin.name) {
        return String::new();
    }
    import_of(origin.to_file, origin.from, origin.name)
}

/// An import of `name`, which is defined in `defining_file`, written into `importer`.
fn import_of(defining_file: &str, importer: &str, name: &str) -> String {
    if is_rust(defining_file) || is_rust(importer) {
        let Some(module) = rust_module_path(defining_file) else {
            return String::new();
        };
        return format!("use {module}::{name};\n");
    }
    let specifier = between(importer, defining_file);
    format!("import {{ {name} }} from '{specifier}';\n")
}

fn rewrite_carried(from: &str, to_file: &str, specifier: &str, statement: &str) -> String {
    let updated = if is_relative(specifier) {
        resolve(from, specifier).map_or_else(
            || specifier.to_owned(),
            |resolved| rewrite(to_file, specifier, &resolved),
        )
    } else if specifier.contains("::") {
        rust_absolute(from, specifier).unwrap_or_else(|| specifier.to_owned())
    } else {
        specifier.to_owned()
    };
    statement.replacen(specifier, &updated, 1)
}

fn retarget(
    file: &str,
    source: &str,
    import: &Import,
    from: &str,
    to_file: &str,
    name: &str,
) -> Option<ImportEdit> {
    let end = semicolon_end(source, import.span.end);
    let statement = source.get(import.span.start..end)?;
    let sole = sole_binding(import, name);
    let (start, end, after) = if file == to_file {
        if sole {
            let end = eat_terminator(source, import.span.end);
            let start = blank_line_start(source, import.span.start);
            (start, end, String::new())
        } else {
            (
                import.span.start,
                end,
                remove_binding(statement, import, name),
            )
        }
    } else if sole {
        let updated = retarget_specifier(file, &import.specifier, from, to_file)?;
        (
            import.span.start,
            end,
            statement.replacen(&import.specifier, &updated, 1),
        )
    } else {
        let updated = fresh_import(file, &import.specifier, from, to_file, name)?;
        let without = remove_binding(statement, import, name);
        (import.span.start, end, format!("{without}\n{updated}"))
    };
    Some(ImportEdit {
        file: file.to_owned(),
        start,
        end,
        after,
    })
}

fn semicolon_end(source: &str, end: usize) -> usize {
    if source.as_bytes().get(end) == Some(&b';') {
        end + 1
    } else {
        end
    }
}

fn retarget_specifier(
    importer: &str,
    specifier: &str,
    from: &str,
    to_file: &str,
) -> Option<String> {
    if is_relative(specifier) {
        return Some(retarget_relative(importer, specifier, to_file));
    }
    retarget_rust(importer, specifier, from, to_file)
}

fn fresh_import(
    importer: &str,
    specifier: &str,
    from: &str,
    to_file: &str,
    name: &str,
) -> Option<String> {
    if is_relative(specifier) {
        let path = retarget_relative(importer, specifier, to_file);
        return Some(format!("import {{ {name} }} from '{path}';"));
    }
    let module = retarget_rust(importer, specifier, from, to_file)?;
    let path = if module.ends_with(&format!("::{name}")) {
        module
    } else {
        format!("{module}::{name}")
    };
    Some(format!("use {path};"))
}

fn retarget_relative(importer: &str, specifier: &str, to_file: &str) -> String {
    let updated = between(importer, to_file);
    let wrote_extension = specifier
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.') && !last.starts_with('.'));
    if wrote_extension {
        return updated;
    }
    match updated.rsplit_once('.') {
        Some((stem, _)) => stem.to_owned(),
        None => updated,
    }
}

fn retarget_rust(importer: &str, specifier: &str, from: &str, to_file: &str) -> Option<String> {
    let old = rust_module_path(from)?;
    let new_module = rust_module_path(to_file)?;
    let absolute = rust_absolute(importer, specifier)?;
    if absolute == old {
        return Some(new_module);
    }
    let rest = absolute.strip_prefix(&old)?.strip_prefix("::")?;
    if rest.is_empty() {
        Some(new_module)
    } else {
        Some(format!("{new_module}::{rest}"))
    }
}

fn points_at(importer: &str, specifier: &str, file: &str) -> bool {
    if is_relative(specifier) {
        let Some(resolved) = resolve(importer, specifier) else {
            return false;
        };
        return resolved == file
            || file
                .rsplit_once('.')
                .is_some_and(|(stem, _)| resolved == stem);
    }
    if !is_rust(file) {
        return false;
    }
    let Some(module) = rust_module_path(file) else {
        return false;
    };
    let Some(absolute) = rust_absolute(importer, specifier) else {
        return false;
    };
    absolute == module
        || absolute
            .strip_prefix(&module)
            .is_some_and(|rest| rest.starts_with("::"))
}

fn is_rust(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
}

fn rust_module_path(file: &str) -> Option<String> {
    if !is_rust(file) {
        return None;
    }
    let path = std::path::Path::new(file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let path = path.as_str();
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = path.strip_prefix("src/").unwrap_or(path);
    if path.is_empty() || path == "lib" || path == "main" {
        return Some("crate".to_owned());
    }
    let path = path.strip_suffix("/mod").unwrap_or(path);
    let path = path.strip_suffix("/lib").unwrap_or(path);
    if path.is_empty() {
        Some("crate".to_owned())
    } else {
        Some(format!("crate::{}", path.replace('/', "::")))
    }
}

fn rust_absolute(file: &str, specifier: &str) -> Option<String> {
    if specifier.is_empty() {
        return None;
    }
    let mut module = rust_module_path(file).unwrap_or_else(|| "crate".to_owned());
    let mut parts = specifier.split("::").peekable();
    while let Some(head) = parts.peek().copied() {
        match head {
            "self" => {
                parts.next();
            }
            "crate" => {
                parts.next();
                "crate".clone_into(&mut module);
            }
            "super" => {
                parts.next();
                let index = module.rfind("::")?;
                module.truncate(index);
            }
            _ => break,
        }
    }
    let rest = parts.collect::<Vec<_>>().join("::");
    if rest.is_empty() {
        Some(module)
    } else {
        Some(format!("{module}::{rest}"))
    }
}

fn names_symbol(import: &Import, name: &str) -> bool {
    if import.bindings.is_empty() {
        return import.names.iter().any(|local| local == name);
    }
    import
        .bindings
        .iter()
        .any(|binding| binding.local == name || binding.imported == name)
}

fn sole_binding(import: &Import, name: &str) -> bool {
    let locals = locals_of(import);
    locals.len() == 1 && locals.first().is_some_and(|local| *local == name)
}

fn locals_of(import: &Import) -> Vec<&str> {
    if import.bindings.is_empty() {
        import.names.iter().map(String::as_str).collect()
    } else {
        import
            .bindings
            .iter()
            .map(|binding| binding.local.as_str())
            .collect()
    }
}

fn used_locals(import: &Import, symbol: &str, declaration: &str) -> Vec<String> {
    locals_of(import)
        .into_iter()
        .filter(|local| *local != symbol && word_in(declaration, local))
        .map(str::to_owned)
        .collect()
}

fn rust_sole_use(source: &str, import: &Import, name: &str) -> bool {
    if !sole_binding(import, name) {
        return false;
    }
    let Some(statement) = source.get(import.span.start..import.span.end) else {
        return false;
    };
    let trimmed = statement.trim();
    trimmed.starts_with("use ") && !trimmed.contains('{') && !trimmed.contains(',')
}

fn remove_binding(statement: &str, import: &Import, name: &str) -> String {
    let imported = import
        .bindings
        .iter()
        .find(|binding| binding.local == name)
        .map_or(name, |binding| binding.imported.as_str());
    let Some((start, end)) = binding_at(statement, imported, name) else {
        return statement.to_owned();
    };
    let (start, end) = comma_around(statement, start, end);
    let mut out = String::new();
    out.push_str(statement.get(..start).unwrap_or(""));
    out.push_str(statement.get(end..).unwrap_or(""));
    out
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

fn comma_around(text: &str, start: usize, end: usize) -> (usize, usize) {
    let Some(rest) = text.get(end..) else {
        return (start, end);
    };
    let spaces = rest
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .map(char::len_utf8)
        .sum::<usize>();
    if text
        .get(end + spaces..)
        .is_some_and(|tail| tail.starts_with(','))
    {
        return (start, end + spaces + 1);
    }
    let Some(before) = text.get(..start) else {
        return (start, end);
    };
    let Some(comma) = before.rfind(',') else {
        return (start, end);
    };
    if before
        .get(comma + 1..)
        .is_some_and(|gap| gap.chars().all(char::is_whitespace))
    {
        return (comma, end);
    }
    (start, end)
}

fn word_in(text: &str, name: &str) -> bool {
    word_at(text, name).is_some()
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

fn outside_text(source: &str, start: usize, end: usize) -> Option<String> {
    let mut text = String::new();
    text.push_str(source.get(..start)?);
    text.push_str(source.get(end..)?);
    Some(text)
}

fn push_line(lines: &mut String, destination: &str, statement: &str) {
    let mut trimmed = statement.trim().to_owned();
    if trimmed.is_empty() {
        return;
    }
    let is_import = trimmed.starts_with("use ") || trimmed.starts_with("import ");
    if is_import && !trimmed.ends_with(';') {
        trimmed.push(';');
    }
    if destination.contains(trimmed.trim_end_matches(';')) || lines.contains(&trimmed) {
        return;
    }
    lines.push_str(&trimmed);
    if !lines.ends_with('\n') {
        lines.push('\n');
    }
}

fn eat_terminator(source: &str, mut end: usize) -> usize {
    if source.as_bytes().get(end) == Some(&b';') {
        end += 1;
    }
    if source.as_bytes().get(end) == Some(&b'\r') {
        end += 1;
    }
    if source.as_bytes().get(end) == Some(&b'\n') {
        end += 1;
    }
    end
}

fn blank_line_start(source: &str, start: usize) -> usize {
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    if source
        .get(line_start..start)
        .is_some_and(|prefix| prefix.chars().all(char::is_whitespace))
    {
        line_start
    } else {
        start
    }
}

fn indexed_files(state: &RepositoryState) -> Vec<String> {
    let mut files = state
        .graph()
        .nodes()
        .iter()
        .filter(|node| node.kind == weavatrix_rust::NodeKind::File)
        .map(|node| node.label.clone())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    files.sort_unstable();
    files.dedup();
    files
}
