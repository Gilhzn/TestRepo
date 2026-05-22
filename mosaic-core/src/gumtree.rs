//! GumTree-style structural AST tree-diff.
//!
//! The existing semantic layer (`semantic.rs`) detects renames by matching
//! definition *body hashes*. That misses moved subtrees, distinguishes poorly
//! between edits and renames at finer granularity, and cannot describe the
//! shape of a change as an edit script. This module implements a pragmatic,
//! bounded version of the GumTree algorithm:
//!
//!   1. **Top-down matching**: every subtree is content-hashed (kind + child
//!      hashes + leaf bytes, mirroring `ast.rs`). Subtrees that share a hash
//!      on both sides are matched. If their structural *path* (the chain of
//!      ancestor kinds back to the root) differs, the match is a `Move`;
//!      otherwise it is a plain `Match`.
//!   2. **Named-definition reconciliation**: an unmatched definition that
//!      exists on both sides with the same name but a different body hash is
//!      an `Update` (edited body). One that survives only on one side becomes
//!      an `Insert` or `Delete`.
//!
//! To keep the script human-readable we only emit Insert/Delete/Update/Move
//! at the granularity of top-level definitions and direct statement-level
//! children of the root — we never explode the script down to every token.

use crate::ast::{AstTree, Lang};
use crate::error::Result;
use crate::hash::{Hash, Hasher};
use crate::semantic::SemanticHint;
use std::collections::HashMap;
use tree_sitter::Node;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditScript {
    pub actions: Vec<EditAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// A subtree present in both, unchanged (matched by hash).
    Match {
        kind: String,
        before_span: (usize, usize),
        after_span: (usize, usize),
    },
    /// A subtree only in `after` (inserted).
    Insert {
        kind: String,
        after_span: (usize, usize),
    },
    /// A subtree only in `before` (deleted).
    Delete {
        kind: String,
        before_span: (usize, usize),
    },
    /// Same content, different position (moved) — matched by hash but the
    /// surrounding path differs.
    Move {
        kind: String,
        before_span: (usize, usize),
        after_span: (usize, usize),
    },
    /// Same identity (e.g. function name) but changed body.
    Update {
        kind: String,
        name: String,
        before_span: (usize, usize),
        after_span: (usize, usize),
    },
}

impl EditScript {
    pub fn matched(&self) -> usize {
        self.count(|a| matches!(a, EditAction::Match { .. }))
    }

    pub fn inserted(&self) -> usize {
        self.count(|a| matches!(a, EditAction::Insert { .. }))
    }

    pub fn deleted(&self) -> usize {
        self.count(|a| matches!(a, EditAction::Delete { .. }))
    }

    pub fn moved(&self) -> usize {
        self.count(|a| matches!(a, EditAction::Move { .. }))
    }

    pub fn updated(&self) -> usize {
        self.count(|a| matches!(a, EditAction::Update { .. }))
    }

    fn count(&self, f: impl Fn(&EditAction) -> bool) -> usize {
        self.actions.iter().filter(|a| f(a)).count()
    }
}

/// A flattened view of a single subtree we care about: a top-level definition
/// or a direct statement-level child of the root.
struct NodeInfo {
    kind: String,
    hash: Hash,
    /// `/`-joined chain of ancestor kinds (root first) — the structural path.
    path: String,
    span: (usize, usize),
    /// `child_by_field_name("name")` text for named definitions.
    name: Option<String>,
}

