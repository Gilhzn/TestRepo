//! Semantic merge.
//!
//! When the patch layer reports a conflict on a file we know the language
//! of, we try to resolve it at the AST level: did one side rename a top-
//! level definition? Did the other side edit references to that definition?
//! If so, we can propagate the rename and continue.
//!
//! This module deliberately stops short of a full GumTree implementation.
//! It implements the *highest-value* AI-friendly subset:
//!
//!   * Detect rename-of-definition: a definition with name X exists on the
//!     base + ours and a definition with name Y and an identical body hash
//!     exists on theirs.
//!   * Detect call-site rewrite candidates: where a callsite for the old
//!     name appears in ours, point it out as a candidate rewrite.
//!
//! The output is structured `SemanticHint`s an AI agent or interactive
//! resolver can act on programmatically.

use crate::ast::{AstTree, DefInfo, Lang};
use crate::error::Result;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticHint {
    DefinitionRenamed {
        from: String,
        to: String,
        kind: String,
    },
    DefinitionRemoved {
        name: String,
        kind: String,
    },
    DefinitionAdded {
        name: String,
        kind: String,
    },
    DefinitionEdited {
        name: String,
        kind: String,
    },
    CallSiteUsesOldName {
        old_name: String,
        new_name: String,
        line: usize,
        column: usize,
    },
}

pub fn analyze_pair(lang: Lang, before: &[u8], after: &[u8]) -> Result<Vec<SemanticHint>> {
    let a = AstTree::parse(lang, before)?;
    let b = AstTree::parse(lang, after)?;
    Ok(diff_definitions(&a.definitions(), &b.definitions()))
}

pub fn analyze_three_way(
    lang: Lang,
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
) -> Result<ThreeWaySemanticReport> {
    let base = AstTree::parse(lang, base)?;
    let ours = AstTree::parse(lang, ours)?;
    let theirs = AstTree::parse(lang, theirs)?;

    let base_defs = base.definitions();
    let ours_defs = ours.definitions();
    let theirs_defs = theirs.definitions();

    let mut renames = Vec::new();
    let mut hints = Vec::new();

    for (name, base_info) in &base_defs {
        let in_ours = ours_defs.contains_key(name);
        let in_theirs = theirs_defs.contains_key(name);
        match (in_ours, in_theirs) {
            (true, true) => {}
            (false, true) => {
                if let Some(new_name) = find_matching_def(base_info, &ours_defs, &base_defs) {
                    renames.push(Rename {
                        from: name.clone(),
                        to: new_name.clone(),
                        side: Side::Ours,
                        kind: base_info.kind.clone(),
                    });
                    hints.push(SemanticHint::DefinitionRenamed {
                        from: name.clone(),
                        to: new_name,
                        kind: base_info.kind.clone(),
                    });
                } else {
                    hints.push(SemanticHint::DefinitionRemoved {
                        name: name.clone(),
                        kind: base_info.kind.clone(),
                    });
                }
            }
            (true, false) => {
                if let Some(new_name) = find_matching_def(base_info, &theirs_defs, &base_defs) {
                    renames.push(Rename {
                        from: name.clone(),
                        to: new_name.clone(),
                        side: Side::Theirs,
                        kind: base_info.kind.clone(),
                    });
                    hints.push(SemanticHint::DefinitionRenamed {
                        from: name.clone(),
                        to: new_name,
                        kind: base_info.kind.clone(),
                    });
                } else {
                    hints.push(SemanticHint::DefinitionRemoved {
                        name: name.clone(),
                        kind: base_info.kind.clone(),
                    });
                }
            }
            (false, false) => hints.push(SemanticHint::DefinitionRemoved {
                name: name.clone(),
                kind: base_info.kind.clone(),
            }),
        }
    }

    for (name, info) in &ours_defs {
        if !base_defs.contains_key(name) && !renames.iter().any(|r| &r.to == name) {
            hints.push(SemanticHint::DefinitionAdded {
                name: name.clone(),
                kind: info.kind.clone(),
            });
        }
    }
    for (name, info) in &theirs_defs {
        if !base_defs.contains_key(name)
            && !ours_defs.contains_key(name)
            && !renames.iter().any(|r| &r.to == name)
        {
            hints.push(SemanticHint::DefinitionAdded {
                name: name.clone(),
                kind: info.kind.clone(),
            });
        }
    }

    let opposite_source = |side: Side| -> &[u8] {
        match side {
            Side::Ours => theirs.source(),
            Side::Theirs => ours.source(),
        }
    };
    for r in &renames {
        let opposite = opposite_source(r.side);
        for (line_idx, line) in std::str::from_utf8(opposite)
            .unwrap_or("")
            .lines()
            .enumerate()
        {
            if let Some(col) = find_word(line, &r.from) {
                hints.push(SemanticHint::CallSiteUsesOldName {
                    old_name: r.from.clone(),
                    new_name: r.to.clone(),
                    line: line_idx + 1,
                    column: col + 1,
                });
            }
        }
    }

    Ok(ThreeWaySemanticReport { hints, renames })
}

