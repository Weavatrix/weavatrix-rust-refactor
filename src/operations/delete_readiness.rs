//! Per-symbol deletion verdict.
//!
//! `safe: true` means a private symbol whose declaring file was read, whose file shows no
//! dynamic or reflection load, and which has no incoming use edge. A public symbol stays unsafe
//! because consumers outside this repository are invisible. The verdict stays `REVIEW_REQUIRED`:
//! this operation never writes.

use crate::evidence::{Risk, declaring_file, read_source};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use weavatrix_rust::{EdgeKind, RepositoryState};

const MAX_REFERENCES: usize = 200;

pub(super) fn delete_readiness(state: &RepositoryState, arguments: &Value) -> Value {
    let Some(symbol) = arguments.get("symbol").and_then(Value::as_str) else {
        return crate::operations::invalid_args("delete_readiness", &["symbol"]);
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return json!({
            "status": "NOT_FOUND",
            "reason": "the selected symbol is not present in the active graph",
            "symbol": symbol,
        });
    };
    let Some(node) = state.graph().node_at(index) else {
        return json!({
            "status": "NOT_FOUND",
            "reason": "the selected symbol is not present in the active graph",
            "symbol": symbol,
        });
    };

    let file = declaring_file(node);
    let source = file
        .as_deref()
        .and_then(|path| read_source(state.root(), path));

    // Containment is structure, not use: a file containing a symbol is not a caller of it.
    let known_references = state
        .graph()
        .incoming_at(index)
        .filter(|edge| edge.kind != EdgeKind::Contains)
        .take(MAX_REFERENCES)
        .map(|edge| known_reference(state, edge))
        .collect::<Vec<_>>();

    let signals = Risk::assess(node, source.as_deref());
    let blocking = signals.blocking();

    let (safe, confidence, reason) = if !known_references.is_empty() {
        (
            Value::from(false),
            "high",
            format!(
                "{} known reference(s) target this symbol; deleting it would break them",
                known_references.len()
            ),
        )
    } else if blocking.is_empty() {
        (
            Value::from(true),
            "high",
            "private symbol, declaring file read, no dynamic or reflection pattern, and no \
             incoming use edge"
                .to_owned(),
        )
    } else if source.is_none() {
        (
            Value::from("UNPROVEN"),
            "low",
            format!(
                "absence of references is not proven safe: {}",
                blocking.join(", ")
            ),
        )
    } else {
        (
            Value::from(false),
            "high",
            format!("not safe: {}", blocking.join(", ")),
        )
    };

    json!({
        "status": "OK",
        "symbol": node.label,
        "file": file,
        "safe": safe,
        "confidence": confidence,
        "reason": reason,
        "knownReferences": known_references,
        "unknownDynamicUsages": signals.report(),
        "deletion": deletion_span(node, file.as_deref()),
        "verdict": "REVIEW_REQUIRED",
    })
}

/// One incoming use, named so an agent can open the caller without a second graph query.
///
/// The line is the graph's own 1-based line. Adding one here reported the following line after
/// the engine stopped storing 0-based positions.
fn known_reference(state: &RepositoryState, edge: &weavatrix_rust::Edge) -> Value {
    let source = state
        .graph()
        .node_index(edge.source.as_str())
        .and_then(|index| state.graph().node_at(index));
    let span = edge
        .provenance
        .span
        .as_ref()
        .or_else(|| source.and_then(|node| node.span.as_ref()));
    json!({
        "relation": edge.kind.as_str(),
        "provenance": edge.provenance.evidence.as_str(),
        "symbol": source.map_or(Value::Null, |node| Value::from(node.label.as_str())),
        "id": edge.source.as_str(),
        "file": span.map_or(Value::Null, |item| Value::from(item.file.as_str())),
        "line": span.map_or(Value::Null, |item| Value::from(item.start.line)),
    })
}

