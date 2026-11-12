//! Cascading deletion on a real tree.
//!
//! `exclusive` must leave every other route and every shared helper in the file.
//! `connected` may also remove the direct caller, and still must leave the unrelated route.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::{NodeKind, RepositoryState, Weavatrix};
use weavatrix_rust_refactor::operations::RefactorSession;

const SOURCE: &str = "\
export function health() {\n\
  return privateHealth();\n\
}\n\
function privateHealth() {\n\
  return \"ok\";\n\
}\n\
export function metrics() {\n\
  return sharedStamp();\n\
}\n\
function sharedStamp() {\n\
  return 1;\n\
}\n\
export function status() {\n\
  return health();\n\
}\n\
const app = {\n\
  get(path, handler) {\n\
    return handler;\n\
  },\n\
};\n\
app.get(\"/health\", health);\n\
app.get(\"/metrics\", metrics);\n\
";

const WITHOUT_METRICS: &str = "\
export function health() {\n\
  return privateHealth();\n\
}\n\
function privateHealth() {\n\
  return \"ok\";\n\
}\n\
export function status() {\n\
  return health();\n\
}\n\
const app = {\n\
  get(path, handler) {\n\
    return handler;\n\
  },\n\
};\n\
app.get(\"/health\", health);\n\
";

const WITHOUT_HEALTH: &str = "\
export function metrics() {\n\
  return sharedStamp();\n\
}\n\
function sharedStamp() {\n\
  return 1;\n\
}\n\
const app = {\n\
  get(path, handler) {\n\
    return handler;\n\
  },\n\
};\n\
app.get(\"/metrics\", metrics);\n\
";

fn repository(name: &str) -> PathBuf {
    repository_with(name, SOURCE)
}

fn repository_with(name: &str, source: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-closure-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("directories");
    fs::write(root.join("src/service.js"), source).expect("source");
    fs::write(
        root.join("package.json"),
        "{\"name\":\"fixture\",\"version\":\"0.0.0\",\"type\":\"module\"}\n",
    )
    .expect("manifest");
    root
}

fn status_of(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn symbol(state: &RepositoryState, name: &str) -> Option<String> {
    state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.label.trim_end_matches("()") == name
                && node
                    .span
                    .as_ref()
                    .is_some_and(|span| span.file == "src/service.js")
                && matches!(node.kind, NodeKind::Function | NodeKind::Method)
        })
        .map(|node| node.id.as_str().to_owned())
}

fn endpoint(state: &RepositoryState, path: &str) -> Option<String> {
    state
        .graph()
        .nodes()
        .iter()
        .find(|node| {
            node.kind == NodeKind::Endpoint
                && node
                    .label
                    .split_once(' ')
                    .is_some_and(|(_, route)| route == path)
        })
        .map(|node| node.id.as_str().to_owned())
}

