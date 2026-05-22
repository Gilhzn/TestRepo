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

    /// Human-readable explanation of what the merge engine did and why.
    /// Useful for `mos merge --explain` and for AI agents that want a
    /// natural-language summary alongside the structured data.
    pub fn explain(&self) -> String {
        let mut s = String::new();
        s.push_str("Mosaic merge summary\n");
        s.push_str("====================\n\n");

        // Strategy summary
        s.push_str("Strategy chain:\n");
        s.push_str("  1. patch commutation (line-graph) — applied\n");
        s.push_str("  2. semantic AST analysis — ");
        if self.semantic_hints.is_empty() {
            s.push_str("(no hints emitted)\n");
        } else {
            s.push_str(&format!(
                "{} hint(s) found\n",
                self.semantic_hints.len()
            ));
        }
        s.push('\n');

        // Outcome
        if self.is_clean() && self.semantic_hints.is_empty() {
            s.push_str("Outcome: clean merge. Both sides combined without conflict.\n");
        } else if self.is_clean() && !self.semantic_hints.is_empty() {
            s.push_str(
                "Outcome: clean text merge. Semantic hints below describe what \
                 the engine learned about the change (for reviewers, agents, IDE).\n",
            );
        } else {
            s.push_str(
                "Outcome: merged graph is valid (acyclic, deterministic order) \
                 but carries structured conflicts. The repository is NOT wedged — \
                 these are data your agent or reviewer can resolve.\n",
            );
        }
        s.push('\n');

        // Renames
        if !self.renames.is_empty() {
            s.push_str("Renames detected:\n");
            for (from, to) in &self.renames {
                s.push_str(&format!("  • {from} → {to}\n"));
            }
            s.push('\n');
        }

        // Semantic hints
        if !self.semantic_hints.is_empty() {
            s.push_str("Semantic hints:\n");
            for hint in &self.semantic_hints {
                let line = match hint {
                    SemanticHint::DefinitionRenamed { from, to, kind } => {
                        format!("  • rename ({kind}): {from} → {to}")
                    }
                    SemanticHint::DefinitionEdited { name, kind } => {
                        format!("  • edited ({kind}): {name}")
                    }
                    SemanticHint::DefinitionAdded { name, kind } => {
                        format!("  • added ({kind}): {name}")
                    }
                    SemanticHint::DefinitionRemoved { name, kind } => {
                        format!("  • removed ({kind}): {name}")
                    }
                    SemanticHint::CallSiteUsesOldName {
                        old_name,
                        new_name,
                        line,
                        column,
                    } => format!(
                        "  • callsite at {line}:{column} still uses old name \
                         '{old_name}' (now '{new_name}') — \
                         consider rewriting"
                    ),
                };
                s.push_str(&line);
                s.push('\n');
            }
            s.push('\n');
        }

        // Patch conflicts
        if !self.patch_conflicts.is_empty() {
            s.push_str("Patch-level conflicts (structured, repo still valid):\n");
            for c in &self.patch_conflicts {
                let line = match c {
                    StructuredConflict::ConcurrentInsert {
                        anchor,
                        ours,
                        theirs,
                        ..
                    } => format!(
                        "  • concurrent insert at anchor {}: ours={}, theirs={} (both kept; \
                         resolver picks one to kill)",
                        short(&anchor.0),
                        short(&ours.0),
                        short(&theirs.0)
                    ),
                    StructuredConflict::EditVsDelete {
                        target,
                        deleter,
                        editor,
                    } => format!(
                        "  • edit-vs-delete on {}: deleted by {deleter:?}, edited by {editor:?} \
                         (dead anchor preserved, downstream patches stay valid)",
                        short(&target.0)
                    ),
                };
                s.push_str(&line);
                s.push('\n');
            }
            s.push('\n');
        }

        s
    }
}

fn short(h: &Hash) -> String {
    h.to_hex()[..8].to_string()
}

pub use crate::m1_patch::merge::ResolveStrategy;

