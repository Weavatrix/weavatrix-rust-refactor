//! The gaps that used to come back as `NOT_SUPPORTED` or `UNPROVEN`, probed together.
//!
//! Each case is the one that previously refused the whole tool. A skip when the graph misses a
//! symbol would hide the gap again, so a missing node fails the test.

use blazingly_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use weavatrix_rust::{NodeKind, RepositoryState, Weavatrix};
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-gaps-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, contents) in files {
        let full = root.join(path);
        fs::create_dir_all(full.parent().expect("parent")).expect("directories");
        fs::write(full, contents).expect("source");
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    fs::write(
        root.join("package.json"),
        "{\"name\":\"fixture\",\"version\":\"0.0.0\",\"type\":\"module\"}\n",
    )
    .expect("package");
    root
}

fn open(root: &Path) -> Weavatrix {
    Weavatrix::open(root).expect("repository opens")
}

fn status_of(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn expect_status(value: &Value, status: &str) {
    assert_eq!(status_of(value), status, "{value:?}");
    assert_ne!(status, "NOT_SUPPORTED");
    assert_ne!(status_of(value), "UNPROVEN", "{value:?}");
}

fn node_id(state: &RepositoryState, kind: &NodeKind, label: &str) -> String {
    let found = state
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == *kind && node.label.trim_end_matches("()") == label);
    let Some(node) = found else {
        let seen = state
            .graph()
            .nodes()
            .iter()
            .map(|node| format!("{:?} {}", node.kind, node.label))
            .collect::<Vec<_>>();
        panic!("missing {kind:?} {label}: {seen:?}");
    };
    node.id.as_str().to_owned()
}

fn apply(session: &RefactorSession, state: &RepositoryState, plan: &Value) {
    let preview = session
        .call(state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("preview");
    expect_status(&preview, "PREVIEW_OK");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token}),
        )
        .expect("apply");
    expect_status(&applied, "APPLIED");
}

#[test]
fn a_rust_signature_change_is_planned() {
    let root = repository(
        "sig",
        &[(
            "src/lib.rs",
            "pub fn used(value: u32) -> u32 {\n    value\n}\n",
        )],
    );
    let engine = open(&root);
    let id = node_id(engine.state(), &NodeKind::Function, "used");
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "change_signature",
            &json!({"symbol": id, "operation": {"kind": "add_parameter", "name": "extra"}}),
        )
        .expect("tool");
    expect_status(&answer, "PLANNED");
    let plan = format!("{:?}", answer.get("plan"));
    assert!(plan.contains("extra: u32"), "{plan}");
}

#[test]
fn a_rust_related_rename_previews_the_new_name() {
    let root = repository(
        "related",
        &[(
            "src/lib.rs",
            "pub fn alpha(value: u32) -> u32 {\n    value\n}\n",
        )],
    );
    let engine = open(&root);
    let id = node_id(engine.state(), &NodeKind::Function, "alpha");
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "rename_related_symbols",
            &json!({"renames": [{"symbol": id, "new_name": "first"}]}),
        )
        .expect("tool");
    expect_status(&answer, "PREVIEW_OK");
    assert!(answer.get("confirmToken").and_then(Value::as_str).is_some());
    assert!(format!("{:?}", answer.get("plan")).contains("first"));
}