fn labels(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("symbol").and_then(Value::as_str))
                .map(|label| label.trim_end_matches("()").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn open(root: &std::path::Path) -> Weavatrix {
    Weavatrix::open(root).expect("repository opens")
}

fn call(root: &std::path::Path, arguments: &Value) -> Value {
    let engine = open(root);
    RefactorSession::read_only()
        .call(engine.state(), "delete_closure", arguments)
        .expect("declared tool")
}

#[test]
fn a_missing_symbol_argument_is_invalid_args() {
    let root = repository("args");
    assert_eq!(status_of(&call(&root, &json!({}))), "INVALID_ARGS");
}

#[test]
fn an_unknown_scope_is_invalid_args() {
    let root = repository("scope");
    let engine = open(&root);
    let Some(id) = symbol(engine.state(), "metrics") else {
        return;
    };
    assert_eq!(
        status_of(&call(&root, &json!({"symbol": id, "scope": "everything"}))),
        "INVALID_ARGS"
    );
}

#[test]
fn exclusive_metrics_removes_its_private_helper_and_leaves_the_other_route() {
    let root = repository("exclusive-metrics");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = endpoint(&state, "/metrics").or_else(|| symbol(&state, "metrics")) else {
        panic!("metrics is missing from the graph");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let deleted = labels(&planned, "deleted");
    assert!(
        deleted.iter().any(|label| label == "metrics"),
        "the selected handler has to be in the closure: {planned:?}"
    );
    for left in ["health", "privateHealth", "status"] {
        assert!(
            !deleted.iter().any(|label| label == left),
            "{left} belongs to the other route: {planned:?}"
        );
    }
    let source = apply(&session, &state, &root, &planned);
    assert!(source.contains("function health("), "{source}");
    assert!(source.contains("function status("), "{source}");
    assert!(source.contains("function privateHealth("), "{source}");
    assert!(
        source.contains("app.get(\"/health\", health)"),
        "the other route must stay: {source}"
    );
    assert!(
        !source.contains("function metrics("),
        "the selected handler was not removed: {source}"
    );
    assert!(
        !source.contains("function sharedStamp("),
        "the helper only metrics uses was left behind: {source}"
    );
    assert_eq!(source, WITHOUT_METRICS, "{source}");
}

#[test]
fn exclusive_function_removes_the_one_route_that_names_it() {
    let root = repository("exclusive-function");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = symbol(&state, "metrics") else {
        panic!("metrics is missing from the graph");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let source = apply(&session, &state, &root, &planned);
    assert_eq!(source, WITHOUT_METRICS, "{source}");
}

#[test]
fn exclusive_endpoint_keeps_a_handler_another_function_calls() {
    let root = repository("exclusive-endpoint");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = endpoint(&state, "/health") else {
        panic!("health route is missing from the graph");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let deleted = labels(&planned, "deleted");
    assert!(
        deleted.iter().any(|label| label == "GET /health"),
        "{planned:?}"
    );
    assert!(
        !deleted.iter().any(|label| label == "health"),
        "{planned:?}"
    );
    let source = apply(&session, &state, &root, &planned);
    assert!(source.contains("function health("), "{source}");
    assert!(source.contains("function status("), "{source}");
    assert!(
        source.contains("app.get(\"/metrics\", metrics)"),
        "{source}"
    );
    assert!(!source.contains("app.get(\"/health\", health)"), "{source}");
    assert!(balanced(&source), "{source}");
}

#[test]
fn exclusive_health_refuses_because_status_still_calls_it() {
    let root = repository("exclusive-health");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = symbol(&state, "health") else {
        panic!("health is missing from the graph");
    };
    let answer = RefactorSession::read_only()
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&answer), "BLOCKED", "{answer:?}");
    assert!(answer.get("plan").is_none(), "{answer:?}");
    let outsiders = labels(&answer, "outsiders");
    assert!(
        outsiders.iter().any(|label| label == "status"),
        "status is the other contract that still uses health: {answer:?}"
    );
    let source = fs::read_to_string(root.join("src/service.js")).expect("source");
    assert_eq!(source, SOURCE);
}

#[test]
fn connected_health_removes_the_direct_caller_and_leaves_metrics() {
    let root = repository("connected-health");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = symbol(&state, "health") else {
        panic!("health is missing from the graph");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "connected"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    assert_eq!(
        planned.get("completeness").and_then(Value::as_str),
        Some("PARTIAL"),
        "{planned:?}"
    );
    let deleted = labels(&planned, "deleted");
    for gone in ["health", "status", "privateHealth"] {
        assert!(
            deleted.iter().any(|label| label == gone),
            "{gone} is part of the connected closure: {planned:?}"
        );
    }
    for left in ["metrics", "sharedStamp"] {
        assert!(
            !deleted.iter().any(|label| label == left),
            "{left} is a different route: {planned:?}"
        );
    }
    let source = apply(&session, &state, &root, &planned);
    assert!(!source.contains("function health("), "{source}");
    assert!(!source.contains("function status("), "{source}");
    assert!(!source.contains("function privateHealth("), "{source}");
    assert!(source.contains("function metrics("), "{source}");
    assert!(source.contains("function sharedStamp("), "{source}");
    assert!(
        source.contains("app.get(\"/metrics\", metrics)"),
        "the unrelated route must stay: {source}"
    );
    assert!(!source.contains("app.get(\"/health\", health)"), "{source}");
    assert_eq!(source, WITHOUT_HEALTH, "{source}");
}

#[test]
fn connected_endpoint_removes_the_handler_and_the_other_caller() {
    let root = repository("connected-endpoint");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = endpoint(&state, "/health") else {
        panic!("health route is missing from the graph");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "connected"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let deleted = labels(&planned, "deleted");
    for gone in ["health", "status", "privateHealth", "GET /health"] {
        assert!(
            deleted.iter().any(|label| label == gone),
            "{gone}: {planned:?}"
        );
    }
    let source = apply(&session, &state, &root, &planned);
    assert_eq!(source, WITHOUT_HEALTH, "{source}");
}

#[test]
fn exclusive_refuses_a_function_two_endpoints_hand_to() {
    let source = "\
export function health() {\n\
  return \"ok\";\n\
}\n\
const app = {\n\
  get(path, handler) {\n\
    return handler;\n\
  },\n\
};\n\
app.get(\"/health\", health);\n\
app.get(\"/ready\", health);\n\
";
    let root = repository_with("two-routes", source);
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = symbol(&state, "health") else {
        panic!("health is missing from the graph");
    };
    let answer = RefactorSession::read_only()
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&answer), "BLOCKED", "{answer:?}");
    assert!(answer.get("plan").is_none(), "{answer:?}");
    let outsiders = labels(&answer, "outsiders");
    assert!(
        outsiders.iter().any(|label| label.contains("/health")),
        "{answer:?}"
    );
    assert!(
        outsiders.iter().any(|label| label.contains("/ready")),
        "{answer:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/service.js")).expect("source"),
        source
    );

    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "connected"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let applied = apply(&session, &state, &root, &planned);
    assert!(!applied.contains("function health("), "{applied}");
    assert!(!applied.contains("/health"), "{applied}");
    assert!(!applied.contains("/ready"), "{applied}");
    assert!(applied.contains("get(path, handler)"), "{applied}");
    assert!(balanced(&applied), "{applied}");
}

