# weavatrix-rust-refactor

[![CI](https://github.com/Weavatrix/weavatrix-rust-refactor/actions/workflows/ci.yml/badge.svg)](https://github.com/Weavatrix/weavatrix-rust-refactor/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/weavatrix-rust-refactor.svg)](https://crates.io/crates/weavatrix-rust-refactor)
[![docs.rs](https://docs.rs/weavatrix-rust-refactor/badge.svg)](https://docs.rs/weavatrix-rust-refactor)
[![MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

The protocol-independent refactor operation layer of the [Weavatrix ecosystem](https://weavatrix.com/ecosystem).

Evidence-backed refactor operations for Weavatrix: plan production, transactional
application, and rollback.

This crate is to [`weavatrix-rust`](https://github.com/Weavatrix/weavatrix-rust)
what a writer is to a reader. It consumes the read-only evidence graph, produces
`weavatrix.edit-plan.v1` envelopes, and applies them through
[`weavatrix-worktree`](https://github.com/Weavatrix/weavatrix-worktree)'s
crash-recoverable transaction. It owns no protocol: the MCP host
[`weavatrix-refactor`](https://github.com/Weavatrix/weavatrix-refactor)
composes this catalog with the read-only one.

```text
weavatrix-rust            reusable read-only evidence engine
        |
        +-- weavatrix             read-only MCP host
        |
        +-- weavatrix-rust-refactor   refactor operations  (this crate)
                    |
                    +-- weavatrix-refactor   refactor MCP host
```

Built on the existing refactor kernel rather than a reimplementation of it:
`weavatrix-worktree` for the transaction and its journal, `weavatrix-refactor-plan`
for evidence metadata and canonical fingerprints, `weavatrix-edit` for
Unicode-safe edit-plan validation and application.

## The contract is frozen, not re-derived

The tool names, their JSON schemas and the result states live in
[`contract/refactor-tools.v1.json`](contract/refactor-tools.v1.json). Contract
version 2 adds `delete_closure` and keeps the version 1 result states. That file
is compiled into the crate and is the **only** source of the tool catalog, so this
implementation cannot drift from the schemas agents already depend on.

An operation is conformant when it answers with a status from that file — never a
new one, never a renamed one. Tests enforce both directions: every contract tool
has an operation arm, and no operation exists outside the contract.

Changing a name, a schema or a status means changing the contract file, and that
is a contract-version decision rather than an implementation detail.

## Operations

All twelve are native. Dispatch has no fallback arm, so a tool added to the
contract fails to compile rather than answering "not supported" at run time.

| Operation | Backend | What it proves |
| --- | --- | --- |
| `rename_symbol` | graph + lexical | The declaration, every site the graph proves calls or references it, and the import lines of files the graph proves import the declaring file |
| `rename_related_symbols` | the same, merged | One transaction for several renames, with collision and contested-site detection; chains and swaps apply simultaneously rather than in sequence |
| `change_signature` | graph + token-split lists | A parameter added or removed in any indexed language, including Kotlin read with the Java tokenizer. A typed language copies the previous parameter's type. With a default, existing calls stay. Without one, proven calls are edited when a call-site value is given. A call the graph missed is edited only when the name is declared once and its argument list can be read; otherwise it is listed. The result stays `PARTIAL` |
| `organize_imports` | occurrence count | Named bindings whose identifier occurs once. Rust snake_case, Python from-imports and aliases, Go path segments, Java imports, C# usings, C++ using-names, and Kotlin imports are counted. Traits, `pub use`, bare Python imports, Go blank or dot imports, star imports, C++ `#include`, and `using namespace` stay |
| `edit_symbol` | declaration range | A symbol-anchored replacement or insertion. The end is the matching brace, the indent block, or the signature line. A signature-line end is `PARTIAL` and warns `DECLARATION_END_UNPROVEN`. The plan warns that syntax was not checked |
| `bulk_replace` | lexical | Literal or regex matches in indexed code. Comments and strings are included only with `include_trivia`. An extension the tokenizer does not know still skips `//` comments, block comments, quotes, and a `#` that starts a line |
| `move_file` | specifier arithmetic | The filesystem rename and the relative import specifiers, in one transaction. A missing parent directory is created at apply and removed on rollback when it is empty. A non-relative specifier whose last segment is the moved file is reported and the plan stays partial |
| `move_symbol` | graph | A cycle review plus an edit plan that moves the declaration, retargets the imports it can prove, and copies the imports the declaration uses. The status stays `EVALUATED` |
| `delete_readiness` | graph | Whether anything still depends on the symbol. A private symbol with no incoming use can be `safe: true`. The verdict stays `REVIEW_REQUIRED` and nothing is written |
| `delete_closure` | graph | A deletion plan for a symbol or endpoint and the callees only it uses. `exclusive` keeps a shared handler and every other route; `connected` also takes direct referrers |
| `apply_edit_plan` | `weavatrix-worktree` | Preview, single-use token, atomic write with retained contents. A rename missing `from`, `to`, or `sha256`, or a delete missing `path` or `sha256`, refuses the whole plan as `INVALID_PLAN` |
| `rollback_last_apply` | `weavatrix-worktree` | The newest retained apply, restored. An empty directory created for a rename is removed. A later edit of the same path invalidates the older receipt |

`rename_symbol` and `rename_related_symbols` own their complete two-phase workflow: preview
returns a plan-bound confirmation token, and repeating the same operation with identical rename
arguments, `mode="apply"`, and that token applies the recomputed plan. The agent never has to
echo the edit plan into `apply_edit_plan`; that generic tool remains available for plans produced
by the other operations or by an external planner.

### What "PARTIAL" means here

A rename never claims `COMPLETE`. Every call site comes from a graph edge, so these
operations prove the sites they edit — they cannot prove the *absence* of other
references. A same-named occurrence the graph does not vouch for is reported as an
`UNPROVEN_OCCURRENCE` rather than edited, which is the whole difference between
renaming a symbol and find-replacing a string.

`NOT_SUPPORTED` is no longer the answer for a whole tool. `delete_closure` will
not delete a repository (`BLOCKED`). A file nothing else uses is a filesystem
delete; exclusive mode returns `BLOCKED` when another file still imports it.
A module or package with no source span, and any other symbol with no source
span, is `NOT_A_SYMBOL`. A handler named on the following line of an unclosed
`(` is part of that route, including a uniquely named handler in another file.
A closer line may end with a `//` or `/* */` comment. A route label is renamed
as a path literal. A missing span on a rename is `NOT_A_SYMBOL`.

`organize_imports` counts a name and removes it only when that count proves the
import is unused. `use std::io::Write` stays: a CamelCase Rust import can be a
trait used through its methods. `pub use`, a bare Python `import`, a Go blank
or dot import, a star import, a C++ `#include`, and a `using namespace` stay
for the same reason. A snake_case Rust `use`, a Python `from` import, a Go path
segment, a Java import, a C# `using`, a C++ `using` name, and a Kotlin `import`
are removed when the name occurs only on the import itself. A file whose
extension has no recognised import syntax answers `NO_UNUSED_IMPORTS`.

## Safety boundary

Producing a plan is a read. Applying one requires all three gates: the host
exposes the edit capability, `WEAVATRIX_ALLOW_SOURCE_EDITS=1` is set, and the call
presents a single-use token bound to that exact plan and repository. Source
bytes go through weavatrix-worktree. Two control files under `.weavatrix`
record apply order and the cross-process lock. Readers share that lock. A writer
returns `REPO_BUSY` to other processes, and an apply that finds the repository
busy keeps its confirm token. An empty directory created for a rename is removed
on rollback.

## License

MIT.