/// Diff two source versions of the same language into an edit script.
pub fn diff(lang: Lang, before: &[u8], after: &[u8]) -> Result<EditScript> {
    let a = AstTree::parse(lang, before)?;
    let b = AstTree::parse(lang, after)?;

    let before_nodes = collect_top_level(&a);
    let after_nodes = collect_top_level(&b);

    // Index the `after` side by content hash so top-down matching is O(n).
    // A hash can repeat (identical siblings); keep a queue per hash so each
    // `before` node consumes a distinct `after` node.
    let mut after_by_hash: HashMap<Hash, Vec<usize>> = HashMap::new();
    for (i, n) in after_nodes.iter().enumerate() {
        after_by_hash.entry(n.hash).or_default().push(i);
    }

    let mut before_matched = vec![false; before_nodes.len()];
    let mut after_matched = vec![false; after_nodes.len()];
    let mut actions = Vec::new();

    // Phase 1: top-down hash matching (Match / Move).
    for (bi, bn) in before_nodes.iter().enumerate() {
        if let Some(slot) = after_by_hash.get_mut(&bn.hash).and_then(|q| q.pop()) {
            let an = &after_nodes[slot];
            before_matched[bi] = true;
            after_matched[slot] = true;
            if bn.path == an.path {
                actions.push(EditAction::Match {
                    kind: bn.kind.clone(),
                    before_span: bn.span,
                    after_span: an.span,
                });
            } else {
                actions.push(EditAction::Move {
                    kind: bn.kind.clone(),
                    before_span: bn.span,
                    after_span: an.span,
                });
            }
        }
    }

    // Phase 2: named-definition reconciliation (Update for shared names whose
    // bodies differ — these are unmatched because the hashes diverged).
    for (bi, bn) in before_nodes.iter().enumerate() {
        if before_matched[bi] {
            continue;
        }
        let Some(name) = bn.name.as_ref() else {
            continue;
        };
        let candidate = after_nodes.iter().enumerate().find(|(ai, an)| {
            !after_matched[*ai] && an.name.as_deref() == Some(name) && an.kind == bn.kind
        });
        if let Some((ai, an)) = candidate {
            before_matched[bi] = true;
            after_matched[ai] = true;
            actions.push(EditAction::Update {
                kind: bn.kind.clone(),
                name: name.clone(),
                before_span: bn.span,
                after_span: an.span,
            });
        }
    }

    // Phase 3: leftovers — deletions (before-only) and insertions (after-only).
    for (bi, bn) in before_nodes.iter().enumerate() {
        if !before_matched[bi] {
            actions.push(EditAction::Delete {
                kind: bn.kind.clone(),
                before_span: bn.span,
            });
        }
    }
    for (ai, an) in after_nodes.iter().enumerate() {
        if !after_matched[ai] {
            actions.push(EditAction::Insert {
                kind: an.kind.clone(),
                after_span: an.span,
            });
        }
    }

    Ok(EditScript { actions })
}

/// Collect the diff-granularity nodes: every direct child of the root. This
/// keeps top-level definitions and statement-level constructs while avoiding
/// an explosion down to individual tokens.
fn collect_top_level(ast: &AstTree) -> Vec<NodeInfo> {
    let source = ast.source();
    let root = ast.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        // Skip pure punctuation / trivia leaves (e.g. stray semicolons) so the
        // script focuses on meaningful constructs.
        if child.is_extra() || (child.child_count() == 0 && !child.is_named()) {
            continue;
        }
        out.push(NodeInfo {
            kind: child.kind().to_string(),
            hash: subtree_hash(child, source),
            path: node_path(child),
            span: (child.start_byte(), child.end_byte()),
            name: name_of(child, source),
        });
    }
    out
}

/// `name` field text, if this node is a named definition.
fn name_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    let bytes = &source[name_node.start_byte()..name_node.end_byte()];
    Some(String::from_utf8_lossy(bytes).to_string())
}

/// Structural path: chain of ancestor kinds from the root down to (and
/// including) this node, with the child index within each parent so that two
/// otherwise-identical subtrees in different positions get distinct paths.
fn node_path(node: Node<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        let idx = sibling_index(n);
        parts.push(format!("{}[{}]", n.kind(), idx));
        cur = n.parent();
    }
    parts.reverse();
    parts.join("/")
}

fn sibling_index(node: Node<'_>) -> usize {
    let Some(parent) = node.parent() else {
        return 0;
    };
    let mut cursor = parent.walk();
    for (i, child) in parent.children(&mut cursor).enumerate() {
        if child.id() == node.id() {
            return i;
        }
    }
    0
}

/// Content hash of a subtree: hash of (kind + child hashes + leaf bytes).
/// Mirrors `ast.rs::write_node` so equal-shaped subtrees hash equally and
/// whitespace/layout is ignored (tree-sitter elides it from the tree shape).
fn subtree_hash(node: Node<'_>, source: &[u8]) -> Hash {
    let mut h = Hasher::new();
    h.update(b"mosaic.gumtree.v1");
    write_node(node, source, &mut h);
    h.finalize()
}

fn write_node(node: Node<'_>, source: &[u8], h: &mut Hasher) {
    h.update(node.kind().as_bytes());
    h.update(b"\x00");
    if node.child_count() == 0 {
        let bytes = &source[node.start_byte()..node.end_byte()];
        h.update(&(bytes.len() as u32).to_le_bytes());
        h.update(bytes);
        h.update(b"\x00");
        return;
    }
    h.update(b"(");
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        write_node(child, source, h);
    }
    h.update(b")");
}

