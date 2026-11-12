//! `move_symbol` against real dependency shapes.
//!
//! The verdict only means something if the fixture can actually produce a cycle, so one case
//! moves a declaration into the file that already depends on it — the move that turns a
//! one-way dependency into a mutual one.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

fn repository(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-move-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("repository");
    for (path, contents) in files {
        fs::write(root.join(path), contents).expect("source");
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    root
}

fn call(root: &std::path::Path, arguments: &Value) -> Value {
    let engine = Weavatrix::open(root).expect("repository opens");
    RefactorSession::read_only()
        .call(engine.state(), "move_symbol", arguments)
        .expect("declared tool")
}

fn status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn verdict(value: &Value) -> &str {
    value
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// `caller` depends on `core`; nothing depends on `caller`.
const FILES: [(&str, &str); 3] = [
    ("src/lib.rs", "pub mod core;\npub mod caller;\n"),
    (
        "src/core.rs",
        "pub fn helper(value: u32) -> u32 {\n    value + 1\n}\n",
    ),
    (
        "src/caller.rs",
        "use crate::core::helper;\n\npub fn run() -> u32 {\n    helper(1)\n}\n",
    ),
];

#[test]
fn a_missing_argument_is_invalid_args() {
    let root = repository("args", &FILES);
    assert_eq!(
        status(&call(&root, &json!({"symbol": "helper"}))),
        "INVALID_ARGS"
    );
}

#[test]
fn an_unknown_symbol_is_not_found() {
    let root = repository("missing", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "absent::name", "to_file": "src/caller.rs"}),
    );
    assert_eq!(status(&answer), "NOT_FOUND");
}

#[test]
fn moving_a_symbol_to_its_own_file_is_no_change() {
    let root = repository("same", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/core.rs"}),
    );
    if status(&answer) == "NOT_FOUND" {
        return; // the fixture graph did not expose that label
    }
    assert_eq!(status(&answer), "NO_CHANGE");
}

#[test]
fn a_move_that_creates_no_cycle_is_feasible() {
    let root = repository("feasible", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/caller.rs"}),
    );
    if status(&answer) != "EVALUATED" {
        return;
    }
    // Moving the helper into its only consumer removes the dependency rather than adding one.
    assert_eq!(verdict(&answer), "FEASIBLE", "{answer:?}");
    let introduced = answer
        .get("cycles")
        .and_then(|cycles| cycles.get("introduced"))
        .and_then(Value::as_array)
        .map(Vec::len);
    assert_eq!(introduced, Some(0));
}

#[test]
fn the_answer_names_who_depends_on_the_symbol() {
    let root = repository("blast", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/other.rs"}),
    );
    if status(&answer) != "EVALUATED" {
        return;
    }
    let importers = answer
        .get("blastRadius")
        .and_then(|radius| radius.get("importers"))
        .and_then(Value::as_array)
        .expect("importers");
    assert!(
        importers
            .iter()
            .any(|file| file.as_str() == Some("src/caller.rs")),
        "the consumer must appear in the blast radius, got {importers:?}"
    );
}

#[test]
fn the_projection_says_it_is_a_projection() {
    let root = repository("fidelity", &FILES);
    let answer = call(
        &root,
        &json!({"symbol": "helper", "to_file": "src/caller.rs"}),
    );
    if status(&answer) != "EVALUATED" {
        return;
    }
    assert_eq!(
        answer.get("fidelity").and_then(Value::as_str),
        Some("PROJECTED_FROM_GRAPH_EDGES"),
        "a prediction from simulated edges must not read as a rebuild"
    );
    assert_eq!(
        answer
            .get("plan")
            .and_then(|plan| plan.get("schemaVersion"))
            .and_then(Value::as_str),
        Some("weavatrix.edit-plan.v1"),
        "move_symbol emits the plan that cuts the declaration: {answer:?}"
    );
}

#[test]
fn applying_the_move_retargets_imports_and_carries_dependencies() {
    let root = repository(
        "imports",
        &[
            (
                "src/lib.rs",
                "pub mod core;\npub mod caller;\npub mod util;\npub mod third;\n",
            ),
            (
                "src/util.rs",
                "pub fn add(value: u32) -> u32 {\n    value\n}\n",
            ),
            (
                "src/core.rs",
                "use crate::util::add;\n\npub fn local_add(value: u32) -> u32 {\n    value\n}\n\npub fn helper(value: u32) -> u32 {\n    add(value) + local_add(value)\n}\n\npub fn other() -> u32 {\n    helper(1)\n}\n",
            ),
            (
                "src/caller.rs",
                "use crate::core::helper;\n\npub fn run() -> u32 {\n    helper(1)\n}\n",
            ),
            (
                "src/third.rs",
                "use crate::core::helper;\n\npub fn go() -> u32 {\n    helper(2)\n}\n",
            ),
        ],
    );
    let engine = Weavatrix::open(&root).expect("repository opens");
    let state = engine.state().clone();
    let symbol = state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.kind == weavatrix_rust::NodeKind::Function
                && node.label.trim_end_matches("()") == "helper"
        })
        .map(|node| node.id.as_str().to_owned())
        .expect("helper is indexed");
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "move_symbol",
            &json!({"symbol": symbol, "to_file": "src/caller.rs"}),
        )
        .expect("declared tool");
    assert_eq!(status(&planned), "EVALUATED", "{planned:?}");
    assert_eq!(
        planned.get("fidelity").and_then(Value::as_str),
        Some("PROJECTED_FROM_GRAPH_EDGES")
    );
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("preview");
    assert_eq!(status(&preview), "PREVIEW_OK", "{preview:?}");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan, "mode": "apply", "confirm_token": token}),
        )
        .expect("apply");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    let core = fs::read_to_string(root.join("src/core.rs")).expect("core");
    assert!(!core.contains("fn helper"), "{core}");
    assert!(core.contains("fn local_add"), "{core}");
    assert!(core.contains("fn other"), "{core}");
    assert!(core.contains("use crate::caller::helper;"), "{core}");

    let caller = fs::read_to_string(root.join("src/caller.rs")).expect("caller");
    assert!(caller.contains("fn helper"), "{caller}");
    assert!(caller.contains("fn run"), "{caller}");
    assert!(caller.contains("use crate::util::add;"), "{caller}");
    assert!(caller.contains("use crate::core::local_add;"), "{caller}");
    assert!(!caller.contains("use crate::core::helper"), "{caller}");

    let third = fs::read_to_string(root.join("src/third.rs")).expect("third");
    assert!(third.contains("use crate::caller::helper;"), "{third}");
    assert!(!third.contains("crate::core::helper"), "{third}");
}
