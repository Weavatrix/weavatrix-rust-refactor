//! The write path against a real working tree: preview, apply, roll back.
//!
//! These run on files on disk rather than a mocked transaction, because every property worth
//! asserting here is about what ends up in the tree — that a closed gate leaves it untouched,
//! that a spent confirmation cannot write twice, and that a rollback restores the exact bytes.

use blazingly_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use weavatrix_rust::Weavatrix;
use weavatrix_rust_refactor::operations::RefactorSession;

const ORIGINAL: &str = "pub fn one() -> u32 {\n    1\n}\n";

fn repository(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("wvxr-write-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("repository");
    fs::write(root.join("src/lib.rs"), ORIGINAL).expect("source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    root
}

fn sha256_of(text: &str) -> String {
    // The plan carries the hash the applier will check, so the test computes it the same way the
    // producer would rather than hard-coding a constant that would rot with the fixture.
    weavatrix_worktree::Sha256Hash::compute(text.as_bytes()).to_string()
}

fn plan(root: &std::path::Path) -> Value {
    let contents = fs::read_to_string(root.join("src/lib.rs")).expect("source");
    json!({
        "schemaVersion": "weavatrix.edit-plan.v1",
        "operation": "rename_symbol",
        "files": [{
            "path": "src/lib.rs",
            "sha256": sha256_of(&contents),
            "edits": [{
                "startLine": 1, "startChar": 7, "endLine": 1, "endChar": 10,
                "before": "one", "after": "uno", "provenance": "EXACT_LSP",
            }],
        }],
    })
}

fn status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn session_for(
    root: &std::path::Path,
    write_allowed: bool,
) -> (RefactorSession, weavatrix_rust::RepositoryState) {
    let engine = Weavatrix::open(root).expect("repository opens");
    (RefactorSession::new(write_allowed), engine.state().clone())
}

#[test]
fn a_closed_gate_leaves_the_working_tree_untouched() {
    let root = repository("gate");
    let (session, state) = session_for(&root, false);
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan(&root)}))
        .expect("declared tool");
    assert_eq!(
        status(&preview),
        "PREVIEW_OK",
        "preview must not need the gate"
    );

    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token");
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({
                "plan": plan(&root), "mode": "apply", "confirm_token": token,
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "WRITE_GATE_CLOSED");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
}

#[test]
fn preview_then_apply_writes_exactly_the_planned_bytes_and_rolls_back() {
    let root = repository("cycle");
    let (session, state) = session_for(&root, true);
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan(&root)}))
        .expect("declared tool");
    assert_eq!(status(&preview), "PREVIEW_OK");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();

    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({
                "plan": plan(&root), "mode": "apply", "confirm_token": token,
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn uno() -> u32 {\n    1\n}\n"
    );

    let rolled = session
        .call(&state, "rollback_last_apply", &json!({}))
        .expect("declared tool");
    assert_eq!(status(&rolled), "ROLLED_BACK", "{rolled:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL,
        "rollback must restore the exact original bytes"
    );
}

#[test]
fn a_confirmation_cannot_write_twice() {
    let root = repository("replay");
    let (session, state) = session_for(&root, true);
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan(&root)}))
        .expect("declared tool");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let first = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({
                "plan": plan(&root), "mode": "apply", "confirm_token": token.clone(),
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&first), "APPLIED");

    let after = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let replay = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({
                "plan": plan(&root), "mode": "apply", "confirm_token": token,
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&replay), "TOKEN_UNKNOWN");
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), after);
}

#[test]
fn applying_without_a_confirmation_writes_nothing() {
    let root = repository("no-token");
    let (session, state) = session_for(&root, true);
    let refused = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan(&root), "mode": "apply"}),
        )
        .expect("declared tool");
    assert_eq!(status(&refused), "TOKEN_UNKNOWN");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
}

#[test]
fn a_stale_plan_is_blocked_at_preview() {
    let root = repository("stale");
    let planned_before_drift = plan(&root);
    fs::write(root.join("src/lib.rs"), "pub fn one() -> u32 {\n    2\n}\n").expect("drift");
    let (session, state) = session_for(&root, true);
    let preview = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": planned_before_drift}),
        )
        .expect("declared tool");
    assert_eq!(status(&preview), "PREVIEW_BLOCKED", "{preview:?}");
}

#[test]
fn an_inferred_edit_never_reaches_the_tree() {
    let root = repository("inferred");
    let mut unproven = plan(&root);
    if let Some(edit) = unproven
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .and_then(|files| files.first_mut())
        .and_then(|file| file.get_mut("edits"))
        .and_then(Value::as_array_mut)
        .and_then(|edits| edits.first_mut())
        .and_then(Value::as_object_mut)
    {
        edit.insert("provenance".to_owned(), "INFERRED".into());
    }
    let (session, state) = session_for(&root, true);
    let refused = session
        .call(&state, "apply_edit_plan", &json!({"plan": unproven}))
        .expect("declared tool");
    assert_eq!(status(&refused), "INVALID_PLAN");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
}