/// Bridge to the semantic layer: turn an edit script into `SemanticHint`s.
///
///   * `Update` → `DefinitionEdited { name, kind }`
///   * `Move` and `Match` are ignored in v1.
///
/// `Insert`/`Delete` actions only retain a kind + span (not a name), so they
/// are not surfaced as `DefinitionAdded`/`DefinitionRemoved` here; callers that
/// need add/remove hints should use the name-aware `semantic::analyze_pair`,
/// which this layer complements rather than replaces.
pub fn hints_from_script(script: &EditScript) -> Vec<SemanticHint> {
    script
        .actions
        .iter()
        .filter_map(|action| match action {
            EditAction::Update { name, kind, .. } => Some(SemanticHint::DefinitionEdited {
                name: name.clone(),
                kind: kind.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_sources_all_match() {
        let src = b"fn a(){}\nfn b(){ 1 }";
        let s = diff(Lang::Rust, src, src).unwrap();
        assert_eq!(s.inserted(), 0);
        assert_eq!(s.deleted(), 0);
        assert_eq!(s.updated(), 0);
        assert_eq!(s.moved(), 0);
        assert!(s.matched() >= 2, "expected both fns matched, got {s:?}");
    }

    #[test]
    fn append_function_is_one_insert() {
        let before = b"fn a(){}\nfn b(){ 1 }";
        let after = b"fn a(){}\nfn b(){ 1 }\nfn c(){ 2 }";
        let s = diff(Lang::Rust, before, after).unwrap();
        assert_eq!(s.inserted(), 1, "{s:?}");
        assert_eq!(s.deleted(), 0, "{s:?}");
        assert_eq!(s.updated(), 0, "{s:?}");
        assert!(s.matched() >= 2, "{s:?}");
    }

    #[test]
    fn delete_function_is_one_delete() {
        let before = b"fn a(){}\nfn b(){ 1 }\nfn c(){ 2 }";
        let after = b"fn a(){}\nfn b(){ 1 }";
        let s = diff(Lang::Rust, before, after).unwrap();
        assert_eq!(s.deleted(), 1, "{s:?}");
        assert_eq!(s.inserted(), 0, "{s:?}");
        assert_eq!(s.updated(), 0, "{s:?}");
    }

    #[test]
    fn body_edit_same_name_is_one_update() {
        let before = b"fn a(){}\nfn b(){ 1 }";
        let after = b"fn a(){}\nfn b(){ 2 }"; // b's body changed
        let s = diff(Lang::Rust, before, after).unwrap();
        assert_eq!(s.updated(), 1, "{s:?}");
        assert_eq!(s.inserted(), 0, "{s:?}");
        assert_eq!(s.deleted(), 0, "{s:?}");
        // `a` is untouched and should still match.
        assert!(s.matched() >= 1, "{s:?}");
    }

    #[test]
    fn whitespace_only_change_all_match() {
        let before = b"fn x()->u32{7}";
        let after = b"fn  x ( )  ->  u32  {  7  }";
        let s = diff(Lang::Rust, before, after).unwrap();
        assert_eq!(s.inserted(), 0, "{s:?}");
        assert_eq!(s.deleted(), 0, "{s:?}");
        assert_eq!(s.updated(), 0, "{s:?}");
        assert_eq!(s.moved(), 0, "{s:?}");
        assert!(s.matched() >= 1, "{s:?}");
    }

    #[test]
    fn rust_and_python_both_work() {
        // Rust: edit one body.
        let rs = diff(
            Lang::Rust,
            b"fn keep(){}\nfn edit(){ 1 }",
            b"fn keep(){}\nfn edit(){ 2 }",
        )
        .unwrap();
        assert_eq!(rs.updated(), 1, "rust: {rs:?}");

        // Python: append a function.
        let py_before = b"def keep():\n    return 1\n";
        let py_after = b"def keep():\n    return 1\n\ndef added():\n    return 2\n";
        let py = diff(Lang::Python, py_before, py_after).unwrap();
        assert_eq!(py.inserted(), 1, "python: {py:?}");
        assert_eq!(py.deleted(), 0, "python: {py:?}");
    }

    #[test]
    fn hints_from_script_maps_update_to_definition_edited() {
        let before = b"fn a(){}\nfn b(){ 1 }";
        let after = b"fn a(){}\nfn b(){ 2 }";
        let s = diff(Lang::Rust, before, after).unwrap();
        let hints = hints_from_script(&s);
        assert!(
            hints.iter().any(|h| matches!(
                h,
                SemanticHint::DefinitionEdited { name, kind }
                    if name == "b" && kind == "function_item"
            )),
            "expected DefinitionEdited for b, got {hints:?}"
        );
    }

    #[test]
    fn moved_definition_is_classified_as_move() {
        // Wrap a function so its structural path (and sibling index) changes
        // while its content hash stays the same.
        let before = b"fn a(){ 1 }\nfn b(){ 2 }";
        let after = b"fn b(){ 2 }\nfn a(){ 1 }"; // reordered
        let s = diff(Lang::Rust, before, after).unwrap();
        // Reordering changes each fn's sibling index → both classify as Move.
        assert_eq!(s.inserted(), 0, "{s:?}");
        assert_eq!(s.deleted(), 0, "{s:?}");
        assert_eq!(s.updated(), 0, "{s:?}");
        assert!(s.moved() >= 1, "expected at least one move, got {s:?}");
    }
}
