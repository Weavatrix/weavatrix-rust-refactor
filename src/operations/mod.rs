//! The refactor operation surface.
//!
//! Dispatch is total and every arm is a native engine — all sixteen of the frozen tools answer
//! for themselves, and the match has no fallback arm, so adding a tool to the contract fails to
//! compile rather than quietly returning "not supported" at run time.
//!
//! A missing span is `NOT_A_SYMBOL`. An exclusive delete that would touch another contract is
//! `BLOCKED`. Neither answer is a missing tool.

mod apply;
mod bulk_replace;
mod change_signature;
mod delete_closure;
mod delete_imports;
mod delete_readiness;
mod edit_symbol;
mod edits;
mod extract_duplicate;
mod extract_window;
mod import_syntax;
mod journal;
mod move_file;
mod move_imports;
mod move_symbol;
mod organize_imports;
mod remove_duplicates;
mod rename_related;
mod rename_symbol;
mod rust_type;
mod share_method_body;
mod signature;
mod syntax_gate;
mod text_scan;

use crate::contract;
use crate::token::TokenStore;
use blazingly_json::{Value, json};
use std::path::Path;
use weavatrix_rust::RepositoryState;

/// One server's refactor surface: the confirmations it has issued and whether it may write.
///
/// The write gate lives here rather than at each call site so there is exactly one place that
/// decides it, and it is fixed when the session is created — a gate re-read per call could be
/// changed underneath a running server.
pub struct RefactorSession {
    tokens: TokenStore,
    write_allowed: bool,
}

impl RefactorSession {
    /// Opens a session. `write_allowed` is the environment gate, already decided by the host.
    #[must_use]
    pub fn new(write_allowed: bool) -> Self {
        Self {
            tokens: TokenStore::default(),
            write_allowed,
        }
    }

    /// A session that will never write, for callers that only plan.
    ///
    /// Named rather than a bare `new(false)` so a caller that meant to pass a real gate cannot
    /// silently get a closed one — which is exactly the bug this replaced.
    #[must_use]
    pub fn read_only() -> Self {
        Self::new(false)
    }

    /// Calls one refactor operation.
    ///
    /// The repository lock is taken before the operation reads or writes. Readers share it. A
    /// call that will apply or roll back takes it alone when the write gate is open. A
    /// conflicting lock on that same repository is `REPO_BUSY` and does not spend a confirm
    /// token. Any other repository runs at the same time.
    ///
    /// # Errors
    ///
    /// Returns an error only when `name` is not a tool the contract declares. Every other
    /// refusal is a value carrying a contract status, because an agent branches on statuses and
    /// cannot branch on a transport error.
    pub fn call(
        &self,
        state: &RepositoryState,
        name: &str,
        arguments: &Value,
    ) -> Result<Value, String> {
        let Some(operation) = Operation::from_name(name) else {
            return Err(format!("unknown refactor operation: {name}"));
        };
        // The host may already hold this lock across the graph refresh. Taking it again on
        // the same thread reenters; it does not refuse the call that owns the lock.
        let _guard = match repository_guard(state.root(), name, arguments, self.write_allowed) {
            Ok(guard) => guard,
            Err(refusal) => return Ok(refusal),
        };
        Ok(match operation {
            Operation::DeleteReadiness => delete_readiness::delete_readiness(state, arguments),
            Operation::EditSymbol => edit_symbol::edit_symbol(state, arguments),
            Operation::BulkReplace => bulk_replace::bulk_replace(state, arguments),
            Operation::MoveSymbol => move_symbol::move_symbol(state, arguments),
            Operation::MoveFile => move_file::move_file(state, arguments),
            Operation::OrganizeImports => organize_imports::organize_imports(state, arguments),
            Operation::RenameSymbol => {
                rename_symbol::rename_symbol(state, &self.tokens, arguments, self.write_allowed)
            }
            Operation::RenameRelatedSymbols => rename_related::rename_related_symbols(
                state,
                &self.tokens,
                arguments,
                self.write_allowed,
            ),
            Operation::ApplyEditPlan => {
                apply::apply_edit_plan(state.root(), &self.tokens, arguments, self.write_allowed)
            }
            Operation::RollbackLastApply => {
                apply::rollback_last_apply(state.root(), self.write_allowed)
            }
            Operation::ChangeSignature => change_signature::change_signature(state, arguments),
            Operation::DeleteClosure => delete_closure::delete_closure(state, arguments),
            Operation::RemoveDuplicates => remove_duplicates::remove_duplicates(state, arguments),
            Operation::ExtractDuplicate => extract_duplicate::extract_duplicate(state, arguments),
            Operation::ExtractWindow => extract_window::extract_window(state, arguments),
            Operation::ShareMethodBody => share_method_body::share_method_body(state, arguments),
        })
    }
}