/// Three-way merge a file and deterministically resolve every conflict with
/// `strategy`, returning the resolved file as a single string.
pub fn resolve_text_file(
    creator: &Hash,
    base: &str,
    ours: &str,
    theirs: &str,
    strategy: ResolveStrategy,
) -> Result<String> {
    let result = patch_merge(creator, base, ours, theirs)?;
    let lines = result.resolve(strategy)?;
    let mut out: String = lines.into_iter().map(|l| format!("{l}\n")).collect();
    if !out.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
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

    // Both sides anchor on the same base (so anchors line up) but mint their
    // *new* vertices from distinct creators. That way an identical line added
    // on both sides becomes two distinct vertices rather than colliding into
    // one — which previously crashed apply and, after the dedup workaround,
    // could fuse the bodies of two separate edits into broken output.
    let ours_creator = Hash::of(format!("{}:ours", creator.to_hex()).as_bytes());
    let theirs_creator = Hash::of(format!("{}:theirs", creator.to_hex()).as_bytes());
    let ours_patch = derive_text_patch(creator, &ours_creator, &base_norm, &ours_norm);
    let theirs_patch = derive_text_patch(creator, &theirs_creator, &base_norm, &theirs_norm);

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

fn derive_text_patch(anchor_creator: &Hash, insert_creator: &Hash, before: &str, after: &str) -> Patch {
    let result = crate::crdt::compile_session_salted(anchor_creator, insert_creator, before, after)
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
    fn explain_clean_merge_has_no_conflict_section() {
        let creator = Hash::of(b"explain-1");
        let result = merge_text_file(
            &creator,
            None,
            "alpha\nbeta\n",
            "alpha\nNEW_A\nbeta\n",
            "alpha\nbeta\nNEW_B\n",
        )
        .unwrap();
        let explanation = result.explain();
        assert!(explanation.contains("Mosaic merge summary"));
        assert!(explanation.contains("clean merge"));
        assert!(!explanation.contains("Patch-level conflicts"));
        assert!(!explanation.contains("Renames detected"));
    }

    #[test]
    fn explain_rename_calls_out_the_rename_and_callsite() {
        let creator = Hash::of(b"explain-2");
        let result = merge_text_file(
            &creator,
            Some(Lang::Rust),
            "fn chargeCard() {}\nfn other() { chargeCard(); }\n",
            "fn processCharge() {}\nfn other() { chargeCard(); }\n",
            "fn chargeCard() {}\nfn other() { chargeCard(); log(); }\n",
        )
        .unwrap();
        let explanation = result.explain();
        assert!(
            explanation.contains("Renames detected"),
            "expected renames section, got:\n{explanation}"
        );
        assert!(explanation.contains("chargeCard → processCharge"));
        assert!(explanation.contains("callsite"));
    }

    #[test]
    fn resolve_ours_keeps_local_on_concurrent_insert() {
        // Both sides insert a different line at the same anchor.
        let base = "alpha\nbeta\n";
        let ours = "alpha\nOURS_LINE\nbeta\n";
        let theirs = "alpha\nTHEIRS_LINE\nbeta\n";
        let creator = Hash::of(b"resolve-1");

        let ours_resolved =
            resolve_text_file(&creator, base, ours, theirs, ResolveStrategy::Ours).unwrap();
        assert!(ours_resolved.contains("OURS_LINE"));
        assert!(!ours_resolved.contains("THEIRS_LINE"));

        let theirs_resolved =
            resolve_text_file(&creator, base, ours, theirs, ResolveStrategy::Theirs).unwrap();
        assert!(theirs_resolved.contains("THEIRS_LINE"));
        assert!(!theirs_resolved.contains("OURS_LINE"));

        let union =
            resolve_text_file(&creator, base, ours, theirs, ResolveStrategy::Union).unwrap();
        assert!(union.contains("OURS_LINE"));
        assert!(union.contains("THEIRS_LINE"));
    }

    #[test]
    fn identical_line_on_both_sides_does_not_crash() {
        // Regression: both sides insert a line with identical content
        // ("SHARED"). Under a single creator these derived the same VertexId and
        // aborted the merge ("vertex already exists"). With per-side creators it
        // must merge without crashing and keep *both* sides' unique work.
        let base = "alpha\nbeta\n";
        let ours = "alpha\nOURS\nSHARED\nbeta\n";
        let theirs = "alpha\nTHEIRS\nSHARED\nbeta\n";
        let creator = Hash::of(b"identical-line");
        let result = merge_text_file(&creator, None, base, ours, theirs)
            .expect("merge must not crash on identical concurrent inserts");
        assert!(result.merged_lines.iter().any(|l| l == "OURS"));
        assert!(result.merged_lines.iter().any(|l| l == "THEIRS"));
        assert!(result.merged_lines.iter().any(|l| l == "SHARED"));
    }

    #[test]
    fn concurrent_blocks_with_identical_body_lines_stay_valid() {
        // Two agents each add a distinct `if` block whose bodies share identical
        // lines. Fusing those identical lines into one (the old dedup) left each
        // `if` without a body — syntactically broken. Per-side creators keep
        // each block's body intact: both bodies must survive.
        let base = "def main():\n    pass\n";
        let ours = "def main():\n    if a:\n        do()\n        return 0\n    pass\n";
        let theirs = "def main():\n    if b:\n        do()\n        return 0\n    pass\n";
        let creator = Hash::of(b"concurrent-blocks");
        let result = merge_text_file(&creator, Some(Lang::Python), base, ours, theirs)
            .expect("merge must not crash");
        // Both guards present...
        assert!(result.merged_lines.iter().any(|l| l.contains("if a:")));
        assert!(result.merged_lines.iter().any(|l| l.contains("if b:")));
        // ...and the shared body line survives for *each* block (two copies),
        // so neither `if` is left bodyless.
        let bodies = result.merged_lines.iter().filter(|l| l.contains("do()")).count();
        assert_eq!(bodies, 2, "each block must keep its own body, got {bodies}");
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