#[test]
fn organize_imports_edits_rust_python_and_go_and_names_an_unknown_language() {
    let root = repository(
        "imports",
        &[
            (
                "src/lib.rs",
                "use crate::util::helper;\nuse std::io::Write;\n\nfn main() {}\n",
            ),
            (
                "src/app.py",
                "from x import A\nimport os\n\ndef run():\n    return os\n",
            ),
            (
                "main.go",
                "package main\nimport \"fmt\"\nimport _ \"embed\"\nimport \"os\"\nfunc main() { fmt.Println(1) }\n",
            ),
            ("App.kt", "import foo.Bar\n"),
            ("notes.xyz", "import foo.Bar\n"),
        ],
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let session = RefactorSession::new(true);

    let rust = session
        .call(&state, "organize_imports", &json!({"file": "src/lib.rs"}))
        .expect("tool");
    expect_status(&rust, "PLANNED");
    apply(&session, &state, rust.get("plan").expect("plan"));
    let rust_after = fs::read_to_string(root.join("src/lib.rs")).expect("rust");
    assert!(!rust_after.contains("helper"), "{rust_after}");
    assert!(rust_after.contains("Write"), "{rust_after}");

    let python = session
        .call(&state, "organize_imports", &json!({"file": "src/app.py"}))
        .expect("tool");
    expect_status(&python, "PLANNED");
    apply(&session, &state, python.get("plan").expect("plan"));
    let python_after = fs::read_to_string(root.join("src/app.py")).expect("python");
    assert!(!python_after.contains("from x"), "{python_after}");
    assert!(python_after.contains("import os"), "{python_after}");

    let go = session
        .call(&state, "organize_imports", &json!({"file": "main.go"}))
        .expect("tool");
    expect_status(&go, "PLANNED");
    apply(&session, &state, go.get("plan").expect("plan"));
    let go_after = fs::read_to_string(root.join("main.go")).expect("go");
    assert!(!go_after.contains("\"os\""), "{go_after}");
    assert!(go_after.contains("_ \"embed\""), "{go_after}");
    assert!(go_after.contains("\"fmt\""), "{go_after}");

    let kotlin = session
        .call(&state, "organize_imports", &json!({"file": "App.kt"}))
        .expect("tool");
    expect_status(&kotlin, "PLANNED");
    apply(&session, &state, kotlin.get("plan").expect("plan"));
    let kotlin_after = fs::read_to_string(root.join("App.kt")).expect("kotlin");
    assert!(!kotlin_after.contains("import foo.Bar"), "{kotlin_after}");

    let unknown = session
        .call(&state, "organize_imports", &json!({"file": "notes.xyz"}))
        .expect("tool");
    expect_status(&unknown, "NO_UNUSED_IMPORTS");
    assert!(unknown.get("plan").is_none(), "{unknown:?}");
}

#[test]
fn organize_imports_removes_unused_java_csharp_and_cpp_names() {
    let root = repository(
        "managed",
        &[
            (
                "App.java",
                "import com.foo.Bar;\nimport com.foo.Used;\nclass T { Used value; }\n",
            ),
            (
                "App.cs",
                "using System.Text;\nusing Alias = System.IO;\nclass T { Text value; }\n",
            ),
            (
                "App.cc",
                "#include <vector>\nusing std::vector;\nusing namespace std;\nint main() { return 0; }\n",
            ),
        ],
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let session = RefactorSession::new(true);

    let java = session
        .call(&state, "organize_imports", &json!({"file": "App.java"}))
        .expect("tool");
    expect_status(&java, "PLANNED");
    apply(&session, &state, java.get("plan").expect("plan"));
    let java_after = fs::read_to_string(root.join("App.java")).expect("java");
    assert!(!java_after.contains("Bar"), "{java_after}");
    assert!(java_after.contains("Used"), "{java_after}");

    let csharp = session
        .call(&state, "organize_imports", &json!({"file": "App.cs"}))
        .expect("tool");
    expect_status(&csharp, "PLANNED");
    apply(&session, &state, csharp.get("plan").expect("plan"));
    let csharp_after = fs::read_to_string(root.join("App.cs")).expect("csharp");
    assert!(!csharp_after.contains("Alias"), "{csharp_after}");
    assert!(csharp_after.contains("Text"), "{csharp_after}");

    let cpp = session
        .call(&state, "organize_imports", &json!({"file": "App.cc"}))
        .expect("tool");
    expect_status(&cpp, "PLANNED");
    apply(&session, &state, cpp.get("plan").expect("plan"));
    let cpp_after = fs::read_to_string(root.join("App.cc")).expect("cpp");
    assert!(!cpp_after.contains("using std::vector"), "{cpp_after}");
    assert!(cpp_after.contains("#include"), "{cpp_after}");
    assert!(cpp_after.contains("using namespace std"), "{cpp_after}");
}

#[test]
fn bulk_replace_skips_a_comment_and_a_string() {
    let root = repository(
        "bulk",
        &[(
            "src/lib.rs",
            "#[mark]\nfn mark() {}\n// mark\nconst S: &str = \"mark\";\n",
        )],
    );
    let engine = open(&root);
    let session = RefactorSession::read_only();
    let answer = session
        .call(
            engine.state(),
            "bulk_replace",
            &json!({"pattern": "mark", "replacement": "tag"}),
        )
        .expect("tool");
    expect_status(&answer, "PREVIEW");
    assert_eq!(
        answer.get("total").and_then(Value::as_u64),
        Some(2),
        "{answer:?}"
    );
    let with_trivia = session
        .call(
            engine.state(),
            "bulk_replace",
            &json!({"pattern": "mark", "replacement": "tag", "include_trivia": true}),
        )
        .expect("tool");
    assert_eq!(
        with_trivia.get("total").and_then(Value::as_u64),
        Some(4),
        "{with_trivia:?}"
    );
}

#[test]
fn delete_closure_refuses_a_repository_and_a_spanless_package() {
    let root = repository(
        "containers",
        &[(
            "src/service.js",
            "import leftPad from \"left-pad\";\nexport const padded = leftPad(\"x\", 3);\n",
        )],
    );
    let engine = open(&root);
    let state = engine.state();
    let session = RefactorSession::read_only();
    let repository_id = state
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::Repository)
        .map(|node| node.id.as_str().to_owned())
        .expect("repository node");
    let blocked = session
        .call(
            state,
            "delete_closure",
            &json!({"symbol": repository_id, "scope": "exclusive"}),
        )
        .expect("tool");
    expect_status(&blocked, "BLOCKED");

    let package_id = state
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::Package && node.span.is_none())
        .map(|node| node.id.as_str().to_owned())
        .expect("a spanless package");
    let missing = session
        .call(
            state,
            "delete_closure",
            &json!({"symbol": package_id, "scope": "exclusive"}),
        )
        .expect("tool");
    expect_status(&missing, "NOT_A_SYMBOL");
}

