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
