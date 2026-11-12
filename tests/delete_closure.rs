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
