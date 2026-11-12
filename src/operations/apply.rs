//! The write path: preview, apply, roll back.
//!
//! The transaction itself belongs to `weavatrix-worktree` — journal, atomic replacement, crash
//! recovery and undo are its job, and reimplementing any of that here would be a second, weaker
//! copy of a thing that already works. What this module owns is the contract: which statuses an
//! agent sees, and the rule that a preview is the only way to get permission to write.

use std::path::{Component, Path, PathBuf};

use crate::envelope::read_envelope;
use crate::token::TokenStore;
use blazingly_json::{Value, json};
use weavatrix_refactor_plan::EditPlan;
use weavatrix_worktree::{
    DeleteFile, RenameFile, TransactionPhase, UndoRetention, Worktree, WorktreeDryRunReport,
    WorktreeError, WorktreeErrorCode, WorktreeOperation, WorktreePlan,
};

/// The transaction takes a plan of operations; a text edit plan is the all-`Modify` case of one.
///
/// Going through the retained-apply path rather than the plain one is what makes
/// `rollback_last_apply` possible at all: only that path keeps the previous contents.
fn as_worktree_plan(plan: &EditPlan) -> WorktreePlan {
    let renames = extension_array(plan, "renames");
    let deletes = extension_array(plan, "deletes");
    let sources = renames
        .iter()
        .filter_map(|rename| rename.get("from").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    let removed = deletes
        .iter()
        .filter_map(|delete| delete.get("path").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    let mut operations = Vec::new();
    for rename in renames {
        let (Some(from), Some(to), Some(hash)) = (
            rename.get("from").and_then(Value::as_str),
            rename.get("to").and_then(Value::as_str),
            rename.get("sha256").and_then(Value::as_str),
        ) else {
            continue;
        };
        let edits = plan
            .files
            .iter()
            .find(|file| file.path == from)
            .map(|file| file.edits.clone())
            .unwrap_or_default();
        operations.push(WorktreeOperation::Rename(
            RenameFile::new(from, to, hash).with_edits(edits),
        ));
    }
    for delete in deletes {
        let (Some(path), Some(hash)) = (
            delete.get("path").and_then(Value::as_str),
            delete.get("sha256").and_then(Value::as_str),
        ) else {
            continue;
        };
        operations.push(WorktreeOperation::Delete(DeleteFile::new(path, hash)));
    }
    operations.extend(plan.files.iter().filter_map(|file| {
        if sources.contains(&file.path) || removed.contains(&file.path) {
            None
        } else {
            Some(WorktreeOperation::Modify(file.clone()))
        }
    }));
    WorktreePlan::new(plan.operation.clone(), operations)
}

fn extension_array<'plan>(plan: &'plan EditPlan, key: &str) -> &'plan [Value] {
    plan.extensions
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn moves_paths(plan: &EditPlan) -> bool {
    plan.extensions.contains_key("renames") || plan.extensions.contains_key("deletes")
}

fn worktree(root: &std::path::Path) -> Result<Worktree, Value> {
    Worktree::open(root).map_err(|error| {
        json!({
            "status": "REPO_BUSY",
            "reason": format!("the repository could not be opened for writing: {error}"),
        })
    })
}

/// Turns a transaction failure into the contract status that describes it.
///
/// Matched on the error's own code rather than on its prose. Sniffing a message for "hash" is
/// how a stale file gets reported as an invalid plan the day someone rewords the error, and the
/// difference matters: a stale tree is fixed by previewing again, an invalid plan never is.
fn failure(error: &WorktreeError) -> Value {
    use WorktreeErrorCode as Code;
    let status = match error.code() {
        Code::SourceHashMismatch | Code::ConcurrentModification => "STALE",
        Code::RootBusy | Code::RecoveryRequired => "REPO_BUSY",
        Code::RollbackFailed | Code::JournalCorrupt => "ROLLBACK_INCOMPLETE",
        _ => "INVALID_PLAN",
    };
    json!({"status": status, "reason": error.to_string()})
}

/// Whether a failure means the working tree moved under the plan.
fn is_stale(error: &WorktreeError) -> bool {
    matches!(
        error.code(),
        WorktreeErrorCode::SourceHashMismatch | WorktreeErrorCode::ConcurrentModification
    )
}

/// Applies a plan that a producing operation has just recomputed from the current graph.
///
/// The caller never supplies this plan. It repeats the operation arguments and presents the
/// preview token; consuming that token against the recomputed envelope proves the apply still
/// names exactly what was previewed.
pub(super) fn apply_generated_plan(
    root: &std::path::Path,
    tokens: &TokenStore,
    plan_value: &Value,
    confirm_token: Option<&str>,
    write_allowed: bool,
) -> Value {
    if !write_allowed {
        return json!({
            "status": "WRITE_GATE_CLOSED",
            "reason": "the server was started without source edits enabled",
        });
    }
    let plan = match read_envelope(plan_value) {
        Ok(plan) => plan,
        Err(error) => {
            return json!({
                "status": error.code,
                "reason": error.reason,
            });
        }
    };
    if let Some(refusal) = tokens.consume(confirm_token, &plan, root) {
        return refusal;
    }
    let tree = match worktree(root) {
        Ok(tree) => tree,
        Err(refusal) => return refusal,
    };
    write_plan(root, &tree, &plan)
}

/// `apply_edit_plan`: preview verifies, apply writes.
///
/// An apply may present the confirmation alone. The preview already carried the plan to the
/// agent; making it echo the plan back is paying for the same bytes twice, the second time as
/// completion tokens. The token names exactly one previewed plan, so the server supplies it.
pub(super) fn apply_edit_plan(
    root: &std::path::Path,
    tokens: &TokenStore,
    arguments: &Value,
    write_allowed: bool,
) -> Value {
    let apply = arguments.get("mode").and_then(Value::as_str) == Some("apply");
    let presented = arguments.get("confirm_token").and_then(Value::as_str);
    if apply && arguments.get("plan").is_none() {
        if !write_allowed {
            return json!({
                "status": "WRITE_GATE_CLOSED",
                "reason": "the server was started without source edits enabled",
            });
        }
        let plan = match tokens.consume_for_plan(presented, root) {
            Ok(plan) => plan,
            Err(refusal) => return refusal,
        };
        let tree = match worktree(root) {
            Ok(tree) => tree,
            Err(refusal) => return refusal,
        };
        return write_plan(root, &tree, &plan);
    }
    let Some(plan_value) = arguments.get("plan") else {
        return super::invalid_args("apply_edit_plan", &["plan"]);
    };
    let plan = match read_envelope(plan_value) {
        Ok(plan) => plan,
        Err(error) => {
            return json!({
                "status": error.code,
                "reason": error.reason,
            });
        }
    };
    let tree = match worktree(root) {
        Ok(tree) => tree,
        Err(refusal) => return refusal,
    };

    if !apply {
        return match preview_plan(&tree, &plan) {
            Ok((files, total_edits)) => {
                let token = tokens.issue(&plan, root);
                json!({
                    "status": "PREVIEW_OK",
                    "files": files,
                    "totalEdits": total_edits,
                    "confirmToken": token.value,
                    "expiresAt": token.expires_at,
                    "next": "call apply_edit_plan again with the same plan, mode=\"apply\", and this \
                             confirm_token. The token is single-use and bound to this exact plan \
                             and repository.",
                })
            }
            // At preview time a moved tree is not an error state to recover from, it is the
            // answer: this plan cannot be applied, produce a fresh one.
            Err(error) if is_stale(&error) => json!({
                "status": "PREVIEW_BLOCKED",
                "reason": format!(
                    "the plan does not match the working tree, so nothing can be applied. \
                     Re-run the producing tool for a fresh plan: {error}"
                ),
            }),
            Err(error) => failure(&error),
        };
    }

    // Gate two is the environment, checked by the host before this runs. Gate three is the token.
    if !write_allowed {
        return json!({
            "status": "WRITE_GATE_CLOSED",
            "reason": "the server was started without source edits enabled",
        });
    }
    if let Some(refusal) = tokens.consume(presented, &plan, root) {
        return refusal;
    }
    write_plan(root, &tree, &plan)
}

/// Writes a plan whose gates have all been passed, retaining the previous contents.
///
/// `weavatrix-worktree` 0.2.1 opens every destination parent during validate, and a rename into
/// a directory that does not exist yet fails before the transaction starts. Those parents are
/// created here, after the write gates, and only as real directories under the repository.
/// Rollback restores the file and leaves the new directory in place.
fn write_plan(root: &Path, tree: &Worktree, plan: &EditPlan) -> Value {
    if let Err(refusal) = ensure_destination_parents(root, plan) {
        return refusal;
    }
    match tree.apply_plan_retained(&as_worktree_plan(plan), UndoRetention::default()) {
        Ok(report) => json!({
            "status": "APPLIED",
            "transactionId": report.apply().transaction_id(),
            "undoId": report.undo_id().to_string(),
            "files": plan.files.iter().map(|file| file.path.clone()).collect::<Vec<_>>(),
            "totalEdits": plan.files.iter().map(|file| file.edits.len()).sum::<usize>(),
            "next": "the previous contents are retained; rollback_last_apply restores them.",
        }),
        Err(error) => failure(&error),
    }
}

fn preview_plan(tree: &Worktree, plan: &EditPlan) -> Result<(Vec<Value>, usize), WorktreeError> {
    if moves_paths(plan) {
        return preview_moves(tree, plan);
    }
    tree.dry_run(plan).map(|report| {
        (
            report
                .files()
                .iter()
                .map(|file| {
                    json!({
                        "path": file.path(),
                        "status": if file.changed() { "READY" } else { "NO_CHANGE" },
                    })
                })
                .collect(),
            report.total_edits(),
        )
    })
}

fn preview_moves(tree: &Worktree, plan: &EditPlan) -> Result<(Vec<Value>, usize), WorktreeError> {
    match tree.dry_run_plan(&as_worktree_plan(plan)) {
        Ok(report) => Ok(operation_ready(&report)),
        Err(error) if destination_parent_missing(&error, plan) => preview_edits(tree, plan),
        Err(error) => Err(error),
    }
}

fn operation_ready(report: &WorktreeDryRunReport) -> (Vec<Value>, usize) {
    (
        report
            .files()
            .iter()
            .map(|file| {
                json!({
                    "path": file.destination_path().or_else(|| file.source_path()).unwrap_or(""),
                    "status": "READY",
                })
            })
            .collect(),
        report.total_edits(),
    )
}

/// The destination file is allowed to be absent. A missing parent directory is not, until apply
/// creates it, so preview checks the text edits and reports the destination as ready.
fn preview_edits(tree: &Worktree, plan: &EditPlan) -> Result<(Vec<Value>, usize), WorktreeError> {
    let (mut files, total) = if plan.files.is_empty() {
        (Vec::new(), 0)
    } else {
        let report = tree.dry_run(plan)?;
        let files = report
            .files()
            .iter()
            .map(|file| {
                json!({
                    "path": file.path(),
                    "status": if file.changed() { "READY" } else { "NO_CHANGE" },
                })
            })
            .collect();
        (files, report.total_edits())
    };
    for rename in extension_array(plan, "renames") {
        if let Some(to) = rename.get("to").and_then(Value::as_str) {
            files.push(json!({"path": to, "status": "READY"}));
        }
    }
    Ok((files, total))
}

fn destination_parent_missing(error: &WorktreeError, plan: &EditPlan) -> bool {
    if error.code() != WorktreeErrorCode::PathMissing || error.phase() != TransactionPhase::Validate
    {
        return false;
    }
    let Some(path) = error.path() else {
        return false;
    };
    extension_array(plan, "renames")
        .iter()
        .any(|rename| rename.get("to").and_then(Value::as_str) == Some(path))
}

fn ensure_destination_parents(root: &Path, plan: &EditPlan) -> Result<(), Value> {
    for rename in extension_array(plan, "renames") {
        let Some(to) = rename.get("to").and_then(Value::as_str) else {
            continue;
        };
        let Some(parent) = confined_parent(root, to)? else {
            continue;
        };
        reject_linked_ancestor(root, &parent)?;
        std::fs::create_dir_all(&parent).map_err(|error| {
            json!({
                "status": "INVALID_PLAN",
                "reason": format!("could not create the parent of {to}: {error}"),
            })
        })?;
    }
    Ok(())
}

fn confined_parent(root: &Path, to: &str) -> Result<Option<PathBuf>, Value> {
    let relative = Path::new(to);
    let escapes = relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if escapes {
        return Err(json!({
            "status": "INVALID_PLAN",
            "reason": format!("rename destination {to} escapes the repository"),
        }));
    }
    Ok(relative.parent().and_then(|parent| {
        if parent.as_os_str().is_empty() {
            None
        } else {
            Some(root.join(parent))
        }
    }))
}

fn reject_linked_ancestor(root: &Path, parent: &Path) -> Result<(), Value> {
    let relative = parent.strip_prefix(root).map_err(|_| {
        json!({
            "status": "INVALID_PLAN",
            "reason": "rename destination parent is outside the repository",
        })
    })?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        if cursor.is_symlink() {
            return Err(json!({
                "status": "INVALID_PLAN",
                "reason": format!("{} is a symbolic link", cursor.display()),
            }));
        }
        if !cursor.exists() {
            break;
        }
    }
    Ok(())
}