#[test]
fn delete_closure_removes_an_unused_file_and_a_cross_file_handler() {
    let root = repository(
        "files",
        &[(
            "src/service.js",
            "import { only } from \"./only.js\";\nconst app = { get(path, handler) { return handler; } };\napp.get(\"/only\", only);\nexport function other() { return 1; }\n",
        )],
    );
    fs::write(
        root.join("src/orphan.js"),
        "export function orphan() { return 1; }\n",
    )
    .expect("orphan");
    fs::write(
        root.join("src/only.js"),
        "export function only() {\n  return 1;\n}\n",
    )
    .expect("only");
    let engine = open(&root);
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let orphan = node_id(&state, &NodeKind::File, "src/orphan.js");
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": orphan, "scope": "exclusive"}),
        )
        .expect("tool");
    expect_status(&planned, "PLANNED");
    apply(&session, &state, planned.get("plan").expect("plan"));
    assert!(!root.join("src/orphan.js").exists());
    assert!(root.join("src/service.js").is_file());

    let route = state
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::Endpoint && node.label.contains("/only"))
        .map(|node| node.id.as_str().to_owned())
        .expect("route");
    let route_plan = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": route, "scope": "exclusive"}),
        )
        .expect("tool");
    expect_status(&route_plan, "PLANNED");
    apply(&session, &state, route_plan.get("plan").expect("plan"));
    let service = fs::read_to_string(root.join("src/service.js")).expect("service");
    let handler = fs::read_to_string(root.join("src/only.js")).expect("handler");
    assert!(!service.contains("\"/only\""), "{service}");
    assert!(!service.contains("only.js"), "{service}");
    assert!(service.contains("function other("), "{service}");
    assert!(!handler.contains("function only("), "{handler}");
}