#[test]
fn a_repository_is_never_deleted() {
    let root = repository("repository");
    let engine = open(&root);
    let Some(id) = engine
        .state()
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::Repository)
        .map(|node| node.id.as_str().to_owned())
    else {
        panic!("repository node is missing");
    };
    let answer = call(&root, &json!({"symbol": id, "scope": "exclusive"}));
    assert_eq!(status_of(&answer), "BLOCKED", "{answer:?}");
    assert!(
        answer
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason.contains("repository")),
        "{answer:?}"
    );
    assert!(root.join("src/service.js").is_file());
}

#[test]
fn a_package_without_a_span_is_not_a_symbol() {
    let root = repository_with(
        "package",
        "import leftPad from \"left-pad\";\nexport function health() { return leftPad(\"ok\"); }\n",
    );
    let engine = open(&root);
    let Some(id) = engine
        .state()
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::Package)
        .map(|node| node.id.as_str().to_owned())
    else {
        panic!(
            "package node is missing: {:?}",
            engine
                .state()
                .graph()
                .nodes()
                .iter()
                .map(|node| node.kind.as_str())
                .collect::<Vec<_>>()
        );
    };
    let answer = call(&root, &json!({"symbol": id}));
    assert_eq!(status_of(&answer), "NOT_A_SYMBOL", "{answer:?}");
}

#[test]
fn exclusive_file_delete_refuses_when_another_file_imports_it() {
    let root = repository("file-blocked");
    fs::write(
        root.join("src/orphan.js"),
        "export function orphan() { return 1; }\n",
    )
    .expect("orphan");
    fs::write(
        root.join("src/service.js"),
        "import { orphan } from \"./orphan.js\";\nexport function health() { return orphan(); }\n",
    )
    .expect("importer");
    let engine = open(&root);
    let Some(id) = file_node(engine.state(), "src/orphan.js") else {
        panic!("orphan file is missing");
    };
    let answer = call(&root, &json!({"symbol": id, "scope": "exclusive"}));
    assert_eq!(status_of(&answer), "BLOCKED", "{answer:?}");
    assert!(root.join("src/orphan.js").is_file());
}