/// A refactor operation, resolved from a tool name the contract declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    RenameSymbol,
    RenameRelatedSymbols,
    ApplyEditPlan,
    RollbackLastApply,
    ChangeSignature,
    EditSymbol,
    BulkReplace,
    OrganizeImports,
    MoveFile,
    MoveSymbol,
    DeleteReadiness,
    DeleteClosure,
    RemoveDuplicates,
    ExtractDuplicate,
    ExtractWindow,
    ShareMethodBody,
}

impl Operation {
    /// Resolves a tool name, or `None` when the contract does not declare it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rename_symbol" => Some(Self::RenameSymbol),
            "rename_related_symbols" => Some(Self::RenameRelatedSymbols),
            "apply_edit_plan" => Some(Self::ApplyEditPlan),
            "rollback_last_apply" => Some(Self::RollbackLastApply),
            "change_signature" => Some(Self::ChangeSignature),
            "edit_symbol" => Some(Self::EditSymbol),
            "bulk_replace" => Some(Self::BulkReplace),
            "organize_imports" => Some(Self::OrganizeImports),
            "move_file" => Some(Self::MoveFile),
            "move_symbol" => Some(Self::MoveSymbol),
            "delete_readiness" => Some(Self::DeleteReadiness),
            "delete_closure" => Some(Self::DeleteClosure),
            "remove_duplicates" => Some(Self::RemoveDuplicates),
            "extract_duplicate" => Some(Self::ExtractDuplicate),
            "extract_window" => Some(Self::ExtractWindow),
            "share_method_body" => Some(Self::ShareMethodBody),
            _ => None,
        }
    }

    /// Whether the operation can write to the repository once every gate is satisfied.
    ///
    /// The plan producers are reads. Only these four ever touch a file, and only behind the
    /// environment gate plus a plan-bound single-use token.
    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(
            self,
            Self::RenameSymbol
                | Self::RenameRelatedSymbols
                | Self::ApplyEditPlan
                | Self::RollbackLastApply
        )
    }

    /// The tool name this operation answers to.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RenameSymbol => "rename_symbol",
            Self::RenameRelatedSymbols => "rename_related_symbols",
            Self::ApplyEditPlan => "apply_edit_plan",
            Self::RollbackLastApply => "rollback_last_apply",
            Self::ChangeSignature => "change_signature",
            Self::EditSymbol => "edit_symbol",
            Self::BulkReplace => "bulk_replace",
            Self::OrganizeImports => "organize_imports",
            Self::MoveFile => "move_file",
            Self::MoveSymbol => "move_symbol",
            Self::DeleteReadiness => "delete_readiness",
            Self::DeleteClosure => "delete_closure",
            Self::RemoveDuplicates => "remove_duplicates",
            Self::ExtractDuplicate => "extract_duplicate",
            Self::ExtractWindow => "extract_window",
            Self::ShareMethodBody => "share_method_body",
        }
    }
}

/// Holds one repository while a call reads or writes it.
///
/// Dropping the guard releases the lock. The same thread may acquire it again; another
/// thread or process on that same repository gets `REPO_BUSY` instead of waiting. Any other
/// repository runs at the same time.
#[must_use = "dropping the guard releases the repository"]
pub struct RepositoryGuard {
    _lock: journal::RepoLock,
}

/// Locks `root` for one refactor call.
///
/// A shared lock covers planning and preview. An exclusive lock covers `rollback_last_apply`
/// and any other writing tool whose `mode` is `"apply"`, and only when `write_allowed` is
/// set. A closed gate cannot write, so that call stays a reader.
///
/// # Errors
///
/// Returns `REPO_BUSY` when another process holds a conflicting lock on this repository,
/// when this thread is already inside a different repository, or when a read on this thread
/// would have to become a write.
pub fn repository_guard(
    root: &Path,
    name: &str,
    arguments: &Value,
    write_allowed: bool,
) -> Result<RepositoryGuard, Value> {
    let lock = journal::acquire(root, write_allowed && intends_to_write(name, arguments))?;
    Ok(RepositoryGuard { _lock: lock })
}

