//! Cross-language integration matrix for
//! `mosaic_core::rename_rewrite::apply_renames`.
//!
//! For every one of the eight supported languages we feed a realistic snippet
//! that deliberately contains the token `oldName` in five distinct positions:
//!
//!   1. a definition of `oldName` (function / method),
//!   2. one or more genuine call sites of `oldName`,
//!   3. the literal text `oldName` inside a STRING literal,
//!   4. the literal text `oldName` inside a COMMENT,
//!   5. a LONGER identifier (`oldNameHelper` / `old_name_helper`) that merely
//!      has `oldName` as a prefix.
//!
//! We then rename `oldName` -> `newName` and assert that only genuine
//! identifier *tokens* are rewritten: the definition and the call sites flip to
//! `newName`, while the string text, the comment text, and the longer
//! identifier are all left untouched. The returned count must equal the number
//! of real identifier occurrences we expected to change.
//!
//! NOTE: `apply_renames` is implemented by a parallel worker. Until that lands,
//! this file is expected to fail to COMPILE with an unresolved-import error.

use mosaic_core::ast::Lang;
use mosaic_core::rename_rewrite::apply_renames;

/// Shared assertion helper. `expected_count` is the number of genuine
/// identifier tokens that should have been rewritten.
fn check(
    lang: Lang,
    source: &str,
    expected_count: usize,
    longer_identifier: &str,
    string_fragment: &str,
    comment_fragment: &str,
) {
    let (out, count) =
        apply_renames(lang, source, &[("oldName".into(), "newName".into())]);

    assert_eq!(
        count, expected_count,
        "[{lang:?}] expected {expected_count} identifier rewrites, got {count}\n--- rewritten ---\n{out}\n-----------------"
    );

    // The genuine identifier tokens (definition + call sites) became `newName`.
    assert!(
        out.contains("newName"),
        "[{lang:?}] expected `newName` to appear after rewrite\n--- rewritten ---\n{out}\n-----------------"
    );

    // The string-literal occurrence must survive verbatim.
    assert!(
        out.contains(string_fragment),
        "[{lang:?}] string literal `{string_fragment}` must be left untouched\n--- rewritten ---\n{out}\n-----------------"
    );

    // The comment occurrence must survive verbatim.
    assert!(
        out.contains(comment_fragment),
        "[{lang:?}] comment text `{comment_fragment}` must be left untouched\n--- rewritten ---\n{out}\n-----------------"
    );

    // The longer identifier (prefix match only) must survive verbatim and must
    // NOT have been corrupted into a `newName...` form.
    assert!(
        out.contains(longer_identifier),
        "[{lang:?}] longer identifier `{longer_identifier}` must be left untouched (no substring rewrite)\n--- rewritten ---\n{out}\n-----------------"
    );

    // No bare `oldName` identifier token should remain. We check that every
    // residual `oldName` substring is part of the longer identifier, the
    // string fragment, or the comment fragment — i.e. never a standalone token.
    let standalone_remaining = out
        .match_indices("oldName")
        .filter(|&(idx, _)| {
            // A char immediately before/after that is an identifier char means
            // this `oldName` is part of a longer identifier (allowed).
            let before_ok = idx
                .checked_sub(1)
                .and_then(|i| out[..idx].chars().last().map(|_| i))
                .map(|_| {
                    out[..idx]
                        .chars()
                        .last()
                        .map(|c| c.is_alphanumeric() || c == '_')
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            let after = &out[idx + "oldName".len()..];
            let after_ok = after
                .chars()
                .next()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
            // Keep only occurrences that look like *standalone* tokens.
            !(before_ok || after_ok)
        })
        .count();
    // Standalone `oldName` tokens are permitted ONLY when they live inside the
    // string or comment fragments we deliberately planted (2 of them).
    assert_eq!(
        standalone_remaining, 2,
        "[{lang:?}] exactly the string + comment occurrences of standalone `oldName` should remain ({standalone_remaining} found)\n--- rewritten ---\n{out}\n-----------------"
    );
}

#[test]
fn rust_rename() {
    let source = r#"
// calls oldName from the comment
fn old_name_helper() -> u32 { 0 }

fn oldName() -> u32 { 7 }

fn run() -> u32 {
    let _ = "oldName is a magic string";
    let _h = old_name_helper();
    oldName() + oldName()
}
"#;
    // def + 2 call sites = 3 genuine rewrites.
    check(Lang::Rust, source, 3, "old_name_helper", "\"oldName is a magic string\"", "// calls oldName from the comment");
}

#[test]
fn python_rename() {
    let source = r#"
# remember to deprecate oldName eventually
def old_name_helper():
    return 0

def oldName():
    return 7

def run():
    msg = "oldName is a magic string"
    _h = old_name_helper()
    return oldName() + oldName()
"#;
    check(Lang::Python, source, 3, "old_name_helper", "\"oldName is a magic string\"", "# remember to deprecate oldName eventually");
}

#[test]
fn typescript_rename() {
    let source = r#"
// TODO: rename oldName project-wide
function oldNameHelper(): number { return 0; }

function oldName(): number { return 7; }

function run(): number {
    const msg: string = "oldName is a magic string";
    const h = oldNameHelper();
    return oldName() + oldName() + h;
}
"#;
    check(Lang::TypeScript, source, 3, "oldNameHelper", "\"oldName is a magic string\"", "// TODO: rename oldName project-wide");
}

#[test]
fn go_rename() {
    let source = r#"
package main

// oldName is exported; remember the docs
func oldNameHelper() int { return 0 }

func oldName() int { return 7 }

func run() int {
    msg := "oldName is a magic string"
    _ = msg
    h := oldNameHelper()
    return oldName() + oldName() + h
}
"#;
    check(Lang::Go, source, 3, "oldNameHelper", "\"oldName is a magic string\"", "// oldName is exported; remember the docs");
}

#[test]
fn java_rename() {
    let source = r#"
class Service {
    // oldName must stay backward compatible
    int oldNameHelper() { return 0; }

    int oldName() { return 7; }

    int run() {
        String msg = "oldName is a magic string";
        int h = oldNameHelper();
        return oldName() + oldName() + h;
    }
}
"#;
    check(Lang::Java, source, 3, "oldNameHelper", "\"oldName is a magic string\"", "// oldName must stay backward compatible");
}

#[test]
fn c_rename() {
    let source = r#"
/* oldName is part of the public ABI */
int old_name_helper(void) { return 0; }

int oldName(void) { return 7; }

int run(void) {
    const char *msg = "oldName is a magic string";
    int h = old_name_helper();
    (void)msg;
    return oldName() + oldName() + h;
}
"#;
    check(Lang::C, source, 3, "old_name_helper", "\"oldName is a magic string\"", "/* oldName is part of the public ABI */");
}

#[test]
fn ruby_rename() {
    let source = r#"
# keep oldName around for the legacy callers
def old_name_helper
  0
end

def oldName
  7
end

def run
  msg = "oldName is a magic string"
  h = old_name_helper
  oldName + oldName + h
end
"#;
    check(Lang::Ruby, source, 3, "old_name_helper", "\"oldName is a magic string\"", "# keep oldName around for the legacy callers");
}

#[test]
fn javascript_rename() {
    let source = r#"
// oldName is referenced by the old SDK
function oldNameHelper() { return 0; }

function oldName() { return 7; }

function run() {
    const msg = "oldName is a magic string";
    const h = oldNameHelper();
    return oldName() + oldName() + h;
}
"#;
    check(Lang::JavaScript, source, 3, "oldNameHelper", "\"oldName is a magic string\"", "// oldName is referenced by the old SDK");
}
