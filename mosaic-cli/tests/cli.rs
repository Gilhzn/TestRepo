//! End-to-end CLI tests for the cross-repo ergonomics commands (`clone`,
//! `branch merge`) and the `commit -m` alias. These drive the real `mos`
//! binary so the working-tree, bundle, and ref plumbing are all exercised.

use std::path::Path;
use std::process::Command;

fn mos(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(env!("CARGO_BIN_EXE_mos"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run mos");
    if !out.status.success() {
        panic!(
            "mos {:?} failed:\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    out
}

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

/// `commit -m` (Git muscle memory) works as an alias for `--intent`, a
/// whole-repo `bundle create` carries the branch ref, and `mos clone <bundle>`
/// materializes a working tree in a fresh directory.
#[test]
fn clone_from_bundle_materializes_working_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    mos(&src, &["init"]);
    mos(&src, &["id", "setup", "--email", "a@example.com", "--name", "A"]);
    write(&src.join("hello.txt"), "hello mosaic\n");
    mos(&src, &["add", "."]);
    // `-m` alias for `--intent`.
    mos(&src, &["commit", "-m", "initial commit"]);

    let bundle = tmp.path().join("repo.bundle");
    // No `-b`: a whole-repo bundle must still carry the `main` ref.
    mos(&src, &["bundle", "create", "-o", bundle.to_str().unwrap()]);

    // Clone into a brand-new directory.
    mos(tmp.path(), &["clone", bundle.to_str().unwrap(), "dest"]);
    let cloned = tmp.path().join("dest").join("hello.txt");
    assert!(cloned.exists(), "clone did not materialize the working tree");
    assert_eq!(std::fs::read_to_string(cloned).unwrap(), "hello mosaic\n");
}

/// `mos branch merge a b --into main` unions divergent branches; the divergent
/// file is 3-way merged so neither branch's edit is dropped.
#[test]
fn branch_merge_unions_divergent_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    mos(repo, &["init"]);
    mos(repo, &["id", "setup", "--email", "a@example.com", "--name", "A"]);
    write(&repo.join("f.txt"), "line1\nPLACEHOLDER\nline3\n");
    mos(repo, &["add", "."]);
    mos(repo, &["commit", "-m", "base"]);

    // Fork feat-a from main, commit AAA.
    mos(repo, &["branch", "merge", "main", "--into", "feat-a"]);
    write(&repo.join("f.txt"), "line1\nAAA\nline3\n");
    mos(repo, &["add", "."]);
    mos(repo, &["commit", "-b", "feat-a", "-m", "a"]);

    // Fork feat-b from main (restoring the base first), commit BBB.
    mos(repo, &["branch", "merge", "main", "--into", "feat-b"]);
    mos(repo, &["checkout", "main"]);
    write(&repo.join("f.txt"), "line1\nBBB\nline3\n");
    mos(repo, &["add", "."]);
    mos(repo, &["commit", "-b", "feat-b", "-m", "b"]);

    // Merge both feature branches into main.
    let out = mos(repo, &["branch", "merge", "feat-a", "feat-b", "--into", "main"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("frontier now has 2 tip(s)"),
        "expected a 2-tip union, got:\n{stdout}"
    );

    let merged = std::fs::read_to_string(repo.join("f.txt")).unwrap();
    assert!(merged.contains("AAA"), "feat-a's edit was dropped:\n{merged}");
    assert!(merged.contains("BBB"), "feat-b's edit was dropped:\n{merged}");
}

/// Two branches each add a distinct block whose bodies share identical lines.
/// The merge must keep each block's body intact (not fuse the shared lines into
/// one and leave a block bodyless) so the result stays syntactically valid.
#[test]
fn branch_merge_keeps_concurrent_blocks_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();

    mos(repo, &["init"]);
    mos(repo, &["id", "setup", "--email", "a@example.com", "--name", "A"]);
    write(&repo.join("calc.py"), "def main(op):\n    pass\n");
    mos(repo, &["add", "."]);
    mos(repo, &["commit", "-m", "base"]);

    mos(repo, &["branch", "merge", "main", "--into", "feat-add"]);
    write(
        &repo.join("calc.py"),
        "def main(op):\n    if op == \"add\":\n        print(compute())\n        return 0\n    pass\n",
    );
    mos(repo, &["add", "."]);
    mos(repo, &["commit", "-b", "feat-add", "-m", "add"]);

    mos(repo, &["branch", "merge", "main", "--into", "feat-sub"]);
    mos(repo, &["checkout", "main"]);
    write(
        &repo.join("calc.py"),
        "def main(op):\n    if op == \"sub\":\n        print(compute())\n        return 0\n    pass\n",
    );
    mos(repo, &["add", "."]);
    mos(repo, &["commit", "-b", "feat-sub", "-m", "sub"]);

    mos(repo, &["branch", "merge", "feat-add", "feat-sub", "--into", "main"]);
    let merged = std::fs::read_to_string(repo.join("calc.py")).unwrap();

    // Both guards present.
    assert!(merged.contains("\"add\""), "add block dropped:\n{merged}");
    assert!(merged.contains("\"sub\""), "sub block dropped:\n{merged}");
    // Each block keeps its own body — the shared body lines must NOT collapse
    // to a single copy (which left a bodyless `if` → broken code).
    let bodies = merged.matches("print(compute())").count();
    assert_eq!(bodies, 2, "a block was left bodyless:\n{merged}");
}

/// `mos merge --apply-renames`: one side renames a function, the other adds a
/// caller of the old name. The resolved output must auto-rewrite that call site
/// to the new name (semantic resolution, not just a hint).
#[test]
fn merge_apply_renames_rewrites_call_sites() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write(&d.join("base.py"), "def charge_card(x):\n    return x\n");
    write(&d.join("ours.py"), "def stripe_charge(x):\n    return x\n");
    write(
        &d.join("theirs.py"),
        "def charge_card(x):\n    return x\n\n\ndef refund(x):\n    return charge_card(x)\n",
    );
    let out = d.join("resolved.py");
    mos(
        d,
        &[
            "merge",
            "--base", "base.py",
            "--ours", "ours.py",
            "--theirs", "theirs.py",
            "--apply-renames",
            "--out", out.to_str().unwrap(),
            "pay.py",
        ],
    );
    let resolved = std::fs::read_to_string(&out).unwrap();
    assert!(
        resolved.contains("return stripe_charge(x)"),
        "call site was not rewritten to the new name:\n{resolved}"
    );
    assert!(
        !resolved.contains("charge_card"),
        "old name still present after --apply-renames:\n{resolved}"
    );
}
