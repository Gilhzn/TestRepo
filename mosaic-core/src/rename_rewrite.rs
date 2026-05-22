//! Identifier-aware rename rewriting (M5 auto-rewrite call sites).
//!
//! When a definition is renamed (e.g. `charge_card` -> `stripe_charge`), the
//! call sites in the rest of the file should follow. Doing this with a textual
//! search-and-replace is unsafe: it would clobber the substring inside string
//! literals, comments, and longer identifiers like `charge_card_fee`. Instead
//! we lean on the language grammar (tree-sitter), rewriting only genuine
//! identifier-*leaf* tokens whose whole text equals a rename source.

use crate::ast::{AstTree, Lang};
use std::collections::BTreeMap;
use tree_sitter::Node;

/// The set of tree-sitter leaf node kinds we treat as "an identifier token".
///
/// We additionally require `child_count() == 0` at the call site, so we only
/// ever look at genuine leaves. String literals and comments are excluded by
/// construction: their bytes are not parsed as nodes of these kinds, so they
/// can never match. Longer identifiers such as `charge_card_fee` are excluded
/// because the *whole* leaf text (`charge_card_fee`) is compared, not a
/// substring.
const IDENT_KINDS: &[&str] = &[
    "identifier",
    "field_identifier",
    "type_identifier",
    "property_identifier",
    "shorthand_property_identifier",
    "constant",
    "package_identifier",
    "scoped_identifier",
];

/// Rewrite identifier-token occurrences of each `from` -> `to` in `source`,
/// using the language grammar so only genuine identifiers are touched — never
/// string contents, comments, or a substring of a longer identifier. Returns
/// (rewritten_source, number_of_tokens_rewritten). On parse failure, empty
/// `renames`, or ambiguous renames, returns the source unchanged (count 0 for
/// the ambiguous/failed entries).
pub fn apply_renames(lang: Lang, source: &str, renames: &[(String, String)]) -> (String, usize) {
    if renames.is_empty() {
        return (source.to_string(), 0);
    }

    // Build the rename map, dropping ambiguous `from`s (same source mapped to
    // two different targets). We track conflicting keys and remove them.
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (from, to) in renames {
        match map.get(from) {
            Some(existing) if existing != to => {
                ambiguous.insert(from.clone());
            }
            _ => {
                map.insert(from.clone(), to.clone());
            }
        }
    }
    for k in &ambiguous {
        map.remove(k);
    }
    if map.is_empty() {
        return (source.to_string(), 0);
    }

    let tree = match AstTree::parse(lang, source.as_bytes()) {
        Ok(t) => t,
        Err(_) => return (source.to_string(), 0),
    };

    let src = tree.source();
    // Collected (byte_range, replacement_text).
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    collect_edits(tree.root_node(), src, &map, &mut edits);

    if edits.is_empty() {
        return (source.to_string(), 0);
    }

    let count = edits.len();

    // Apply replacements in DESCENDING start order so earlier byte offsets stay
    // valid as we splice. Identifier leaves never overlap, so a simple sort by
    // start (descending) is sufficient and unambiguous.
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut out = source.to_string();
    for (range, replacement) in &edits {
        out.replace_range(range.clone(), replacement);
    }

    (out, count)
}