/// `rollback_last_apply`: restores the most recent retained transaction.
pub(super) fn rollback_last_apply(root: &std::path::Path, write_allowed: bool) -> Value {
    if !write_allowed {
        return json!({
            "status": "WRITE_GATE_CLOSED",
            "reason": "the server was started without source edits enabled",
        });
    }
    let tree = match worktree(root) {
        Ok(tree) => tree,
        Err(refusal) => return refusal,
    };
    let receipts = match tree.undo_receipts() {
        Ok(receipts) => receipts,
        Err(error) => return failure(&error),
    };
    let Some(latest) = receipts.last() else {
        return json!({
            "status": "NO_CHANGE",
            "reason": "no retained apply exists for this repository; there is nothing to roll back",
        });
    };
    match tree.rollback_undo(latest.id()) {
        Ok(report) => json!({
            "status": "ROLLED_BACK",
            "restored": report.restored_paths(),
            "rollbackTransactionId": report.rollback_transaction_id(),
            "next": "run verified_change phase=verify if the refactor is being retried.",
        }),
        Err(error) => {
            let text = error.to_string();
            json!({
                "status": "ROLLBACK_INCOMPLETE",
                "reason": format!(
                    "the retained contents are kept and the rollback can be retried: {text}"
                ),
            })
        }
    }
}