#[test]
fn a_private_unused_symbol_is_safe_and_still_needs_review() {
    let root = repository("ready", &[("src/lib.rs", "fn hidden() {}\n")]);
    let engine = open(&root);
    let id = node_id(engine.state(), &NodeKind::Function, "hidden");
    let answer = RefactorSession::read_only()
        .call(engine.state(), "delete_readiness", &json!({"symbol": id}))
        .expect("tool");
    expect_status(&answer, "OK");
    assert_eq!(
        answer.get("verdict").and_then(Value::as_str),
        Some("REVIEW_REQUIRED")
    );
    assert_eq!(
        answer.get("safe").and_then(Value::as_bool),
        Some(true),
        "{answer:?}"
    );
}

#[test]
fn edit_symbol_plans_a_rust_body() {
    let root = repository(
        "edit",
        &[(
            "src/lib.rs",
            "pub fn used(value: u32) -> u32 {\n    value\n}\n",
        )],
    );
    let engine = open(&root);
    let id = node_id(engine.state(), &NodeKind::Function, "used");
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "edit_symbol",
            &json!({
                "symbol": id,
                "operation": "replace_symbol_body",
                "content": "pub fn used(value: u32) -> u32 { value + 1 }",
            }),
        )
        .expect("tool");
    expect_status(&answer, "PLANNED");
    assert_eq!(
        answer.get("completeness").and_then(Value::as_str),
        Some("COMPLETE"),
        "{answer:?}"
    );
    assert!(answer.get("plan").is_some(), "{answer:?}");
    let warnings = answer
        .get("warnings")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        !warnings.contains(&"SYNTAX_CHECK_NOT_PERFORMED"),
        "{answer:?}"
    );
}

#[test]
fn edit_symbol_refuses_a_body_that_does_not_parse() {
    let root = repository(
        "edit-syntax",
        &[(
            "src/lib.rs",
            "pub fn used(value: u32) -> u32 {\n    value\n}\n",
        )],
    );
    let engine = open(&root);
    let id = node_id(engine.state(), &NodeKind::Function, "used");
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "edit_symbol",
            &json!({
                "symbol": id,
                "operation": "replace_symbol_body",
                "content": "pub fn used( {",
            }),
        )
        .expect("tool");
    expect_status(&answer, "INVALID_PLAN");
    assert!(answer.get("plan").is_none(), "{answer:?}");
}

#[test]
fn remove_duplicates_plans_only_the_unused_exact_copy() {
    let root = repository(
        "dedupe",
        &[
            ("src/lib.rs", "pub mod extra;\npub mod kept;\n"),
            (
                "src/kept.rs",
                "fn copy() -> u32 {\n    1 + 2\n}\n\npub fn run() -> u32 {\n    copy()\n}\n",
            ),
            ("src/extra.rs", "fn copy() -> u32 {\n    1 + 2\n}\n"),
        ],
    );
    let engine = open(&root);
    let answer = RefactorSession::read_only()
        .call(engine.state(), "remove_duplicates", &json!({}))
        .expect("tool");
    expect_status(&answer, "PLANNED");
    let deleted = answer
        .get("deleted")
        .and_then(Value::as_array)
        .expect("deleted");
    assert_eq!(deleted.len(), 1, "{answer:?}");
    assert_eq!(
        deleted[0].get("file").and_then(Value::as_str),
        Some("src/extra.rs"),
        "{answer:?}"
    );
    let preview = RefactorSession::read_only()
        .call(
            engine.state(),
            "apply_edit_plan",
            &json!({"plan": answer.get("plan").expect("plan").clone()}),
        )
        .expect("preview");
    expect_status(&preview, "PREVIEW_OK");
}