/// Whether this call will change the tree when the write gate is open.
///
/// Preview uses the same tools as apply. The mode is what distinguishes them. Rollback has
/// no mode and always intends to write.
fn intends_to_write(name: &str, arguments: &Value) -> bool {
    match Operation::from_name(name) {
        Some(Operation::RollbackLastApply) => true,
        Some(operation) if operation.writes() => {
            arguments.get("mode").and_then(Value::as_str) == Some("apply")
        }
        _ => false,
    }
}

/// The tool catalog, taken from the frozen contract.
#[must_use]
pub fn catalog() -> Value {
    contract::catalog_value().clone()
}

/// Names of every tool this crate exposes.
#[must_use]
pub fn catalog_names() -> Vec<String> {
    contract::tools()
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

/// Calls one refactor operation.
///
/// # Errors
///
/// Returns an error only when `name` is not a tool the contract declares. Every other refusal
/// is a value carrying a contract status, because an agent branches on statuses and cannot
/// branch on a transport error.
pub fn call(state: &RepositoryState, name: &str, arguments: &Value) -> Result<Value, String> {
    RefactorSession::read_only().call(state, name, arguments)
}

/// The symbol an agent named is not in the graph, with every id it could have meant.
///
/// The candidates ride in the refusal because without them the refusal costs a round trip: the
/// agent has to run a graph query — measured at ~26 KB — to learn ids the resolver already saw.
pub(crate) fn not_found(graph: &weavatrix_graph::Graph, symbol: &str) -> Value {
    let candidates = crate::resolve::candidate_ids(graph, symbol);
    json!({
        "status": "NOT_FOUND",
        "reason": if candidates.len() > 1 {
            "the name matches more than one symbol; pass one of the candidate ids"
        } else {
            "the selected symbol is not present in the active graph; pass an exact id"
        },
        "symbol": symbol,
        "candidates": candidates,
    })
}

/// The graph and the file no longer agree, so no range from it can be trusted.
pub(crate) fn stale_graph(file: &str) -> Value {
    json!({
        "status": "STALE_GRAPH",
        "reason": format!(
            "{file}: the recorded source range no longer matches the file. Rebuild the graph; \
             nothing was planned from a range that cannot be located."
        ),
    })
}

/// A missing or wrongly typed argument, named rather than described.
///
/// The engines below state their preconditions by returning this, never by panicking: an agent
/// branches on a status and cannot branch on a crash.
pub(crate) fn invalid_args(operation: &str, missing: &[&str]) -> Value {
    json!({
        "status": "INVALID_ARGS",
        "operation": operation,
        "invalid": missing,
        "reason": format!(
            "missing or invalid required argument(s): {}. Nothing was planned or written.",
            missing.join(", ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::{Operation, call, catalog, catalog_names};
    use crate::contract;

    #[test]
    fn every_contract_tool_resolves_to_an_operation() {
        for tool in contract::tools() {
            assert!(
                Operation::from_name(&tool.name).is_some(),
                "{} is in the contract with no operation arm",
                tool.name
            );
        }
    }

    #[test]
    fn no_operation_exists_outside_the_contract() {
        for name in catalog_names() {
            assert!(contract::declares(&name));
        }
        assert_eq!(catalog_names().len(), contract::tools().len());
    }

    #[test]
    fn exactly_four_operations_can_write() {
        let writers = contract::tools()
            .iter()
            .filter_map(|tool| Operation::from_name(&tool.name))
            .filter(|operation| operation.writes())
            .count();
        assert_eq!(writers, 4);
    }

    #[test]
    fn every_operation_answers_with_a_contract_status_ported_or_not() {
        let state = crate::test_support::fixture_state();
        for tool in contract::tools() {
            let answer =
                call(&state, &tool.name, &blazingly_json::json!({})).expect("declared tool");
            let status = answer
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            assert!(
                contract::permits_state(status),
                "{} answered {status}, which is outside the contract",
                tool.name
            );
        }
    }

    #[test]
    fn an_undeclared_tool_is_an_error_not_a_status() {
        let state = crate::test_support::fixture_state();
        assert!(call(&state, "reformat_universe", &blazingly_json::json!({})).is_err());
    }

    #[test]
    fn the_catalog_matches_the_contract() {
        assert_eq!(
            catalog().as_array().map(Vec::len),
            Some(contract::tools().len())
        );
    }
}
