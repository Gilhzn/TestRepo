//! Combined merge engine.
//!
//! The patch layer (M1) gives us mathematically correct text merges with
//! conflict-as-data. The AST layer (M5) lets us reinterpret some of those
//! "conflicts" as renames + propagatable references. This module ties them
//! together as a single API the SDK and CLI call.
//!
//! Strategy (in order):
//!   1. **Patch commutation / three-way merge** — solves the majority of
//!      cases mechanically.
//!   2. **Semantic analysis** — if the file's language is known, surface
//!      renames, definition edits, and call-site hints. These don't *resolve*
//!      the text conflicts on their own; they hand structured data to the
//!      caller (an AI agent or a human resolver) describing *why* the patches
//!      collide.

use crate::ast::Lang;
use crate::error::Result;
use crate::hash::Hash;
use crate::m1_patch::line_graph::LineGraph;
use crate::m1_patch::merge::{three_way_merge, MergeResult, StructuredConflict};
use crate::m1_patch::patch::Patch;
use crate::semantic::{analyze_three_way, SemanticHint, ThreeWaySemanticReport};

/// Three-way merge for a single text file across both layers.
pub struct FileMerge {
    pub merged_lines: Vec<String>,
    pub patch_conflicts: Vec<StructuredConflict>,
    pub semantic_hints: Vec<SemanticHint>,
    pub renames: Vec<(String, String)>,
}

impl FileMerge {
    pub fn is_clean(&self) -> bool {
        self.patch_conflicts.is_empty()
    }

    pub fn detected_a_rename(&self) -> bool {
        !self.renames.is_empty()
    }
}

pub fn merge_text_file(
    creator: &Hash,
    lang: Option<Lang>,
    base: &str,
    ours: &str,
    theirs: &str,
) -> Result<FileMerge> {
    let MergeResult {
        graph,
        conflicts: patch_conflicts,
    } = patch_merge(creator, base, ours, theirs)?;
    let merged_lines: Vec<String> = graph
        .flatten()
        .into_iter()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .collect();

    let (semantic_hints, renames) = match lang {
        Some(l) => {
            let report = analyze_three_way(l, base.as_bytes(), ours.as_bytes(), theirs.as_bytes())?;
            let renames: Vec<(String, String)> = report
                .renames()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
            (report.hints, renames)
        }
        None => (Vec::new(), Vec::new()),
    };

    Ok(FileMerge {
        merged_lines,
        patch_conflicts,
        semantic_hints,
        renames,
    })
}

pub fn semantic_report(
    lang: Lang,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<ThreeWaySemanticReport> {
    analyze_three_way(lang, base, ours, theirs)
}

fn patch_merge(creator: &Hash, base: &str, ours: &str, theirs: &str) -> Result<MergeResult> {
    let base_norm = normalize(base);
    let ours_norm = normalize(ours);
    let theirs_norm = normalize(theirs);

    let base_lines: Vec<&[u8]> = base_norm
        .lines()
        .map(|s| s.as_bytes())
        .collect();
    let base_graph = LineGraph::from_lines(creator, &base_lines);

    let ours_patch = derive_text_patch(creator, &base_norm, &ours_norm);
    let theirs_patch = derive_text_patch(creator, &base_norm, &theirs_norm);

    three_way_merge(&base_graph, &ours_patch, &theirs_patch)
}

fn normalize(s: &str) -> String {
    if s.ends_with('\n') {
        s.to_string()
    } else if s.is_empty() {
        String::new()
    } else {
        format!("{s}\n")
    }
}

fn derive_text_patch(creator: &Hash, before: &str, after: &str) -> Patch {
    let result = crate::crdt::compile_session(creator, before, after)
        .expect("compile_session on plain strings is infallible");
    result.patch
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_inserts_only_merge_clean() {
        let base = "alpha\nbeta\n";
        let ours = "alpha\nNEW_A\nbeta\n";
        let theirs = "alpha\nbeta\nNEW_B\n";
        let creator = Hash::of(b"merge-1");
        let result = merge_text_file(&creator, None, base, ours, theirs).unwrap();
        assert!(
            result.is_clean(),
            "expected clean merge, got conflicts={:?}",
            result.patch_conflicts
        );
        assert!(result.merged_lines.iter().any(|l| l == "NEW_A"));
        assert!(result.merged_lines.iter().any(|l| l == "NEW_B"));
    }

    #[test]
    fn disjoint_edits_produce_correct_merged_text_even_with_informational_conflicts() {
        // Body edits replace lines, so the patch layer reports
        // EditVsDelete (one side kills the old line that the other
        // anchors on). The graph still produces the right text.
        let base = "fn x() {}\nfn y() {}\nfn z() {}\n";
        let ours = "fn x() {}\nfn y() { 1 }\nfn z() {}\n";
        let theirs = "fn x() {}\nfn y() {}\nfn z() { 2 }\n";
        let creator = Hash::of(b"merge-2");
        let result = merge_text_file(&creator, Some(Lang::Rust), base, ours, theirs).unwrap();
        assert!(result.merged_lines.iter().any(|l| l.contains("{ 1 }")));
        assert!(result.merged_lines.iter().any(|l| l.contains("{ 2 }")));
    }

    #[test]
    fn rename_on_one_side_surfaces_as_semantic_hint() {
        let base = "fn chargeCard() {}\n";
        let ours = "fn processCharge() {}\n";
        let theirs = "fn chargeCard() {\n    log();\n}\n";
        let creator = Hash::of(b"merge-2");
        let result = merge_text_file(&creator, Some(Lang::Rust), base, ours, theirs).unwrap();
        assert!(result.detected_a_rename());
        assert_eq!(result.renames[0].0, "chargeCard");
        assert_eq!(result.renames[0].1, "processCharge");
    }

    #[test]
    fn unknown_language_returns_no_semantic_hints() {
        let base = "alpha\nbeta\n";
        let ours = "alpha\nBETA\n";
        let theirs = "alpha\nbeta\ngamma\n";
        let creator = Hash::of(b"merge-3");
        let result = merge_text_file(&creator, None, base, ours, theirs).unwrap();
        assert!(result.semantic_hints.is_empty());
    }

    #[test]
    fn python_three_way_merge() {
        let base = "def make_user(name):\n    return name\n";
        let ours = "def create_user(name):\n    return name\n";
        let theirs = "def make_user(name):\n    return name.upper()\n";
        let creator = Hash::of(b"merge-py");
        let result = merge_text_file(&creator, Some(Lang::Python), base, ours, theirs).unwrap();
        assert!(
            result.renames.iter().any(|(a, b)| a == "make_user" && b == "create_user"),
            "expected rename, got renames={:?} hints={:?}",
            result.renames,
            result.semantic_hints
        );
    }
}