#[test]
fn move_file_plans_the_filesystem_rename() {
    let root = repository(
        "move-file",
        &[
            ("src/core.js", "export const value = 1\n"),
            ("src/main.js", "import {value} from './core.js'\n"),
        ],
    );
    let engine = open(&root);
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "move_file",
            &json!({"from": "src/core.js", "to": "src/deep/core.js"}),
        )
        .expect("tool");
    expect_status(&answer, "PLANNED");
    let plan = format!("{:?}", answer.get("plan"));
    assert!(plan.contains("src/deep/core.js"), "{plan}");
    assert!(!plan.contains("FILE_RENAME_NOT_INCLUDED"), "{plan}");
}

#[test]
fn move_symbol_emits_an_edit_plan() {
    let root = repository(
        "move-symbol",
        &[
            ("src/lib.rs", "pub mod helper;\npub mod caller;\n"),
            ("src/helper.rs", "pub fn helper() -> u32 { 1 }\n"),
            (
                "src/caller.rs",
                "use crate::helper::helper;\npub fn run() -> u32 { helper() }\n",
            ),
        ],
    );
    let engine = open(&root);
    let id = node_id(engine.state(), &NodeKind::Function, "helper");
    let answer = RefactorSession::read_only()
        .call(
            engine.state(),
            "move_symbol",
            &json!({"symbol": id, "to_file": "src/caller.rs"}),
        )
        .expect("tool");
    expect_status(&answer, "EVALUATED");
    assert_eq!(
        answer.get("fidelity").and_then(Value::as_str),
        Some("PROJECTED_FROM_GRAPH_EDGES")
    );
    assert!(answer.get("plan").is_some(), "{answer:?}");
}

#[test]
fn rename_apply_and_rollback_restore_the_bytes() {
    let source = "pub fn alpha(value: u32) -> u32 {\n    value\n}\n";
    let root = repository("round", &[("src/lib.rs", source)]);
    let engine = open(&root);
    let state = engine.state().clone();
    let id = node_id(&state, &NodeKind::Function, "alpha");
    let session = RefactorSession::new(true);
    let preview = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "first"}),
        )
        .expect("tool");
    expect_status(&preview, "PLANNED");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            &state,
            "rename_symbol",
            &json!({"symbol": id, "new_name": "first", "mode": "apply", "confirm_token": token}),
        )
        .expect("tool");
    expect_status(&applied, "APPLIED");
    assert!(
        fs::read_to_string(root.join("src/lib.rs"))
            .expect("after")
            .contains("fn first")
    );
    let rolled = session
        .call(&state, "rollback_last_apply", &json!({}))
        .expect("tool");
    expect_status(&rolled, "ROLLED_BACK");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).expect("restored"),
        source
    );
}