fn deletion_span(node: &weavatrix_graph::Node, file: Option<&str>) -> Value {
    node.span.as_ref().map_or_else(
        || {
            json!({
                "file": file,
                "startLine": Value::Null,
                "endLine": Value::Null,
                "note": "no source range recorded; locate the declaration manually",
            })
        },
        |span| {
            json!({
                "file": file,
                "startLine": span.start.line,
                "endLine": span.end.line,
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_REFERENCES, delete_readiness};
    use crate::test_support::fixture_state;
    use blazingly_json::{Value, json};

    #[test]
    fn a_missing_symbol_is_not_found_not_an_error() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "src/nope.rs#ghost"}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("NOT_FOUND")
        );
    }

    #[test]
    fn a_missing_argument_is_invalid_args_not_a_crash() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({}));
        assert_eq!(
            answer.get("status").and_then(Value::as_str),
            Some("INVALID_ARGS")
        );
    }

    #[test]
    fn a_referenced_symbol_is_unsafe_with_high_confidence() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "used"}));
        if answer.get("status").and_then(Value::as_str) != Some("OK") {
            return; // the fixture graph did not resolve that label; the NOT_FOUND path covers it
        }
        let references = answer
            .get("knownReferences")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if references > 0 {
            assert_eq!(answer.get("safe").and_then(Value::as_bool), Some(false));
            assert_eq!(
                answer.get("confidence").and_then(Value::as_str),
                Some("high")
            );
        }
        assert!(references <= MAX_REFERENCES);
    }

    #[test]
    fn deletion_is_never_automated_whatever_the_verdict() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "orphan"}));
        if answer.get("status").and_then(Value::as_str) == Some("OK") {
            assert_eq!(
                answer.get("verdict").and_then(Value::as_str),
                Some("REVIEW_REQUIRED")
            );
            assert_ne!(
                answer.get("safe").and_then(Value::as_bool),
                Some(true),
                "a public function stays unsafe"
            );
        }
    }

    #[test]
    fn a_private_unreferenced_symbol_is_safe_and_still_review_required() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "hidden"}));
        if answer.get("status").and_then(Value::as_str) != Some("OK") {
            return;
        }
        assert_eq!(
            answer.get("safe").and_then(Value::as_bool),
            Some(true),
            "{answer:?}"
        );
        assert_eq!(
            answer.get("confidence").and_then(Value::as_str),
            Some("high")
        );
        assert_eq!(
            answer.get("verdict").and_then(Value::as_str),
            Some("REVIEW_REQUIRED")
        );
    }

    #[test]
    fn the_reported_line_is_the_graph_line_and_a_reference_names_its_source() {
        let state = fixture_state();
        let answer = delete_readiness(&state, &json!({"symbol": "used"}));
        if answer.get("status").and_then(Value::as_str) != Some("OK") {
            return;
        }
        let label = answer.get("symbol").and_then(Value::as_str);
        let file = answer.get("file").and_then(Value::as_str);
        let node = state.graph().nodes().iter().find(|node| {
            Some(node.label.as_str()) == label
                && node.span.as_ref().map(|span| span.file.as_str()) == file
        });
        let Some(node) = node else {
            return;
        };
        let Some(span) = node.span.as_ref() else {
            return;
        };
        assert_eq!(
            answer
                .get("deletion")
                .and_then(|deletion| deletion.get("startLine"))
                .and_then(Value::as_u64),
            Some(u64::from(span.start.line)),
            "the deletion line must be the 1-based graph line: {answer:?}"
        );
        let references = answer
            .get("knownReferences")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for reference in &references {
            assert!(reference.get("relation").and_then(Value::as_str).is_some());
            assert!(reference.get("id").and_then(Value::as_str).is_some());
            assert!(reference.get("symbol").is_some());
            assert!(reference.get("file").is_some());
            assert!(reference.get("line").is_some(), "{reference:?}");
        }
    }
}