fn diff_definitions(
    a: &BTreeMap<String, DefInfo>,
    b: &BTreeMap<String, DefInfo>,
) -> Vec<SemanticHint> {
    let mut out = Vec::new();
    for (name, info) in a {
        match b.get(name) {
            Some(other) if other.body_hash != info.body_hash => {
                out.push(SemanticHint::DefinitionEdited {
                    name: name.clone(),
                    kind: info.kind.clone(),
                })
            }
            Some(_) => {}
            None => {
                if let Some(new_name) = find_matching_def(info, b, a) {
                    out.push(SemanticHint::DefinitionRenamed {
                        from: name.clone(),
                        to: new_name,
                        kind: info.kind.clone(),
                    });
                } else {
                    out.push(SemanticHint::DefinitionRemoved {
                        name: name.clone(),
                        kind: info.kind.clone(),
                    });
                }
            }
        }
    }
    for (name, info) in b {
        if !a.contains_key(name) {
            if out.iter().any(|h| match h {
                SemanticHint::DefinitionRenamed { to, .. } => to == name,
                _ => false,
            }) {
                continue;
            }
            out.push(SemanticHint::DefinitionAdded {
                name: name.clone(),
                kind: info.kind.clone(),
            });
        }
    }
    out
}

fn find_matching_def(
    target: &DefInfo,
    candidates: &BTreeMap<String, DefInfo>,
    avoid: &BTreeMap<String, DefInfo>,
) -> Option<String> {
    for (name, info) in candidates {
        if avoid.contains_key(name) {
            continue;
        }
        if info.body_hash == target.body_hash && info.kind == target.kind {
            return Some(name.clone());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Ours,
    Theirs,
}

#[derive(Debug, Clone)]
struct Rename {
    from: String,
    to: String,
    side: Side,
    kind: String,
}

#[derive(Debug, Clone)]
pub struct ThreeWaySemanticReport {
    pub hints: Vec<SemanticHint>,
    renames: Vec<Rename>,
}

impl ThreeWaySemanticReport {
    pub fn rename_count(&self) -> usize {
        self.renames.len()
    }

    pub fn renames(&self) -> impl Iterator<Item = (&str, &str)> {
        self.renames.iter().map(|r| (r.from.as_str(), r.to.as_str()))
    }
}

fn find_word(line: &str, word: &str) -> Option<usize> {
    let mut i = 0;
    while let Some(pos) = line[i..].find(word) {
        let start = i + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !is_ident(line.as_bytes()[start - 1]);
        let after_ok = end == line.len() || !is_ident(line.as_bytes()[end]);
        if before_ok && after_ok {
            return Some(start);
        }
        i = end;
    }
    None
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unedited_pair() {
        let src = b"fn alpha() {}\nfn beta() {}";
        let hints = analyze_pair(Lang::Rust, src, src).unwrap();
        assert!(hints.is_empty());
    }

    #[test]
    fn detects_body_edit() {
        let a = b"fn x() { 1 }";
        let b = b"fn x() { 2 }";
        let hints = analyze_pair(Lang::Rust, a, b).unwrap();
        assert!(matches!(
            &hints[0],
            SemanticHint::DefinitionEdited { name, .. } if name == "x"
        ));
    }

    #[test]
    fn detects_rename() {
        let a = b"fn chargeCard() { do_stuff(); }";
        let b = b"fn processCharge() { do_stuff(); }";
        let hints = analyze_pair(Lang::Rust, a, b).unwrap();
        assert_eq!(hints.len(), 1);
        match &hints[0] {
            SemanticHint::DefinitionRenamed { from, to, .. } => {
                assert_eq!(from, "chargeCard");
                assert_eq!(to, "processCharge");
            }
            other => panic!("expected DefinitionRenamed, got {other:?}"),
        }
    }

    #[test]
    fn detects_addition() {
        let a = b"fn x() {}";
        let b = b"fn x() {}\nfn y() {}";
        let hints = analyze_pair(Lang::Rust, a, b).unwrap();
        assert!(hints
            .iter()
            .any(|h| matches!(h, SemanticHint::DefinitionAdded { name, .. } if name == "y")));
    }

    #[test]
    fn three_way_rename_plus_callsite_edit() {
        let base = b"fn chargeCard(amount: u32) -> u32 { amount }";
        let ours = b"fn processCharge(amount: u32) -> u32 { amount }";
        let theirs = b"fn chargeCard(amount: u32) -> u32 { amount * 2 }";
        let report = analyze_three_way(Lang::Rust, base, ours, theirs).unwrap();
        let has_rename = report.hints.iter().any(|h| {
            matches!(
                h,
                SemanticHint::DefinitionRenamed { from, to, .. }
                    if from == "chargeCard" && to == "processCharge"
            )
        });
        assert!(has_rename, "expected rename hint, got {:?}", report.hints);
        assert_eq!(report.rename_count(), 1);
    }

    #[test]
    fn three_way_rename_with_callsite_in_other_branch() {
        let base = b"fn chargeCard() {}\nfn caller() { chargeCard(); }";
        let ours = b"fn processCharge() {}\nfn caller() { chargeCard(); }";
        let theirs = b"fn chargeCard() {}\nfn caller() { chargeCard(); log(); }";
        let report = analyze_three_way(Lang::Rust, base, ours, theirs).unwrap();
        let has_callsite = report
            .hints
            .iter()
            .any(|h| matches!(h, SemanticHint::CallSiteUsesOldName { old_name, .. } if old_name == "chargeCard"));
        assert!(has_callsite, "expected CallSiteUsesOldName, got {:?}", report.hints);
    }

    #[test]
    fn three_way_python_rename() {
        let base = b"def make_user(name):\n    return name\n";
        let ours = b"def create_user(name):\n    return name\n";
        let theirs = b"def make_user(name):\n    return name.upper()\n";
        let report = analyze_three_way(Lang::Python, base, ours, theirs).unwrap();
        assert!(report.hints.iter().any(|h| matches!(
            h,
            SemanticHint::DefinitionRenamed { from, to, .. }
                if from == "make_user" && to == "create_user"
        )));
    }

    #[test]
    fn whitespace_change_is_not_a_hint() {
        let a = b"fn x() -> u32 { 7 }";
        let b = b"fn  x ( )  ->  u32  {  7  }";
        let hints = analyze_pair(Lang::Rust, a, b).unwrap();
        assert!(
            hints.is_empty(),
            "whitespace-only change should produce no hints, got {hints:?}"
        );
    }
}