#[test]
fn a_rename_missing_its_hash_refuses_the_whole_plan() {
    let root = repository("broken-rename");
    let contents = fs::read_to_string(root.join("src/lib.rs")).expect("source");
    let (session, state) = session_for(&root, true);
    let broken = json!({
        "schemaVersion": "weavatrix.edit-plan.v1",
        "operation": "move_file",
        "files": [{
            "path": "src/lib.rs",
            "sha256": sha256_of(&contents),
            "edits": [{
                "startLine": 1, "startChar": 7, "endLine": 1, "endChar": 10,
                "before": "one", "after": "uno", "provenance": "EXACT_LSP",
            }],
        }],
        "renames": [{"from": "src/lib.rs", "to": "src/gone/lib.rs"}],
    });
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": broken}))
        .expect("declared tool");
    assert_eq!(status(&preview), "INVALID_PLAN", "{preview:?}");
    let reason = preview.get("reason").and_then(Value::as_str).unwrap_or("");
    assert!(reason.contains("from, to, or sha256"), "{preview:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
    assert!(!root.join("src/gone").exists());

    let deleted = json!({
        "schemaVersion": "weavatrix.edit-plan.v1",
        "operation": "delete_closure",
        "files": [{
            "path": "src/lib.rs",
            "sha256": sha256_of(&contents),
            "edits": [{
                "startLine": 1, "startChar": 7, "endLine": 1, "endChar": 10,
                "before": "one", "after": "uno", "provenance": "EXACT_LSP",
            }],
        }],
        "deletes": [{"path": "src/lib.rs"}],
    });
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": deleted}))
        .expect("declared tool");
    assert_eq!(status(&preview), "INVALID_PLAN", "{preview:?}");
    let reason = preview.get("reason").and_then(Value::as_str).unwrap_or("");
    assert!(reason.contains("path or sha256"), "{preview:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
}

#[test]
fn rollback_undoes_the_newest_apply_and_then_the_one_before_it() {
    let root = repository("stack");
    let (session, state) = session_for(&root, true);
    let first = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan(&root)}))
        .expect("declared tool");
    let token = first
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": plan(&root), "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");

    // A later apply of the same path invalidates the older receipt. The second
    // apply therefore edits a different file, which is the stack the worktree can roll.
    let other = "pub fn two() -> u32 {\n    2\n}\n";
    fs::write(root.join("src/other.rs"), other).expect("other");
    let second_plan = json!({
        "schemaVersion": "weavatrix.edit-plan.v1",
        "operation": "rename_symbol",
        "files": [{
            "path": "src/other.rs",
            "sha256": sha256_of(other),
            "edits": [{
                "startLine": 1, "startChar": 7, "endLine": 1, "endChar": 10,
                "before": "two", "after": "dos", "provenance": "EXACT_LSP",
            }],
        }],
    });
    let second = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": second_plan.clone()}),
        )
        .expect("declared tool");
    let token = second
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();
    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({"plan": second_plan, "mode": "apply", "confirm_token": token}),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");
    assert!(
        fs::read_to_string(root.join("src/other.rs"))
            .unwrap()
            .contains("dos")
    );

    let rolled = session
        .call(&state, "rollback_last_apply", &json!({}))
        .expect("declared tool");
    assert_eq!(status(&rolled), "ROLLED_BACK", "{rolled:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/other.rs")).unwrap(),
        other,
        "the newest apply rolls back first"
    );
    assert!(
        fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("uno")
    );

    let rolled = session
        .call(&state, "rollback_last_apply", &json!({}))
        .expect("declared tool");
    assert_eq!(status(&rolled), "ROLLED_BACK", "{rolled:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
}

#[test]
fn a_busy_repository_keeps_the_confirm_token() {
    let root = repository("busy");
    let (session, state) = session_for(&root, true);
    let preview = session
        .call(&state, "apply_edit_plan", &json!({"plan": plan(&root)}))
        .expect("declared tool");
    assert_eq!(status(&preview), "PREVIEW_OK", "{preview:?}");
    let token = preview
        .get("confirmToken")
        .and_then(Value::as_str)
        .expect("token")
        .to_owned();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let probe = root.clone();
    let holder = std::thread::spawn(move || {
        let guard = weavatrix_rust_refactor::operations::repository_guard(
            &probe,
            "rollback_last_apply",
            &json!({}),
            true,
        )
        .expect("holder");
        ready_tx.send(()).expect("ready");
        release_rx.recv().expect("release");
        drop(guard);
    });
    ready_rx.recv().expect("held");
    let busy = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({
                "plan": plan(&root),
                "mode": "apply",
                "confirm_token": token.clone(),
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&busy), "REPO_BUSY", "{busy:?}");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        ORIGINAL
    );
    release_tx.send(()).expect("release");
    holder.join().expect("holder");

    let applied = session
        .call(
            &state,
            "apply_edit_plan",
            &json!({
                "plan": plan(&root),
                "mode": "apply",
                "confirm_token": token,
            }),
        )
        .expect("declared tool");
    assert_eq!(status(&applied), "APPLIED", "{applied:?}");
    assert!(
        fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("uno")
    );
}

#[test]
fn rolling_back_with_nothing_applied_is_a_status_not_a_failure() {
    let root = repository("empty-undo");
    let (session, state) = session_for(&root, true);
    let rolled = session
        .call(&state, "rollback_last_apply", &json!({}))
        .expect("declared tool");
    assert_eq!(status(&rolled), "NO_CHANGE", "{rolled:?}");
}