fn check_compiles(root: &Path) {
    let output = std::process::Command::new("cargo")
        .args(["check", "--offline", "--quiet", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .expect("cargo check");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn plan_text(answer: &Value) -> String {
    format!("{:?}", answer.get("plan"))
}

#[test]
fn extract_duplicate_retargets_the_used_copy() {
    let body = "fn ordered_sel(items: &[u32]) -> Vec<u32> {\n    let mut out = items.to_vec();\n    out.sort_unstable();\n    out\n}\n";
    let root = repository(
        "extract-dup",
        &[
            ("src/lib.rs", "pub mod app;\npub mod ui;\n"),
            (
                "src/app.rs",
                &format!(
                    "{body}\npub fn show(items: &[u32]) -> Vec<u32> {{\n    ordered_sel(items)\n}}\n"
                ),
            ),
            (
                "src/ui.rs",
                &format!(
                    "{body}\npub fn paint(items: &[u32]) -> Vec<u32> {{\n    ordered_sel(items)\n}}\n"
                ),
            ),
        ],
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let answer = session
        .call(&state, "extract_duplicate", &json!({}))
        .expect("tool");
    expect_status(&answer, "PLANNED");
    let deleted = answer
        .get("deleted")
        .and_then(Value::as_array)
        .expect("deleted");
    assert_eq!(deleted.len(), 1, "{answer:?}");
    assert_eq!(
        deleted[0].get("file").and_then(Value::as_str),
        Some("src/ui.rs"),
        "{answer:?}"
    );
    let plan = plan_text(&answer);
    assert!(plan.contains("pub(crate)"), "{plan}");
    assert!(plan.contains("crate::app::ordered_sel"), "{plan}");
    apply(&session, &state, answer.get("plan").expect("plan"));
    let app = fs::read_to_string(root.join("src/app.rs")).expect("app");
    let ui = fs::read_to_string(root.join("src/ui.rs")).expect("ui");
    assert!(app.contains("pub(crate) fn ordered_sel"), "{app}");
    assert!(ui.contains("crate::app::ordered_sel"), "{ui}");
    assert!(!ui.contains("fn ordered_sel"), "{ui}");
    check_compiles(&root);
}

#[test]
fn extract_duplicate_leaves_an_unused_copy_for_remove_duplicates() {
    let root = repository(
        "extract-unused",
        &[
            ("src/lib.rs", "pub mod extra;\npub mod kept;\n"),
            (
                "src/kept.rs",
                "fn copy() -> u32 {\n    1 + 2\n}\n\npub fn run() -> u32 {\n    copy()\n}\n",
            ),
            ("src/extra.rs", "fn copy() -> u32 {\n    1 + 2\n}\n"),
        ],
    );
    let engine = open(&root);
    let answer = RefactorSession::read_only()
        .call(engine.state(), "extract_duplicate", &json!({}))
        .expect("tool");
    expect_status(&answer, "NO_EDITS");
    assert!(answer.get("plan").is_none(), "{answer:?}");
}

#[test]
fn extract_window_keeps_cfg_split_copies_apart() {
    let root = repository(
        "extract-cfg",
        &[(
            "src/lib.rs",
            "#[cfg(unix)]\nfn enter() -> u32 {\n    let tag = 1;\n    let width: u32 = 1;\n    let height: u32 = 2;\n    let area: u32 = width * height;\n    let padded: u32 = area + height;\n    padded\n}\n\n#[cfg(windows)]\nfn enter() -> u32 {\n    let tag = 2;\n    let width: u32 = 1;\n    let height: u32 = 2;\n    let area: u32 = width * height;\n    let padded: u32 = area + height;\n    padded\n}\n",
        )],
    );
    let engine = open(&root);
    let answer = RefactorSession::read_only()
        .call(engine.state(), "extract_window", &json!({}))
        .expect("tool");
    expect_status(&answer, "NO_EDITS");
    assert!(
        format!("{answer:?}").contains("cfg-split copies stay separate"),
        "{answer:?}"
    );
}

#[test]
fn extract_window_refuses_a_binding_without_a_written_type() {
    let root = repository(
        "extract-untyped",
        &[(
            "src/lib.rs",
            "fn left() -> u32 {\n    let scale = 3;\n    let banner = \"left\";\n    let width: u32 = scale;\n    let height: u32 = 4;\n    let area: u32 = width * height;\n    let padded: u32 = area + height;\n    padded + 1\n}\n\nfn right() -> u32 {\n    let scale = 3;\n    let banner = \"right\";\n    let width: u32 = scale;\n    let height: u32 = 4;\n    let area: u32 = width * height;\n    let padded: u32 = area + height;\n    padded + 2\n}\n",
        )],
    );
    let engine = open(&root);
    let answer = RefactorSession::read_only()
        .call(engine.state(), "extract_window", &json!({}))
        .expect("tool");
    expect_status(&answer, "NO_EDITS");
    assert!(
        format!("{answer:?}").contains("a binding has no written type"),
        "{answer:?}"
    );
}

#[test]
fn extract_window_lifts_a_typed_run() {
    let left = "pub fn paint(scale: u32) -> u32 {\n    let banner = \"left\";\n    let width: u32 = scale;\n    let height: u32 = 4;\n    let area: u32 = width * height;\n    let padded: u32 = area + height;\n    banner.len() as u32 + padded\n}\n";
    let right = "pub fn paint(scale: u32) -> u32 {\n    let banner = \"right\";\n    let width: u32 = scale;\n    let height: u32 = 4;\n    let area: u32 = width * height;\n    let padded: u32 = area + height;\n    banner.len() as u32 + padded + 1\n}\n";
    let root = repository(
        "extract-window",
        &[
            ("src/lib.rs", "pub mod left;\npub mod right;\n"),
            ("src/left.rs", left),
            ("src/right.rs", right),
        ],
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let answer = session
        .call(&state, "extract_window", &json!({}))
        .expect("tool");
    expect_status(&answer, "PLANNED");
    let plan = plan_text(&answer);
    assert!(plan.contains("padded_block"), "{plan}");
    assert!(plan.contains("pub(crate)"), "{plan}");
    assert!(plan.contains("crate::left::padded_block"), "{plan}");
    apply(&session, &state, answer.get("plan").expect("plan"));
    let left_after = fs::read_to_string(root.join("src/left.rs")).expect("left");
    let right_after = fs::read_to_string(root.join("src/right.rs")).expect("right");
    assert!(left_after.contains("fn paint"), "{left_after}");
    assert!(right_after.contains("fn paint"), "{right_after}");
    assert!(
        right_after.contains("crate::left::padded_block(scale)"),
        "{right_after}"
    );
    check_compiles(&root);
}

#[test]
fn share_method_body_wraps_both_methods() {
    let method = "    pub fn duration(&self) -> f64 { (self.end - self.start).max(0.0) }\n";
    let root = repository(
        "share-method",
        &[
            ("src/lib.rs", "pub mod cue;\npub mod intake;\n"),
            (
                "src/cue.rs",
                &format!(
                    "pub struct Cue {{\n    start: f64,\n    end: f64,\n}}\n\nimpl Cue {{\n{method}}}\n"
                ),
            ),
            (
                "src/intake/mod.rs",
                &format!(
                    "pub struct Segment {{\n    start: f64,\n    end: f64,\n}}\n\nimpl Segment {{\n{method}}}\n"
                ),
            ),
        ],
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let session = RefactorSession::new(true);
    let answer = session
        .call(&state, "share_method_body", &json!({}))
        .expect("tool");
    expect_status(&answer, "PLANNED");
    let plan = plan_text(&answer);
    assert!(plan.contains("duration_of"), "{plan}");
    assert!(plan.contains("pub(crate)"), "{plan}");
    assert!(plan.contains("crate::cue::duration_of"), "{plan}");
    let duration = node_id(&state, &NodeKind::Method, "duration");
    let refused = session
        .call(&state, "extract_duplicate", &json!({"symbol": duration}))
        .expect("tool");
    expect_status(&refused, "NOT_A_SYMBOL");
    apply(&session, &state, answer.get("plan").expect("plan"));
    let cue = fs::read_to_string(root.join("src/cue.rs")).expect("cue");
    let intake = fs::read_to_string(root.join("src/intake/mod.rs")).expect("intake");
    assert!(cue.contains("fn duration"), "{cue}");
    assert!(intake.contains("fn duration"), "{intake}");
    assert!(cue.contains("fn duration_of"), "{cue}");
    assert!(
        intake.contains("crate::cue::duration_of(self.end, self.start)"),
        "{intake}"
    );
    check_compiles(&root);
}

#[test]
fn share_method_body_keeps_same_type_methods() {
    let body = "(self.end - self.start).max(0.0)";
    let root = repository(
        "share-same",
        &[(
            "src/lib.rs",
            &format!(
                "pub struct Cue {{\n    start: f64,\n    end: f64,\n}}\n\nimpl Cue {{\n    pub fn duration(&self) -> f64 {{ {body} }}\n    pub fn span(&self) -> f64 {{ {body} }}\n}}\n"
            ),
        )],
    );
    let engine = open(&root);
    let answer = RefactorSession::read_only()
        .call(engine.state(), "share_method_body", &json!({}))
        .expect("tool");
    expect_status(&answer, "NO_EDITS");
    assert!(
        format!("{answer:?}").contains("same-type methods stay with remove_duplicates"),
        "{answer:?}"
    );
}
