use clap::{Parser, Subcommand};
use mosaic_core::chunker::{chunk_and_store, reassemble, Manifest};
use mosaic_core::m1::change::{ChangeBuilder, ChangeId, FileChange, FileKind};
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::SigningKey;
use mosaic_core::m1_patch::line_graph::{LineGraph, Vertex, VertexId};
use mosaic_core::m1_patch::merge::{three_way_merge, StructuredConflict};
use mosaic_core::m1_patch::patch::{Op, Patch};
use mosaic_core::issues::{IssueEventBuilder, IssueEventKind, IssueStatus};
use mosaic_core::repo::{Repository, REPO_DIR};
use mosaic_core::review::{ApprovalBuilder, CommentAnchor, CommentBuilder, Verdict};
use mosaic_core::storage::Cas;
use mosaic_core::Hash;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "mos", version, about = "Mosaic VCS — agent-native version control")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a fresh Mosaic repository in the current directory.
    Init,
    /// Manage the local identity (human or agent).
    #[command(subcommand)]
    Id(IdCmd),
    /// Inspect the change history.
    Log,
    /// Manage branch frontiers.
    #[command(subcommand)]
    Branch(BranchCmd),
    /// Record a new change against the current head.
    Commit {
        #[arg(short, long)]
        intent: String,
        #[arg(short, long)]
        file: Vec<PathBuf>,
        #[arg(short, long, default_value = "main")]
        branch: String,
        /// Commit even if the secret scanner flags likely credentials.
        #[arg(long)]
        allow_secrets: bool,
    },
    /// Store a binary blob via FastCDC and print its manifest hash.
    Put { path: PathBuf },
    /// Restore a blob from a manifest hash.
    Cat {
        hash: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Storage statistics.
    Stats,
    /// Bundle changes for transport to another repo.
    #[command(subcommand)]
    Bundle(BundleCmd),
    /// Import history from another VCS.
    #[command(subcommand)]
    Import(ImportCmd),
    /// Manage remote sync servers.
    #[command(subcommand)]
    Remote(RemoteCmd),
    /// Push local changes to a remote.
    Push {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Pull changes from a remote.
    Pull {
        #[arg(default_value = "origin")]
        remote: String,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Manage server-side trusted signing keys (allowlist).
    #[command(subcommand)]
    Trust(TrustCmd),
    /// Three-way merge a file across the patch + semantic layers.
    Merge {
        /// In-repo path (used to pick the language for semantic analysis).
        path: String,
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        ours: PathBuf,
        #[arg(long)]
        theirs: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print a human-readable explanation of strategy + outcome.
        #[arg(long)]
        explain: bool,
    },
    /// Run an end-to-end demo of two agents editing in parallel and merging cleanly.
    Demo,
    /// Leave a code-review comment on a change.
    Comment {
        /// Hex-encoded change id to comment on.
        change_id: String,
        /// Body of the comment.
        #[arg(short, long)]
        body: String,
        /// Optional file path the comment is anchored to.
        #[arg(short, long)]
        file: Option<String>,
        /// Optional line number (requires --file).
        #[arg(short, long)]
        line: Option<u32>,
        /// Optional parent comment hash (hex) for threading.
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// Approve a change.
    Approve {
        change_id: String,
        #[arg(short, long)]
        body: Option<String>,
    },
    /// Request changes on a change.
    RequestChanges {
        change_id: String,
        #[arg(short, long)]
        body: Option<String>,
    },
    /// Print all review comments + approvals on a change in order.
    Review { change_id: String },
    /// GitHub-style issue tracker.
    #[command(subcommand)]
    Issue(IssueCmd),
    /// Show working tree status vs. branch tip.
    Status {
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Show diff of working tree vs. branch tip.
    Diff {
        path: Option<String>,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Stage files for the next commit (use `.` for everything modified).
    Add {
        paths: Vec<String>,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Unstage files (does not modify working copy).
    Unstage { paths: Vec<String> },
    /// Discard working-copy changes for a file (restore to branch tip).
    Restore {
        path: String,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Garbage-collect unreachable objects.
    Gc {
        /// Show what would be pruned without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move a branch back to the parent(s) of its current tip.
    /// (The tip change stays in the DAG and can be recovered.)
    Undo {
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Delete a branch ref. Commits stay in the DAG until `mos gc`.
    Abandon { branch: String },
    /// Replace the branch tip with a new change that has the same parents
    /// but uses the currently-staged files + a new intent.
    Amend {
        #[arg(short, long)]
        intent: Option<String>,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Squash the current tip into its parent: a new change with the
    /// grandparents as deps and the combined file set.
    Squash {
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Audit log: query everything done by an actor or session.
    #[command(subcommand)]
    Audit(AuditCmd),
    /// Export a branch's history to a Git repository (one-way mirror).
    ExportGit {
        target: PathBuf,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Interactive first-time setup wizard.
    Quickstart {
        #[arg(long)]
        email: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Roll a branch back past everything an agent did in a session.
    /// Dropped changes stay in the DAG (recoverable; pruned by `mos gc`).
    Rollback {
        session_id: String,
        #[arg(short, long, default_value = "main")]
        branch: String,
    },
    /// Subscribe to a branch and poll for changes since you last looked.
    #[command(subcommand)]
    Watch(WatchCmd),
    /// Materialize a branch's files into the working tree (honors sparse).
    Checkout {
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Configure a sparse profile (work with a subset of a large repo).
    #[command(subcommand)]
    Sparse(SparseCmd),
}

#[derive(Subcommand)]
enum SparseCmd {
    /// Show the current sparse profile.
    Show,
    /// Set include/exclude globs (replaces the current profile).
    Set {
        #[arg(short, long)]
        include: Vec<String>,
        #[arg(short, long)]
        exclude: Vec<String>,
    },
    /// Clear the profile (work with everything again).
    Clear,
}

#[derive(Subcommand)]
enum WatchCmd {
    /// Start watching a branch from its current tip.
    Add {
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Stop watching a branch.
    Remove {
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Show new changes since last poll (advances your seen marker).
    Poll {
        #[arg(default_value = "main")]
        branch: String,
    },
    /// Show new changes without advancing your seen marker.
    Peek {
        #[arg(default_value = "main")]
        branch: String,
    },
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Replay every event in a given agent session id.
    Session { session_id: String },
    /// All events for one actor (by identity id, e.g. `human:alice@x.com`).
    Actor { actor_id: String },
    /// Distinct session ids seen so far.
    Sessions,
    /// Print all events ever logged (newest first).
    All,
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Import a Git repository's history.
    Git { path: PathBuf },
}

#[derive(Subcommand)]
enum IssueCmd {
    /// File a new issue (allocates the next number, signs locally).
    Create {
        #[arg(short, long)]
        title: String,
        #[arg(short, long, default_value = "")]
        body: String,
        /// Repeatable label, e.g. `--label bug --label p1`.
        #[arg(short, long)]
        label: Vec<String>,
    },
    /// List all issues with their current status.
    List,
    /// Show an issue and its event stream.
    Show { number: u64 },
    /// Add a comment to an issue.
    Comment {
        number: u64,
        #[arg(short, long)]
        body: String,
    },
    /// Close an issue.
    Close { number: u64 },
    /// Reopen a closed issue.
    Reopen { number: u64 },
    /// Add or remove a label on an issue.
    Label {
        number: u64,
        #[arg(long)]
        add: Option<String>,
        #[arg(long)]
        remove: Option<String>,
    },
}

#[derive(Subcommand)]
enum TrustCmd {
    /// Append a hex-encoded ed25519 public key to the local allowlist.
    Add { pubkey_hex: String },
    /// Show currently trusted keys.
    List,
    /// Remove a key from the allowlist (no-op if missing).
    Remove { pubkey_hex: String },
    /// Print the current local identity's public key (for sharing).
    Me,
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Add a named remote URL.
    Add { name: String, url: String },
    /// Remove a remote.
    Remove { name: String },
    /// List configured remotes.
    List,
}

#[derive(Subcommand)]
enum BundleCmd {
    /// Write a bundle of all local changes (or of a specific branch) to a file.
    Create {
        #[arg(short, long)]
        out: PathBuf,
        #[arg(short, long)]
        branch: Option<String>,
    },
    /// Apply a bundle file into the current repo.
    Apply { path: PathBuf },
    /// Inspect a bundle without applying it.
    Inspect { path: PathBuf },
}

#[derive(Subcommand)]
enum IdCmd {
    /// Print the current identity.
    Show,
    /// Configure a human identity (generates a signing key).
    Setup {
        #[arg(long)]
        email: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum BranchCmd {
    /// List branches and their frontier sizes.
    List,
    /// Show the frontier of a branch as a list of change hashes.
    Show { name: String },
}

fn main() -> ExitCode {
    match Cli::parse().cmd {
        Cmd::Init => run(init),
        Cmd::Id(IdCmd::Show) => run(id_show),
        Cmd::Id(IdCmd::Setup { email, name }) => run(|| id_setup(&email, name.as_deref())),
        Cmd::Log => run(log),
        Cmd::Branch(BranchCmd::List) => run(branch_list),
        Cmd::Branch(BranchCmd::Show { name }) => run(|| branch_show(&name)),
        Cmd::Commit {
            intent,
            file,
            branch,
            allow_secrets,
        } => run(|| commit(&intent, &file, &branch, allow_secrets)),
        Cmd::Put { path } => run(|| put(&path)),
        Cmd::Cat { hash, out } => run(|| cat(&hash, out.as_deref())),
        Cmd::Stats => run(stats),
        Cmd::Bundle(BundleCmd::Create { out, branch }) => {
            run(|| bundle_create(&out, branch.as_deref()))
        }
        Cmd::Bundle(BundleCmd::Apply { path }) => run(|| bundle_apply(&path)),
        Cmd::Bundle(BundleCmd::Inspect { path }) => run(|| bundle_inspect(&path)),
        Cmd::Import(ImportCmd::Git { path }) => run(|| import_git_cmd(&path)),
        Cmd::Remote(RemoteCmd::Add { name, url }) => run(|| remote_add(&name, &url)),
        Cmd::Remote(RemoteCmd::Remove { name }) => run(|| remote_remove(&name)),
        Cmd::Remote(RemoteCmd::List) => run(remote_list),
        Cmd::Push { remote, branch } => run(|| push_cmd(&remote, &branch)),
        Cmd::Pull { remote, branch } => run(|| pull_cmd(&remote, &branch)),
        Cmd::Trust(TrustCmd::Add { pubkey_hex }) => run(|| trust_add(&pubkey_hex)),
        Cmd::Trust(TrustCmd::List) => run(trust_list),
        Cmd::Trust(TrustCmd::Remove { pubkey_hex }) => run(|| trust_remove(&pubkey_hex)),
        Cmd::Trust(TrustCmd::Me) => run(trust_me),
        Cmd::Merge {
            path,
            base,
            ours,
            theirs,
            out,
            explain,
        } => run(|| merge_cmd(&path, &base, &ours, &theirs, out.as_deref(), explain)),
        Cmd::Demo => run(demo),
        Cmd::Comment {
            change_id,
            body,
            file,
            line,
            reply_to,
        } => run(|| comment_cmd(&change_id, &body, file.as_deref(), line, reply_to.as_deref())),
        Cmd::Approve { change_id, body } => {
            run(|| approve_cmd(&change_id, Verdict::Approved, body.as_deref()))
        }
        Cmd::RequestChanges { change_id, body } => {
            run(|| approve_cmd(&change_id, Verdict::RequestedChanges, body.as_deref()))
        }
        Cmd::Review { change_id } => run(|| review_cmd(&change_id)),
        Cmd::Issue(IssueCmd::Create { title, body, label }) => {
            run(|| issue_create_cmd(&title, &body, &label))
        }
        Cmd::Issue(IssueCmd::List) => run(issue_list_cmd),
        Cmd::Issue(IssueCmd::Show { number }) => run(|| issue_show_cmd(number)),
        Cmd::Issue(IssueCmd::Comment { number, body }) => {
            run(|| issue_comment_cmd(number, &body))
        }
        Cmd::Issue(IssueCmd::Close { number }) => {
            run(|| issue_status_cmd(number, IssueStatus::Closed))
        }
        Cmd::Issue(IssueCmd::Reopen { number }) => {
            run(|| issue_status_cmd(number, IssueStatus::Open))
        }
        Cmd::Issue(IssueCmd::Label { number, add, remove }) => {
            run(|| issue_label_cmd(number, add.as_deref(), remove.as_deref()))
        }
        Cmd::Status { branch } => run(|| status_cmd(&branch)),
        Cmd::Diff { path, branch } => run(|| diff_cmd(path.as_deref(), &branch)),
        Cmd::Add { paths, branch } => run(|| add_cmd(&paths, &branch)),
        Cmd::Unstage { paths } => run(|| unstage_cmd(&paths)),
        Cmd::Restore { path, branch } => run(|| restore_cmd(&path, &branch)),
        Cmd::Gc { dry_run } => run(|| gc_cmd(dry_run)),
        Cmd::Undo { branch } => run(|| undo_cmd(&branch)),
        Cmd::Abandon { branch } => run(|| abandon_cmd(&branch)),
        Cmd::Amend { intent, branch } => run(|| amend_cmd(intent.as_deref(), &branch)),
        Cmd::Squash { branch } => run(|| squash_cmd(&branch)),
        Cmd::Audit(AuditCmd::Session { session_id }) => run(|| audit_session(&session_id)),
        Cmd::Audit(AuditCmd::Actor { actor_id }) => run(|| audit_actor(&actor_id)),
        Cmd::Audit(AuditCmd::Sessions) => run(audit_sessions),
        Cmd::Audit(AuditCmd::All) => run(audit_all),
        Cmd::ExportGit { target, branch } => run(|| export_git_cmd(&target, &branch)),
        Cmd::Quickstart { email, name } => {
            run(|| quickstart_cmd(email.as_deref(), name.as_deref()))
        }
        Cmd::Rollback { session_id, branch } => run(|| rollback_cmd(&session_id, &branch)),
        Cmd::Watch(WatchCmd::Add { branch }) => run(|| watch_add(&branch)),
        Cmd::Watch(WatchCmd::Remove { branch }) => run(|| watch_remove(&branch)),
        Cmd::Watch(WatchCmd::Poll { branch }) => run(|| watch_poll(&branch, true)),
        Cmd::Watch(WatchCmd::Peek { branch }) => run(|| watch_poll(&branch, false)),
        Cmd::Checkout { branch } => run(|| checkout_cmd(&branch)),
        Cmd::Sparse(SparseCmd::Show) => run(sparse_show),
        Cmd::Sparse(SparseCmd::Set { include, exclude }) => {
            run(|| sparse_set(&include, &exclude))
        }
        Cmd::Sparse(SparseCmd::Clear) => run(sparse_clear),
    }
}

fn checkout_cmd(branch: &str) -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let repo = Repository::open(&root)?;
    let wc = mosaic_core::working_copy::WorkingCopy::open(&repo, &root);
    let profile = mosaic_core::sparse::SparseProfile::load(&root)?;
    let written = wc.checkout(branch, &profile)?;
    println!(
        "checked out {} file(s) from {branch}{}",
        written.len(),
        if profile.is_active() { " (sparse)" } else { "" }
    );
    for p in written.iter().take(30) {
        println!("  {p}");
    }
    if written.len() > 30 {
        println!("  ... and {} more", written.len() - 30);
    }
    Ok(())
}

fn sparse_show() -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let profile = mosaic_core::sparse::SparseProfile::load(&root)?;
    if !profile.is_active() {
        println!("(no sparse profile — working with everything)");
        return Ok(());
    }
    println!("include:");
    for p in &profile.include {
        println!("  {p}");
    }
    println!("exclude:");
    for p in &profile.exclude {
        println!("  {p}");
    }
    Ok(())
}

fn sparse_set(include: &[String], exclude: &[String]) -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let profile = mosaic_core::sparse::SparseProfile {
        include: include.to_vec(),
        exclude: exclude.to_vec(),
    };
    profile.save(&root)?;
    println!(
        "sparse profile set: {} include, {} exclude pattern(s)",
        include.len(),
        exclude.len()
    );
    println!("  run `mos checkout` to re-materialize the working tree");
    Ok(())
}

fn sparse_clear() -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    mosaic_core::sparse::SparseProfile::default().save(&root)?;
    println!("sparse profile cleared");
    Ok(())
}

fn watch_add(branch: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    mosaic_core::watch::watch(&repo, branch)?;
    println!("watching {branch} from its current tip");
    Ok(())
}

fn watch_remove(branch: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    if mosaic_core::watch::unwatch(&repo, branch)? {
        println!("stopped watching {branch}");
    } else {
        println!("(was not watching {branch})");
    }
    Ok(())
}

fn watch_poll(branch: &str, advance: bool) -> Result<(), AppError> {
    let repo = open_here()?;
    let delta = if advance {
        mosaic_core::watch::poll(&repo, branch)?
    } else {
        mosaic_core::watch::peek(&repo, branch)?
    };
    if delta.is_empty() {
        println!("up to date — no new changes on {branch}");
        return Ok(());
    }
    println!("{} new change(s) on {branch}:", delta.len());
    for id in &delta {
        let change = repo.load_change(id)?;
        println!(
            "  {}  {}  by {}",
            &id.to_hex()[..12],
            change.intent.as_deref().unwrap_or("(no intent)"),
            change.author.display()
        );
    }
    Ok(())
}

fn rollback_cmd(session_id: &str, branch: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    let report = mosaic_core::rollback::rollback_session(&repo, branch, session_id)?;
    if report.dropped.is_empty() {
        println!("nothing to roll back: no changes found for session {session_id}");
        return Ok(());
    }
    println!(
        "rolled back session {session_id} on {branch}: dropped {} change(s)",
        report.dropped.len()
    );
    for h in report.dropped.iter().take(20) {
        println!("  - {}", &h.to_hex()[..12]);
    }
    println!(
        "branch now has {} tip(s); dropped changes remain in the DAG until `mos gc`",
        report.new_frontier.0.len()
    );
    Ok(())
}

fn run(f: impl FnOnce() -> Result<(), AppError>) -> ExitCode {
    match f() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init() -> Result<(), AppError> {
    Repository::init(std::env::current_dir()?)?;
    println!("initialized empty Mosaic repository in {REPO_DIR}/");
    Ok(())
}

fn id_show() -> Result<(), AppError> {
    let repo = open_here()?;
    if !repo.has_identity() {
        println!("no identity configured; run `mos id setup --email <addr>`");
        return Ok(());
    }
    let (id, key) = repo.load_identity()?;
    println!("identity: {}", id.display());
    println!("id:       {}", id.id());
    println!("pubkey:   {}", hex::encode(key.verifying_key().to_bytes()));
    Ok(())
}

fn id_setup(email: &str, name: Option<&str>) -> Result<(), AppError> {
    let repo = open_here()?;
    let identity = Identity::human(email, name.map(str::to_string))?;
    let key = SigningKey::generate();
    repo.save_identity(&identity, &key)?;
    println!("identity configured: {}", identity.display());
    println!("pubkey: {}", hex::encode(key.verifying_key().to_bytes()));
    Ok(())
}

fn log() -> Result<(), AppError> {
    let repo = open_here()?;
    let all = repo.all_change_ids()?;
    if all.is_empty() {
        println!("no changes yet");
        return Ok(());
    }
    let ordered = repo.topo_order(&all);
    for id in ordered {
        let change = repo.load_change(&mosaic_core::m1::change::ChangeId(id))?;
        let intent = change.intent.as_deref().unwrap_or("(no intent)");
        println!("{}  {}  by {}", &id.to_hex()[..12], intent, change.author.display());
    }
    Ok(())
}

fn branch_list() -> Result<(), AppError> {
    let repo = open_here()?;
    let names = repo.refs().list()?;
    if names.is_empty() {
        println!("(no branches)");
        return Ok(());
    }
    for name in names {
        let f = repo.refs().get(&name)?;
        println!("{:<24} {} tip(s)", name, f.0.len());
    }
    Ok(())
}

fn branch_show(name: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    let f = repo.refs().get(name)?;
    if f.0.is_empty() {
        println!("(empty frontier)");
    }
    for h in &f.0 {
        println!("{h}");
    }
    Ok(())
}

fn commit(
    intent: &str,
    files: &[PathBuf],
    branch: &str,
    allow_secrets: bool,
) -> Result<(), AppError> {
    use mosaic_core::working_copy::{StagedIndex, WorkingCopy};

    let root = std::env::current_dir()?;
    let mut repo = Repository::open(&root)?;
    let (identity, key) = repo.load_identity()?;
    let head = repo.refs().get(branch).unwrap_or_default();
    let mut builder = ChangeBuilder::new(identity.clone(), key).intent(intent);
    for h in &head.0 {
        builder = builder.dep(mosaic_core::m1::change::ChangeId(*h));
    }

    // Two modes: explicit -f flags use those files; otherwise pick up the
    // staged index and commit everything in it.
    let mut file_changes: Vec<FileChange> = Vec::new();
    if !files.is_empty() {
        for path in files {
            let bytes = fs::read(path)?;
            let kind = if std::str::from_utf8(&bytes).is_ok() {
                FileKind::Text
            } else {
                FileKind::Binary
            };
            file_changes.push(FileChange {
                path: path.to_string_lossy().into_owned(),
                kind,
                patch: bytes,
                conflicts: Vec::new(),
            });
        }
    } else {
        let wc = WorkingCopy::open(&repo, &root);
        let index = StagedIndex::load(&root)?;
        if index.paths.is_empty() {
            return Err(AppError::Msg(
                "nothing to commit: stage files first with `mos add .` or pass -f <path>".into(),
            ));
        }
        file_changes = wc.build_staged_file_changes(&index)?;
    }

    // Secret scan before anything is written to history.
    let mut findings = Vec::new();
    for fc in &file_changes {
        findings.extend(mosaic_core::secrets::scan_file(&fc.path, &fc.patch));
    }
    if !findings.is_empty() {
        eprintln!("⚠ secret scanner flagged {} potential credential(s):", findings.len());
        for f in &findings {
            eprintln!("    {}:{}  [{}]  {}", f.path, f.line, f.rule, f.excerpt);
        }
        if !allow_secrets {
            return Err(AppError::Msg(
                "commit blocked. Remove the secrets, or re-run with `--allow-secrets` if these are false positives.".into(),
            ));
        }
        eprintln!("  (--allow-secrets given; committing anyway)");
    }

    for fc in file_changes {
        builder = builder.file(fc);
    }
    let change = builder.build()?;
    let id = repo.commit(change)?;
    repo.advance_branch(branch, id)?;
    // Clear the staged index after a successful commit.
    if files.is_empty() {
        let mut index = mosaic_core::working_copy::StagedIndex::load(&root)?;
        index.clear();
        index.save(&root)?;
    }
    println!("committed {} on {branch}", &id.to_hex()[..16]);
    Ok(())
}

fn put(path: &PathBuf) -> Result<(), AppError> {
    let repo = open_here()?;
    let file = fs::File::open(path)?;
    let manifest = chunk_and_store(repo.cas(), file)?;
    let encoded = bincode::serialize(&manifest).map_err(AppError::bincode)?;
    let hash = repo.cas().put(&encoded)?;
    println!("{hash}");
    println!(
        "  {} bytes across {} chunks",
        manifest.total_size,
        manifest.chunks.len()
    );
    Ok(())
}

fn cat(hash_str: &str, out: Option<&std::path::Path>) -> Result<(), AppError> {
    let repo = open_here()?;
    let hash = Hash::from_hex(hash_str)?;
    let manifest_bytes = repo.cas().get(&hash)?;
    let manifest: Manifest =
        bincode::deserialize(&manifest_bytes).map_err(AppError::bincode)?;
    let bytes = reassemble(repo.cas(), &manifest)?;
    match out {
        Some(p) => fs::write(p, bytes)?,
        None => io::Write::write_all(&mut io::stdout().lock(), &bytes)?,
    }
    Ok(())
}

fn stats() -> Result<(), AppError> {
    let repo = open_here()?;
    let mut blob_count = 0u64;
    let mut blob_bytes = 0u64;
    for outer in fs::read_dir(repo.cas().root())? {
        let outer = outer?;
        if !outer.file_type()?.is_dir() {
            continue;
        }
        for inner in fs::read_dir(outer.path())? {
            let inner = inner?;
            blob_count += 1;
            blob_bytes += inner.metadata()?.len();
        }
    }
    let change_count = repo.all_change_ids()?.len();
    let branches = repo.refs().list()?.len();
    println!("blobs:       {blob_count} ({blob_bytes} bytes on disk)");
    println!("changes:     {change_count}");
    println!("branches:    {branches}");
    Ok(())
}

fn bundle_create(out: &PathBuf, branch: Option<&str>) -> Result<(), AppError> {
    let repo = open_here()?;
    let ids: Vec<mosaic_core::m1::change::ChangeId> = match branch {
        Some(name) => {
            let tips = repo.refs().get(name)?;
            let empty = mosaic_core::m1_dag::refs::Frontier::default();
            mosaic_core::sync::missing_changes_for(&repo, &empty, &tips)
        }
        None => repo
            .all_change_ids()?
            .into_iter()
            .map(mosaic_core::m1::change::ChangeId)
            .collect(),
    };
    let ordered = repo.topo_order(&ids.iter().map(|c| c.0).collect());
    let ordered: Vec<mosaic_core::m1::change::ChangeId> = ordered
        .into_iter()
        .map(mosaic_core::m1::change::ChangeId)
        .collect();
    let bundle = match branch {
        Some(name) => mosaic_core::sync::build_bundle_for_branch(&repo, name, &ordered)?,
        None => mosaic_core::sync::build_bundle(&repo, &ordered)?,
    };
    let bytes = bundle.encode()?;
    fs::write(out, &bytes)?;
    println!(
        "wrote bundle {} ({} changes, {} blobs, {} bytes, {} branch advances)",
        out.display(),
        bundle.changes.len(),
        bundle.blobs.len(),
        bytes.len(),
        bundle.branch_advances.len(),
    );
    Ok(())
}

fn bundle_apply(path: &PathBuf) -> Result<(), AppError> {
    let mut repo = open_here()?;
    let bytes = fs::read(path)?;
    let bundle = mosaic_core::sync::Bundle::decode(&bytes)?;
    let report = mosaic_core::sync::apply_bundle(&mut repo, &bundle)?;
    println!(
        "applied {} change(s), skipped {} duplicate(s)",
        report.applied.len(),
        report.skipped.len()
    );
    for id in &report.applied {
        println!("  + {}", &id.to_hex()[..16]);
    }
    Ok(())
}

fn remotes_file() -> Result<PathBuf, AppError> {
    Ok(std::env::current_dir()?.join(".mosaic").join("remotes.json"))
}

fn load_remotes() -> Result<std::collections::BTreeMap<String, String>, AppError> {
    let path = remotes_file()?;
    if !path.exists() {
        return Ok(std::collections::BTreeMap::new());
    }
    let raw = fs::read_to_string(&path)?;
    serde_json::from_str(&raw).map_err(|e| AppError::Encode(e.to_string()))
}

fn save_remotes(map: &std::collections::BTreeMap<String, String>) -> Result<(), AppError> {
    let path = remotes_file()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(map).map_err(|e| AppError::Encode(e.to_string()))?;
    fs::write(path, raw)?;
    Ok(())
}

fn remote_add(name: &str, url: &str) -> Result<(), AppError> {
    let mut map = load_remotes()?;
    let url = url.trim_end_matches('/').to_string();
    map.insert(name.to_string(), url.clone());
    save_remotes(&map)?;
    println!("added remote {name} -> {url}");
    Ok(())
}

fn remote_remove(name: &str) -> Result<(), AppError> {
    let mut map = load_remotes()?;
    if map.remove(name).is_none() {
        return Err(AppError::Msg(format!("no such remote: {name}")));
    }
    save_remotes(&map)?;
    println!("removed remote {name}");
    Ok(())
}

fn remote_list() -> Result<(), AppError> {
    let map = load_remotes()?;
    if map.is_empty() {
        println!("(no remotes configured)");
        return Ok(());
    }
    for (name, url) in map {
        println!("{name:<20} {url}");
    }
    Ok(())
}

fn resolve_remote(name: &str) -> Result<String, AppError> {
    let map = load_remotes()?;
    map.get(name)
        .cloned()
        .ok_or_else(|| AppError::Msg(format!("no such remote: {name}")))
}

fn push_cmd(remote: &str, branch: &str) -> Result<(), AppError> {
    let base = resolve_remote(remote)?;
    let repo = open_here()?;
    let tips = repo.refs().get(branch)?;
    let have_url = format!("{base}/api/v1/branches/{branch}");
    let client = reqwest::blocking::Client::new();
    let server_have = client
        .get(&have_url)
        .send()
        .ok()
        .and_then(|r| r.json::<mosaic_server::BranchSummary>().ok())
        .map(|s| {
            let mut frontier = mosaic_core::m1_dag::refs::Frontier::default();
            for h in s.tips {
                if let Ok(hh) = Hash::from_hex(&h) {
                    frontier.0.insert(hh);
                }
            }
            frontier
        })
        .unwrap_or_default();

    let needed = mosaic_core::sync::missing_changes_for(&repo, &server_have, &tips);
    if needed.is_empty() {
        println!("nothing to push; remote {remote} is up to date on {branch}");
        return Ok(());
    }
    let mut bundle = mosaic_core::sync::build_bundle(&repo, &needed)?;
    bundle.branch_advances.insert(branch.to_string(), tips);
    let body = bundle.encode()?;
    let resp = client
        .post(format!("{base}/api/v1/bundle"))
        .body(body)
        .send()
        .map_err(|e| AppError::Msg(format!("push: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| AppError::Msg(format!("push response: {e}")))?;
    if !status.is_success() {
        let server_msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or(text);
        return Err(AppError::Msg(format!(
            "push rejected by remote ({status}): {server_msg}"
        )));
    }
    let report: mosaic_server::ApplyResponse = serde_json::from_str(&text)
        .map_err(|e| AppError::Msg(format!("push response parse: {e}")))?;
    println!(
        "pushed {} change(s) to {remote}/{branch} (skipped {})",
        report.applied.len(),
        report.skipped.len()
    );
    Ok(())
}

fn pull_cmd(remote: &str, branch: &str) -> Result<(), AppError> {
    let base = resolve_remote(remote)?;
    let mut repo = open_here()?;
    let local = repo.refs().get(branch).unwrap_or_default();
    let have: Vec<String> = local.0.iter().map(|h| h.to_hex()).collect();
    let url = format!(
        "{base}/api/v1/missing?branch={branch}&have={have}",
        have = have.join(",")
    );
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| AppError::Msg(format!("pull: {e}")))?;
    if !resp.status().is_success() {
        return Err(AppError::Msg(format!("pull failed: {}", resp.status())));
    }
    let bytes = resp
        .bytes()
        .map_err(|e| AppError::Msg(format!("pull bytes: {e}")))?;
    let bundle = mosaic_core::sync::Bundle::decode(&bytes)?;
    let report = mosaic_core::sync::apply_bundle(&mut repo, &bundle)?;
    println!(
        "pulled {} change(s) from {remote}/{branch} (skipped {})",
        report.applied.len(),
        report.skipped.len()
    );
    for id in &report.applied {
        println!("  + {}", &id.to_hex()[..16]);
    }
    Ok(())
}

fn trust_path() -> Result<PathBuf, AppError> {
    Ok(std::env::current_dir()?
        .join(".mosaic")
        .join("allowed_signers.txt"))
}

fn load_trusted() -> Result<Vec<String>, AppError> {
    let path = trust_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(fs::read_to_string(&path)?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

fn save_trusted(keys: &[String]) -> Result<(), AppError> {
    let path = trust_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::from("# Mosaic allowed signers (ed25519 public keys, hex)\n");
    for k in keys {
        body.push_str(k);
        body.push('\n');
    }
    fs::write(path, body)?;
    Ok(())
}

fn validate_pubkey_hex(s: &str) -> Result<(), AppError> {
    let bytes = hex::decode(s).map_err(|e| AppError::Msg(format!("invalid hex: {e}")))?;
    if bytes.len() != 32 {
        return Err(AppError::Msg(format!(
            "expected 32-byte ed25519 pubkey, got {} bytes",
            bytes.len()
        )));
    }
    Ok(())
}

fn trust_add(pubkey_hex: &str) -> Result<(), AppError> {
    validate_pubkey_hex(pubkey_hex)?;
    let mut keys = load_trusted()?;
    if keys.iter().any(|k| k == pubkey_hex) {
        println!("(already trusted)");
        return Ok(());
    }
    keys.push(pubkey_hex.to_string());
    save_trusted(&keys)?;
    println!("trusted {}", pubkey_hex);
    Ok(())
}

fn trust_remove(pubkey_hex: &str) -> Result<(), AppError> {
    let mut keys = load_trusted()?;
    let before = keys.len();
    keys.retain(|k| k != pubkey_hex);
    if keys.len() == before {
        println!("(not in allowlist)");
    } else {
        save_trusted(&keys)?;
        println!("removed {}", pubkey_hex);
    }
    Ok(())
}

fn trust_list() -> Result<(), AppError> {
    let keys = load_trusted()?;
    if keys.is_empty() {
        println!("(no trusted keys — server runs in open mode)");
        return Ok(());
    }
    for k in keys {
        println!("{k}");
    }
    Ok(())
}

fn trust_me() -> Result<(), AppError> {
    let repo = open_here()?;
    let (identity, key) = repo.load_identity()?;
    let hex_key = hex::encode(key.verifying_key().to_bytes());
    println!("identity: {}", identity.display());
    println!("pubkey:   {}", hex_key);
    println!();
    println!("share this pubkey with the server admin who runs `mos trust add <pubkey>`");
    Ok(())
}

fn merge_cmd(
    path: &str,
    base: &PathBuf,
    ours: &PathBuf,
    theirs: &PathBuf,
    out: Option<&std::path::Path>,
    explain: bool,
) -> Result<(), AppError> {
    let base_s = fs::read_to_string(base)?;
    let ours_s = fs::read_to_string(ours)?;
    let theirs_s = fs::read_to_string(theirs)?;
    let creator = Hash::of(path.as_bytes());
    let lang = mosaic_core::ast::Lang::from_path(path);
    let result = mosaic_core::merge_strategies::merge_text_file(
        &creator, lang, &base_s, &ours_s, &theirs_s,
    )?;

    let merged: String = result
        .merged_lines
        .iter()
        .map(|l| format!("{l}\n"))
        .collect();
    match out {
        Some(p) => {
            fs::write(p, &merged)?;
            println!("wrote merged result to {}", p.display());
        }
        None => {
            println!("--- merged result ---");
            print!("{merged}");
            println!("--- end ---");
        }
    }

    if explain {
        println!();
        print!("{}", result.explain());
        return Ok(());
    }

    println!();
    println!("patch-level conflicts: {}", result.patch_conflicts.len());
    if !result.semantic_hints.is_empty() {
        println!("semantic hints:");
        for hint in &result.semantic_hints {
            match hint {
                mosaic_core::semantic::SemanticHint::DefinitionRenamed { from, to, kind } => {
                    println!("  rename {kind} {from} -> {to}");
                }
                mosaic_core::semantic::SemanticHint::DefinitionEdited { name, kind } => {
                    println!("  edit   {kind} {name}");
                }
                mosaic_core::semantic::SemanticHint::DefinitionAdded { name, kind } => {
                    println!("  add    {kind} {name}");
                }
                mosaic_core::semantic::SemanticHint::DefinitionRemoved { name, kind } => {
                    println!("  remove {kind} {name}");
                }
                mosaic_core::semantic::SemanticHint::CallSiteUsesOldName {
                    old_name,
                    new_name,
                    line,
                    column,
                } => {
                    println!("  callsite at {line}:{column} uses old name {old_name} (now {new_name})");
                }
            }
        }
    }
    Ok(())
}

fn import_git_cmd(path: &PathBuf) -> Result<(), AppError> {
    let mut repo = open_here()?;
    let (importer, key) = repo.load_identity()?;
    println!("importing git history from {}...", path.display());
    let report = mosaic_core::import_git::import_git(&mut repo, path, &importer, &key)?;
    println!(
        "imported {} change(s), skipped {}",
        report.imported.len(),
        report.skipped.len()
    );
    for (sha, mosaic_id) in report.imported.iter().take(20) {
        println!("  git {} -> mosaic {}", &sha[..12], &mosaic_id.to_hex()[..16]);
    }
    if report.imported.len() > 20 {
        println!("  ... and {} more", report.imported.len() - 20);
    }
    Ok(())
}

fn bundle_inspect(path: &PathBuf) -> Result<(), AppError> {
    let bytes = fs::read(path)?;
    let bundle = mosaic_core::sync::Bundle::decode(&bytes)?;
    println!("bundle: {} changes, {} blobs", bundle.changes.len(), bundle.blobs.len());
    for change in &bundle.changes {
        let intent = change.intent.as_deref().unwrap_or("(no intent)");
        println!(
            "  {}  {}  by {}",
            &change.id().to_hex()[..12],
            intent,
            change.author.display()
        );
    }
    Ok(())
}

fn demo() -> Result<(), AppError> {
    println!("=== Mosaic parallel-merge demonstration ===");
    println!();
    println!("Three actors (a human and two AI agents) edit the same file");
    println!("on independent branches; Mosaic merges all three cleanly without");
    println!("text-marker conflicts, then surfaces remaining ambiguities as data.");
    println!();

    let base_text: &[&[u8]] = &[
        b"# payments module",
        b"",
        b"function chargeCard(amount) {",
        b"  return api.charge(amount);",
        b"}",
    ];
    let creator = Hash::of(b"demo-root");
    let base = LineGraph::from_lines(&creator, base_text);
    print_graph("base", &base);

    let anchor_blank = VertexId::derive(&creator, 1, base_text[1]);
    let anchor_open = VertexId::derive(&creator, 2, base_text[2]);
    let anchor_close = VertexId::derive(&creator, 4, base_text[4]);

    let alice_vertex = make_vertex(b"alice-change", b"// Alice: log the call");
    let alice_patch = Patch::from_ops(vec![Op::InsertAfter {
        anchor: anchor_blank,
        before: anchor_open,
        vertex: alice_vertex,
    }]);

    let bob_vertex = make_vertex(b"bob-change", b"// Bob: track fraud score");
    let bob_patch = Patch::from_ops(vec![Op::InsertAfter {
        anchor: anchor_close,
        before: base.sink_id(),
        vertex: bob_vertex,
    }]);

    let carol_vertex = make_vertex(b"carol-change", b"// Carol: also annotate");
    let carol_patch = Patch::from_ops(vec![Op::InsertAfter {
        anchor: anchor_blank,
        before: anchor_open,
        vertex: carol_vertex,
    }]);

    println!("--- commutation checks ---");
    println!(
        "  alice vs bob  (disjoint regions)   -> commute = {}",
        mosaic_core::m1_patch::merge::commute(&alice_patch, &bob_patch)
    );
    println!(
        "  alice vs carol (same anchor slot)  -> commute = {}",
        mosaic_core::m1_patch::merge::commute(&alice_patch, &carol_patch)
    );
    println!();

    let merged_ab = three_way_merge(&base, &alice_patch, &bob_patch)?;
    println!("--- merge: alice + bob (clean) ---");
    print_graph("result", &merged_ab.graph);
    println!(
        "  {} conflicts (auto-resolved by commutation)",
        merged_ab.conflicts.len()
    );
    println!();

    let merged_ac = three_way_merge(&base, &alice_patch, &carol_patch)?;
    println!("--- merge: alice + carol (concurrent insert at same slot) ---");
    print_graph("result", &merged_ac.graph);
    println!(
        "  {} structured conflict(s) — repo is still valid:",
        merged_ac.conflicts.len()
    );
    for c in &merged_ac.conflicts {
        match c {
            StructuredConflict::ConcurrentInsert { anchor, ours, theirs, .. } => {
                println!(
                    "    ConcurrentInsert at anchor {}: ours={}, theirs={}",
                    short(&anchor.0),
                    short(&ours.0),
                    short(&theirs.0)
                );
            }
            StructuredConflict::EditVsDelete { target, deleter, editor } => {
                println!(
                    "    EditVsDelete at {}: deleter={:?}, editor={:?}",
                    short(&target.0),
                    deleter,
                    editor
                );
            }
        }
    }
    println!();
    println!("Key property: the merged graph is a valid acyclic graph that flattens");
    println!("to a deterministic total order. The conflict is data — an AI agent");
    println!("can read it programmatically and decide which sibling to kill.");
    Ok(())
}

fn parse_change_id(s: &str) -> Result<ChangeId, AppError> {
    Ok(ChangeId(Hash::from_hex(s)?))
}

fn comment_cmd(
    change_id_hex: &str,
    body: &str,
    file: Option<&str>,
    line: Option<u32>,
    reply_to: Option<&str>,
) -> Result<(), AppError> {
    let repo = open_here()?;
    let (identity, key) = repo.load_identity()?;
    let cid = parse_change_id(change_id_hex)?;
    let anchor = match (file, line) {
        (Some(path), Some(n)) => CommentAnchor::Line {
            path: path.to_string(),
            line: n,
        },
        (Some(path), None) => CommentAnchor::File {
            path: path.to_string(),
        },
        (None, Some(_)) => {
            return Err(AppError::Msg(
                "--line requires --file".into(),
            ));
        }
        (None, None) => CommentAnchor::Change,
    };
    let mut builder = CommentBuilder::new(cid, identity, key)
        .anchor(anchor)
        .body(body.to_string());
    if let Some(parent_hex) = reply_to {
        let parent = Hash::from_hex(parent_hex)?;
        builder = builder.reply_to(parent);
    }
    let comment = builder.build()?;
    let id = repo.add_comment(&comment)?;
    println!("comment {} on {}", &id.to_hex()[..16], &cid.to_hex()[..16]);
    Ok(())
}

fn approve_cmd(change_id_hex: &str, verdict: Verdict, body: Option<&str>) -> Result<(), AppError> {
    let repo = open_here()?;
    let (identity, key) = repo.load_identity()?;
    let cid = parse_change_id(change_id_hex)?;
    let mut builder = ApprovalBuilder::new(cid, identity, key, verdict);
    if let Some(b) = body {
        builder = builder.body(b.to_string());
    }
    let approval = builder.build()?;
    let id = repo.add_approval(&approval)?;
    let label = match verdict {
        Verdict::Approved => "approved",
        Verdict::RequestedChanges => "requested changes on",
        Verdict::Commented => "commented on",
    };
    println!("{} {} (approval {})", label, &cid.to_hex()[..16], &id.to_hex()[..16]);
    Ok(())
}

fn review_cmd(change_id_hex: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    let cid = parse_change_id(change_id_hex)?;
    let comments = repo.comments_for(&cid)?;
    let approvals = repo.approvals_for(&cid)?;

    // Merge by timestamp ascending so we get a chronological review log.
    #[derive(Clone)]
    enum Item {
        Comment(mosaic_core::review::Comment),
        Approval(mosaic_core::review::Approval),
    }
    impl Item {
        fn ts(&self) -> mosaic_core::m1::change::Tai64N {
            match self {
                Item::Comment(c) => c.ts,
                Item::Approval(a) => a.ts,
            }
        }
    }
    let mut items: Vec<Item> = Vec::new();
    items.extend(comments.into_iter().map(Item::Comment));
    items.extend(approvals.into_iter().map(Item::Approval));
    items.sort_by_key(|i| i.ts());

    if items.is_empty() {
        println!("(no review activity on {})", &cid.to_hex()[..16]);
        return Ok(());
    }

    println!("review of {}:", &cid.to_hex()[..16]);
    for it in items {
        match it {
            Item::Comment(c) => {
                let where_str = match &c.anchor {
                    CommentAnchor::Change => "(change)".to_string(),
                    CommentAnchor::File { path } => format!("(file {path})"),
                    CommentAnchor::Line { path, line } => format!("({path}:{line})"),
                };
                println!(
                    "  comment {} by {} {}: {}",
                    &c.id().to_hex()[..12],
                    c.author.display(),
                    where_str,
                    c.body
                );
            }
            Item::Approval(a) => {
                let v = match a.verdict {
                    Verdict::Approved => "APPROVED",
                    Verdict::RequestedChanges => "REQUESTED CHANGES",
                    Verdict::Commented => "COMMENTED",
                };
                let body = a.body.as_deref().unwrap_or("");
                println!(
                    "  {} {} by {}{}",
                    v,
                    &a.id().to_hex()[..12],
                    a.reviewer.display(),
                    if body.is_empty() {
                        String::new()
                    } else {
                        format!(": {body}")
                    }
                );
            }
        }
    }
    Ok(())
}

fn issue_create_cmd(title: &str, body: &str, labels: &[String]) -> Result<(), AppError> {
    let repo = open_here()?;
    let (identity, key) = repo.load_identity()?;
    let issue = repo.create_issue(title, body, identity, key, labels.to_vec())?;
    println!("opened issue #{}: {}", issue.number, issue.title);
    if !issue.labels.is_empty() {
        println!("  labels: {}", issue.labels.join(", "));
    }
    Ok(())
}

fn issue_list_cmd() -> Result<(), AppError> {
    let repo = open_here()?;
    let issues = repo.list_issues()?;
    if issues.is_empty() {
        println!("(no issues)");
        return Ok(());
    }
    for issue in issues {
        let status = repo.issue_status(issue.number)?;
        let tag = match status {
            IssueStatus::Open => "OPEN",
            IssueStatus::Closed => "CLOSED",
        };
        println!(
            "#{:<4} [{:<6}] {}  by {}",
            issue.number,
            tag,
            issue.title,
            issue.author.display()
        );
    }
    Ok(())
}

fn issue_show_cmd(number: u64) -> Result<(), AppError> {
    let repo = open_here()?;
    let issue = repo.get_issue(number)?;
    let status = repo.issue_status(number)?;
    let tag = match status {
        IssueStatus::Open => "OPEN",
        IssueStatus::Closed => "CLOSED",
    };
    println!("#{} [{}] {}", issue.number, tag, issue.title);
    println!("opened by {}", issue.author.display());
    if !issue.labels.is_empty() {
        println!("labels: {}", issue.labels.join(", "));
    }
    if !issue.body.is_empty() {
        println!();
        println!("{}", issue.body);
    }
    let events = repo.issue_events(number)?;
    if !events.is_empty() {
        println!();
        println!("--- activity ---");
        for ev in events {
            let who = ev.author.display();
            match &ev.kind {
                IssueEventKind::Comment { body } => println!("  {who} commented: {body}"),
                IssueEventKind::StatusChanged { to } => {
                    let s = match to {
                        IssueStatus::Open => "reopened",
                        IssueStatus::Closed => "closed",
                    };
                    println!("  {who} {s} this issue");
                }
                IssueEventKind::Labeled { label } => println!("  {who} added label {label}"),
                IssueEventKind::Unlabeled { label } => println!("  {who} removed label {label}"),
                IssueEventKind::Referenced { change } => {
                    println!("  {who} referenced change {change}")
                }
            }
        }
    }
    Ok(())
}

fn add_issue_event(number: u64, kind: IssueEventKind) -> Result<(), AppError> {
    let repo = open_here()?;
    // Ensure the issue exists before signing an event for it.
    let _ = repo.get_issue(number)?;
    let (identity, key) = repo.load_identity()?;
    let event = IssueEventBuilder::new(number, identity, key, kind).build()?;
    repo.add_issue_event(&event)?;
    Ok(())
}

fn issue_comment_cmd(number: u64, body: &str) -> Result<(), AppError> {
    add_issue_event(number, IssueEventKind::Comment { body: body.to_string() })?;
    println!("commented on issue #{number}");
    Ok(())
}

fn issue_status_cmd(number: u64, to: IssueStatus) -> Result<(), AppError> {
    add_issue_event(number, IssueEventKind::StatusChanged { to })?;
    match to {
        IssueStatus::Open => println!("reopened issue #{number}"),
        IssueStatus::Closed => println!("closed issue #{number}"),
    }
    Ok(())
}

fn issue_label_cmd(
    number: u64,
    add: Option<&str>,
    remove: Option<&str>,
) -> Result<(), AppError> {
    if add.is_none() && remove.is_none() {
        return Err(AppError::Msg("specify --add <label> or --remove <label>".into()));
    }
    if let Some(label) = add {
        add_issue_event(number, IssueEventKind::Labeled { label: label.to_string() })?;
        println!("added label {label} to issue #{number}");
    }
    if let Some(label) = remove {
        add_issue_event(number, IssueEventKind::Unlabeled { label: label.to_string() })?;
        println!("removed label {label} from issue #{number}");
    }
    Ok(())
}

fn print_graph(label: &str, graph: &LineGraph) {
    println!("{label}:");
    for line in graph.flatten() {
        let txt = std::str::from_utf8(line).unwrap_or("<binary>");
        println!("  {txt}");
    }
}

fn make_vertex(creator_tag: &[u8], line: &[u8]) -> Vertex {
    let creator = Hash::of(creator_tag);
    Vertex {
        id: VertexId::derive(&creator, 0, line),
        bytes: line.to_vec(),
        alive: true,
    }
}

fn short(h: &Hash) -> String {
    h.to_hex()[..8].to_string()
}

fn open_here() -> Result<Repository, AppError> {
    Repository::open(std::env::current_dir()?).map_err(Into::into)
}

fn status_cmd(branch: &str) -> Result<(), AppError> {
    use mosaic_core::working_copy::{FileState, StagedIndex, WorkingCopy};
    let root = std::env::current_dir()?;
    let repo = Repository::open(&root)?;
    let wc = WorkingCopy::open(&repo, &root);
    let index = StagedIndex::load(&root)?;
    let entries = wc.status(branch, &index)?;

    if entries.is_empty() {
        println!("clean working tree (branch {branch})");
        return Ok(());
    }

    let mut staged: Vec<&str> = Vec::new();
    let mut unstaged: Vec<(&str, &str)> = Vec::new();
    let mut untracked: Vec<&str> = Vec::new();
    let mut removed: Vec<&str> = Vec::new();

    for e in &entries {
        let tag = match e.state {
            FileState::Modified => "modified",
            FileState::Untracked => "untracked",
            FileState::Removed => "removed",
            FileState::Unmodified => "unmodified",
        };
        match (e.state, e.staged) {
            (FileState::Untracked, false) => untracked.push(&e.path),
            (FileState::Removed, _) => removed.push(&e.path),
            (_, true) => staged.push(&e.path),
            _ => unstaged.push((tag, &e.path)),
        }
    }

    if !staged.is_empty() {
        println!("Staged for commit:");
        for p in &staged {
            println!("    {p}");
        }
        println!();
    }
    if !unstaged.is_empty() {
        println!("Changes not staged for commit:");
        for (tag, p) in &unstaged {
            println!("    {tag:<10} {p}");
        }
        println!("  (use `mos add <file>` or `mos add .` to stage them)");
        println!();
    }
    if !untracked.is_empty() {
        println!("Untracked files:");
        for p in &untracked {
            println!("    {p}");
        }
        println!("  (use `mos add <file>` to start tracking)");
        println!();
    }
    if !removed.is_empty() {
        println!("Deleted from working tree:");
        for p in &removed {
            println!("    {p}");
        }
        println!();
    }
    println!("Branch: {branch}");
    Ok(())
}

fn diff_cmd(path: Option<&str>, branch: &str) -> Result<(), AppError> {
    use mosaic_core::working_copy::{FileState, StagedIndex, WorkingCopy};
    let root = std::env::current_dir()?;
    let repo = Repository::open(&root)?;
    let wc = WorkingCopy::open(&repo, &root);

    let targets: Vec<String> = match path {
        Some(p) => vec![p.to_string()],
        None => {
            let index = StagedIndex::load(&root)?;
            wc.status(branch, &index)?
                .into_iter()
                .filter(|e| matches!(e.state, FileState::Modified | FileState::Untracked))
                .map(|e| e.path)
                .collect()
        }
    };
    if targets.is_empty() {
        println!("(no differences)");
        return Ok(());
    }
    for p in targets {
        let text = wc.diff_text(branch, &p)?;
        if text.is_empty() {
            continue;
        }
        println!("--- {p} (branch {branch})");
        println!("+++ {p} (working tree)");
        print!("{text}");
    }
    Ok(())
}

fn add_cmd(paths: &[String], branch: &str) -> Result<(), AppError> {
    use mosaic_core::working_copy::{StagedIndex, WorkingCopy};
    let root = std::env::current_dir()?;
    let repo = Repository::open(&root)?;
    let wc = WorkingCopy::open(&repo, &root);
    let mut index = StagedIndex::load(&root)?;

    if paths.is_empty() || paths.iter().any(|p| p == ".") {
        let added = wc.stage_all_modified(branch, &mut index)?;
        index.save(&root)?;
        println!("staged {added} file(s)");
        return Ok(());
    }
    for p in paths {
        index.stage(p.clone());
    }
    index.save(&root)?;
    println!("staged {} file(s)", paths.len());
    Ok(())
}

fn unstage_cmd(paths: &[String]) -> Result<(), AppError> {
    use mosaic_core::working_copy::StagedIndex;
    let root = std::env::current_dir()?;
    let mut index = StagedIndex::load(&root)?;
    let mut removed = 0;
    for p in paths {
        if index.unstage(p) {
            removed += 1;
        }
    }
    index.save(&root)?;
    println!("unstaged {removed} file(s)");
    Ok(())
}

fn restore_cmd(path: &str, branch: &str) -> Result<(), AppError> {
    use mosaic_core::working_copy::WorkingCopy;
    let root = std::env::current_dir()?;
    let repo = Repository::open(&root)?;
    let wc = WorkingCopy::open(&repo, &root);
    let restored = wc.restore(branch, path)?;
    if restored {
        println!("restored {path} to branch tip");
    } else {
        println!("nothing to restore for {path} (not in branch tip)");
    }
    Ok(())
}

fn current_tip(repo: &Repository, branch: &str) -> Result<Option<ChangeId>, AppError> {
    let frontier = repo.refs().get(branch).unwrap_or_default();
    if frontier.0.is_empty() {
        return Ok(None);
    }
    // Pick the most recent tip via topological order over the frontier.
    let mut set = std::collections::BTreeSet::new();
    for h in &frontier.0 {
        set.insert(*h);
    }
    let ordered = repo.topo_order(&set);
    Ok(ordered.last().map(|h| ChangeId(*h)))
}

fn undo_cmd(branch: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    let tip = current_tip(&repo, branch)?
        .ok_or_else(|| AppError::Msg(format!("branch {branch} has no tip to undo")))?;
    let change = repo.load_change(&tip)?;
    let new_frontier = if change.deps.is_empty() {
        // Tip is a root commit — undo means: empty branch.
        mosaic_core::m1_dag::refs::Frontier::default()
    } else {
        let mut f = mosaic_core::m1_dag::refs::Frontier::default();
        for d in &change.deps {
            f.0.insert(d.0);
        }
        f
    };
    repo.refs().put(branch, &new_frontier)?;
    println!(
        "undone: branch {branch} moved back to {} parent(s)",
        new_frontier.0.len()
    );
    println!("  (the change {} is still in the DAG; `mos gc` will prune if unreferenced)", &tip.to_hex()[..12]);
    Ok(())
}

fn abandon_cmd(branch: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    repo.refs().delete(branch)?;
    println!("abandoned branch {branch}");
    println!("  (its commits stay in the DAG; `mos gc` will prune if unreachable)");
    Ok(())
}

fn amend_cmd(new_intent: Option<&str>, branch: &str) -> Result<(), AppError> {
    use mosaic_core::working_copy::{StagedIndex, WorkingCopy};
    let root = std::env::current_dir()?;
    let mut repo = Repository::open(&root)?;
    let tip = current_tip(&repo, branch)?
        .ok_or_else(|| AppError::Msg(format!("branch {branch} has no tip to amend")))?;
    let old = repo.load_change(&tip)?;

    let (identity, key) = repo.load_identity()?;
    let intent = new_intent
        .map(str::to_string)
        .or(old.intent.clone())
        .unwrap_or_else(|| "(amended)".into());

    let mut builder = ChangeBuilder::new(identity, key).intent(intent);
    for d in &old.deps {
        builder = builder.dep(*d);
    }

    // If anything is staged, use those files; otherwise reuse the tip's body.
    let wc = WorkingCopy::open(&repo, &root);
    let index = StagedIndex::load(&root)?;
    let body: Vec<FileChange> = if !index.paths.is_empty() {
        wc.build_staged_file_changes(&index)?
    } else {
        old.body.clone()
    };
    for fc in body {
        builder = builder.file(fc);
    }

    let new_change = builder.build()?;
    let new_id = repo.commit(new_change)?;

    // Replace the branch's tip: drop the old tip, insert the new one.
    let mut frontier = repo.refs().get(branch).unwrap_or_default();
    frontier.0.remove(&tip.0);
    frontier.0.insert(new_id.0);
    repo.refs().put(branch, &frontier)?;

    // Clear staged index if we consumed it.
    let mut idx = mosaic_core::working_copy::StagedIndex::load(&root)?;
    idx.clear();
    idx.save(&root)?;

    println!(
        "amended {} -> {} on {branch}",
        &tip.to_hex()[..12],
        &new_id.to_hex()[..12]
    );
    Ok(())
}

fn squash_cmd(branch: &str) -> Result<(), AppError> {
    let mut repo = open_here()?;
    let tip = current_tip(&repo, branch)?
        .ok_or_else(|| AppError::Msg(format!("branch {branch} has no tip to squash")))?;
    let child = repo.load_change(&tip)?;
    if child.deps.is_empty() {
        return Err(AppError::Msg("nothing to squash: tip has no parent".into()));
    }
    if child.deps.len() != 1 {
        return Err(AppError::Msg(
            "squash only supported on a tip with a single parent (linear history)".into(),
        ));
    }
    let parent_id = child.deps[0];
    let parent = repo.load_change(&parent_id)?;

    let (identity, key) = repo.load_identity()?;
    let intent = match (&parent.intent, &child.intent) {
        (Some(p), Some(c)) => format!("{p} + {c}"),
        (Some(s), None) | (None, Some(s)) => s.clone(),
        _ => "(squashed)".into(),
    };

    // Merge file sets: later (child) wins on path collisions.
    let mut by_path: std::collections::BTreeMap<String, FileChange> =
        parent
            .body
            .iter()
            .cloned()
            .map(|fc| (fc.path.clone(), fc))
            .collect();
    for fc in child.body {
        by_path.insert(fc.path.clone(), fc);
    }

    let mut builder = ChangeBuilder::new(identity, key).intent(intent);
    for d in &parent.deps {
        builder = builder.dep(*d);
    }
    for fc in by_path.into_values() {
        builder = builder.file(fc);
    }
    let new_change = builder.build()?;
    let new_id = repo.commit(new_change)?;

    let mut frontier = repo.refs().get(branch).unwrap_or_default();
    frontier.0.remove(&tip.0);
    frontier.0.remove(&parent_id.0);
    frontier.0.insert(new_id.0);
    repo.refs().put(branch, &frontier)?;

    println!(
        "squashed {} + {} -> {} on {branch}",
        &parent_id.to_hex()[..12],
        &tip.to_hex()[..12],
        &new_id.to_hex()[..12]
    );
    Ok(())
}

fn quickstart_cmd(email: Option<&str>, name: Option<&str>) -> Result<(), AppError> {
    use std::path::Path;
    println!("╭──────────────────────────────────────────────────────╮");
    println!("│   Mosaic quickstart                                  │");
    println!("│   The 5-step path from zero to your first commit.    │");
    println!("╰──────────────────────────────────────────────────────╯");
    println!();

    let cwd = std::env::current_dir()?;
    let already_init = cwd.join(".mosaic").exists();

    if already_init {
        println!("✓ step 1 (init): already a Mosaic repo at {}", cwd.display());
    } else {
        println!("→ step 1: initialising a fresh repo at {}", cwd.display());
        init()?;
    }
    println!();

    let repo = Repository::open(&cwd)?;
    if repo.has_identity() {
        let (id, _) = repo.load_identity()?;
        println!("✓ step 2 (identity): already configured as {}", id.display());
    } else {
        let resolved_email = email.map(str::to_string).unwrap_or_else(|| {
            std::env::var("EMAIL")
                .or_else(|_| std::env::var("GIT_AUTHOR_EMAIL"))
                .unwrap_or_else(|_| "you@example.com".to_string())
        });
        let resolved_name = name.map(str::to_string);
        println!(
            "→ step 2: setting up identity {} ({})",
            resolved_name.as_deref().unwrap_or("(no display name)"),
            resolved_email
        );
        id_setup(&resolved_email, resolved_name.as_deref())?;
    }
    println!();

    let readme_path = "MOSAIC_QUICKSTART.md";
    let placeholder = format!(
        "# Welcome to Mosaic\n\n\
         This file was created by `mos quickstart` to give you something to commit.\n\
         Feel free to edit or delete it.\n\n\
         - Run `mos status` to see your working-tree state.\n\
         - Run `mos add .` then `mos commit -i \"...\"` to record a change.\n\
         - Run `mos log` to see history; `mos branch list` to see branches.\n\
         - Visit `http://localhost:7700/` (after `mosaic-serve --repo .`)\n  \
           for the dashboard.\n"
    );
    if !Path::new(readme_path).exists() {
        std::fs::write(readme_path, placeholder)?;
        println!("→ step 3: wrote {readme_path} for you to commit");
    } else {
        println!("✓ step 3 ({readme_path}): already exists");
    }
    println!();

    // Show status, then stage + commit + log if nothing committed yet.
    println!("→ step 4: staging and committing");
    let added = {
        use mosaic_core::working_copy::{StagedIndex, WorkingCopy};
        let repo = Repository::open(&cwd)?;
        let wc = WorkingCopy::open(&repo, &cwd);
        let mut index = StagedIndex::load(&cwd)?;
        let n = wc.stage_all_modified("main", &mut index)?;
        index.save(&cwd)?;
        n
    };
    println!("  staged {added} file(s)");
    if added > 0 {
        commit("welcome to mosaic", &[], "main", false)?;
    } else {
        println!("  (nothing new to commit)");
    }
    println!();

    println!("✓ step 5: you're all set. Next:");
    println!("    mos status        # see modified files");
    println!("    mos log           # see history");
    println!("    mos demo          # see the parallel-merge thesis in action");
    println!("    mosaic-serve --repo . --bind 127.0.0.1:7700");
    println!("    # then open http://127.0.0.1:7700/ for the landing page,");
    println!("    # http://127.0.0.1:7700/dashboard for stats + history.");
    Ok(())
}

fn export_git_cmd(target: &PathBuf, branch: &str) -> Result<(), AppError> {
    let repo = open_here()?;
    println!("exporting branch {branch} to {}...", target.display());
    let report = mosaic_core::git_export::export(&repo, branch, target)?;
    println!(
        "exported {} change(s); head -> {}",
        report.commits_written.len(),
        report.head_git_sha.as_deref().unwrap_or("(none)")
    );
    for (mid, gsha) in report.commits_written.iter().take(20) {
        println!("  {} -> {}", &mid.to_hex()[..12], &gsha[..12]);
    }
    if report.commits_written.len() > 20 {
        println!("  ... and {} more", report.commits_written.len() - 20);
    }
    Ok(())
}

fn audit_session(session_id: &str) -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let log = mosaic_core::audit::AuditLog::open(&root)?;
    let events = log.replay_session(session_id)?;
    if events.is_empty() {
        println!("(no events for session {session_id})");
        return Ok(());
    }
    println!("Replay of session {session_id} — {} event(s):", events.len());
    for (i, ev) in events.iter().enumerate() {
        println!(
            "  [{i}] {:?} by {} at {:?}",
            ev.action,
            ev.actor.display(),
            ev.ts
        );
    }
    Ok(())
}

fn audit_actor(actor_id: &str) -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let log = mosaic_core::audit::AuditLog::open(&root)?;
    let events = log.by_actor(actor_id)?;
    if events.is_empty() {
        println!("(no events for actor {actor_id})");
        return Ok(());
    }
    println!("Events for {actor_id} — {}:", events.len());
    for ev in events {
        println!(
            "  {:?}  session={}",
            ev.action,
            ev.session_id.unwrap_or_else(|| "(none)".into())
        );
    }
    Ok(())
}

fn audit_sessions() -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let log = mosaic_core::audit::AuditLog::open(&root)?;
    let sessions = log.sessions()?;
    if sessions.is_empty() {
        println!("(no session-tagged events yet)");
        return Ok(());
    }
    for s in sessions {
        println!("{s}");
    }
    Ok(())
}

fn audit_all() -> Result<(), AppError> {
    let root = std::env::current_dir()?;
    let log = mosaic_core::audit::AuditLog::open(&root)?;
    println!("{} total event(s)", log.count()?);
    Ok(())
}

fn gc_cmd(dry_run: bool) -> Result<(), AppError> {
    use mosaic_core::gc::{collect, GcMode};
    let repo = open_here()?;
    let mode = if dry_run { GcMode::DryRun } else { GcMode::Apply };
    let report = collect(&repo, mode)?;

    println!("live changes: {}", report.live_changes);
    println!("live blobs:   {}", report.live_blobs);
    if dry_run {
        println!(
            "would prune:  {} changes, {} blobs, {} bytes",
            report.pruned_changes.len(),
            report.pruned_blobs.len(),
            report.bytes_freed,
        );
    } else {
        println!(
            "pruned:       {} changes, {} blobs, {} bytes freed",
            report.pruned_changes.len(),
            report.pruned_blobs.len(),
            report.bytes_freed,
        );
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum AppError {
    #[error("{0}")]
    Msg(String),
    #[error(transparent)]
    Core(#[from] mosaic_core::Error),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("encode: {0}")]
    Encode(String),
}

impl AppError {
    fn bincode(e: bincode::Error) -> Self {
        Self::Encode(e.to_string())
    }
}