#[test]
fn exclusive_file_delete_plans_an_unused_file() {
    let root = repository("file-unused");
    fs::write(
        root.join("src/orphan.js"),
        "export function orphan() { return 1; }\n",
    )
    .expect("orphan");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = file_node(&state, "src/orphan.js") else {
        panic!("orphan file is missing");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let paths = planned
        .get("plan")
        .and_then(|plan| plan.get("deletes"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("path").and_then(Value::as_str))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(paths, ["src/orphan.js"], "{planned:?}");
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("preview");
    assert_eq!(status_of(&preview), "PREVIEW_OK", "{preview:?}");
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
    assert_eq!(status_of(&applied), "APPLIED", "{applied:?}");
    assert!(!root.join("src/orphan.js").exists());
    assert!(root.join("src/service.js").is_file());
}

#[test]
fn a_handler_on_the_next_line_is_part_of_the_route() {
    let root = repository_with(
        "wrapped",
        "\
export function only() {\n\
  return 1;\n\
}\n\
const app = { get(path, handler) { return handler; } };\n\
app.get(\"/only\",\n\
  only);\n\
",
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = symbol(&state, "only") else {
        panic!("only is missing");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let source = apply(&session, &state, &root, &planned);
    assert!(!source.contains("function only("), "{source}");
    assert!(!source.contains("/only"), "{source}");
    assert!(balanced(&source), "{source}");
}

#[test]
fn a_trailing_comment_on_the_closer_is_removed_with_the_function() {
    let root = repository_with(
        "closer",
        "\
export function only() {\n\
  return 1;\n\
} // end\n\
",
    );
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = symbol(&state, "only") else {
        panic!("only is missing");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let source = apply(&session, &state, &root, &planned);
    assert!(!source.contains("function only("), "{source}");
    assert!(!source.contains("// end"), "{source}");
    assert!(balanced(&source), "{source}");
}

#[test]
fn a_unique_handler_in_another_file_is_deleted_with_the_route() {
    let root = repository_with(
        "cross-handler",
        "\
import { only } from \"./only.js\";\n\
const app = { get(path, handler) { return handler; } };\n\
app.get(\"/only\", only);\n\
export function other() { return 1; }\n\
",
    );
    fs::write(
        root.join("src/only.js"),
        "export function only() {\n  return 1;\n}\n",
    )
    .expect("handler");
    let engine = open(&root);
    let state = engine.state().clone();
    let Some(id) = endpoint(&state, "/only") else {
        panic!("the route is missing from the graph");
    };
    let session = RefactorSession::new(true);
    let planned = session
        .call(
            &state,
            "delete_closure",
            &json!({"symbol": id, "scope": "exclusive"}),
        )
        .expect("declared tool");
    assert_eq!(status_of(&planned), "PLANNED", "{planned:?}");
    let deleted = labels(&planned, "deleted");
    assert!(
        deleted.iter().any(|label| label == "only"),
        "the handler in the other file has to be in the closure: {planned:?}"
    );
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("preview");
    assert_eq!(status_of(&preview), "PREVIEW_OK", "{preview:?}");
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
    assert_eq!(status_of(&applied), "APPLIED", "{applied:?}");
    let service = fs::read_to_string(root.join("src/service.js")).expect("service");
    let handler = fs::read_to_string(root.join("src/only.js")).expect("handler");
    assert!(
        !service.contains("\"/only\""),
        "the route literal should be gone, got {service}"
    );
    assert!(service.contains("function other("), "{service}");
    assert!(
        !handler.contains("function only("),
        "the handler in the other file should be gone, got {handler}"
    );
}

fn file_node(state: &RepositoryState, path: &str) -> Option<String> {
    state
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::File && node.label == path)
        .map(|node| node.id.as_str().to_owned())
}

fn balanced(source: &str) -> bool {
    let mut depth = 0_i32;
    for character in source.chars() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn apply(
    session: &RefactorSession,
    state: &RepositoryState,
    root: &std::path::Path,
    planned: &Value,
) -> String {
    let plan = planned.get("plan").expect("plan").clone();
    let preview = session
        .call(state, "apply_edit_plan", &json!({"plan": plan.clone()}))
        .expect("declared tool");
    assert_eq!(
        status_of(&preview),
        "PREVIEW_OK",
        "the edited bytes did not match the files: {preview:?}"
    );
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
        .expect("declared tool");
    assert_eq!(status_of(&applied), "APPLIED", "{applied:?}");
    fs::read_to_string(root.join("src/service.js")).expect("file")
}