/// Walk the whole tree, recording an edit for every identifier-leaf node whose
/// exact text is a key in `map`.
fn collect_edits(
    node: Node<'_>,
    src: &[u8],
    map: &BTreeMap<String, String>,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    // Only leaf nodes are candidate identifier tokens; this excludes strings
    // and comments whose text spans children / isn't an identifier kind.
    if node.child_count() == 0 && IDENT_KINDS.contains(&node.kind()) {
        if let Ok(text) = node.utf8_text(src) {
            if let Some(to) = map.get(text) {
                edits.push((node.byte_range(), to.clone()));
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_edits(child, src, map, edits);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(from: &str, to: &str) -> (String, String) {
        (from.to_string(), to.to_string())
    }

    #[test]
    fn rust_rename_skips_string_comment_and_longer_ident() {
        let src = r#"
fn charge_card(amount: u32) -> u32 {
    // call charge_card here in a comment
    let note = "please charge_card the user";
    let fee = charge_card_fee(amount);
    charge_card(amount) + fee
}

fn run() {
    let x = charge_card(10);
    let _ = x;
}
"#;
        let (out, count) = apply_renames(Lang::Rust, src, &[r("charge_card", "stripe_charge")]);

        // Definition name + 2 genuine call sites = 3 rewrites.
        assert_eq!(count, 3, "got output:\n{out}");
        assert!(out.contains("fn stripe_charge(amount: u32)"), "def renamed:\n{out}");
        assert!(out.contains("stripe_charge(amount) + fee"));
        assert!(out.contains("let x = stripe_charge(10);"));

        // Untouched: comment, string contents, longer identifier.
        assert!(out.contains("// call charge_card here in a comment"), "comment intact:\n{out}");
        assert!(out.contains(r#""please charge_card the user""#), "string intact:\n{out}");
        assert!(out.contains("charge_card_fee(amount)"), "longer ident intact:\n{out}");
        // The longer identifier must NOT have been turned into stripe_charge_fee.
        assert!(!out.contains("stripe_charge_fee"));
    }

    #[test]
    fn python_rename_skips_string_and_longer_ident() {
        let src = r#"
def make_user(name):
    # make_user does the thing
    msg = "make_user was called"
    helper = make_user_v2(name)
    return make_user(name) + helper

def caller():
    return make_user("bob")
"#;
        let (out, count) = apply_renames(Lang::Python, src, &[r("make_user", "create_user")]);

        // def name + 2 call sites = 3.
        assert_eq!(count, 3, "got output:\n{out}");
        assert!(out.contains("def create_user(name):"), "def renamed:\n{out}");
        assert!(out.contains("return create_user(name) + helper"));
        assert!(out.contains(r#"return create_user("bob")"#));

        // Untouched.
        assert!(out.contains("# make_user does the thing"), "comment intact:\n{out}");
        assert!(out.contains(r#""make_user was called""#), "string intact:\n{out}");
        assert!(out.contains("make_user_v2(name)"), "longer ident intact:\n{out}");
        assert!(!out.contains("create_user_v2"));
    }

    #[test]
    fn javascript_function_decl_and_call() {
        let src = r#"
function chargeCard(amount) {
    // chargeCard comment
    return amount;
}

function main() {
    var s = "chargeCard string";
    return chargeCard(42);
}
"#;
        let (out, count) = apply_renames(Lang::JavaScript, src, &[r("chargeCard", "processCharge")]);

        // decl name + 1 call site = 2.
        assert_eq!(count, 2, "got output:\n{out}");
        assert!(out.contains("function processCharge(amount)"), "decl renamed:\n{out}");
        assert!(out.contains("return processCharge(42);"));
        assert!(out.contains("// chargeCard comment"), "comment intact:\n{out}");
        assert!(out.contains(r#""chargeCard string""#), "string intact:\n{out}");
    }

    #[test]
    fn ambiguous_rename_is_dropped() {
        let src = "fn f() { let a = 1; a + a }";
        let (out, count) =
            apply_renames(Lang::Rust, src, &[r("a", "b"), r("a", "c")]);
        assert_eq!(count, 0);
        assert_eq!(out, src, "ambiguous source left untouched");
    }

    #[test]
    fn empty_renames_returns_source_unchanged() {
        let src = "fn f() { charge_card(); }";
        let (out, count) = apply_renames(Lang::Rust, src, &[]);
        assert_eq!(count, 0);
        assert_eq!(out, src);
    }

    #[test]
    fn parse_does_not_panic_and_non_matching_returns_unchanged() {
        let src = "fn f() { other(); }";
        let (out, count) = apply_renames(Lang::Rust, src, &[r("charge_card", "stripe_charge")]);
        assert_eq!(count, 0);
        assert_eq!(out, src);
    }
}
