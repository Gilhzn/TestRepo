use clap::{Parser, Subcommand};
use mosaic_core::chunker::{chunk_and_store, reassemble, Manifest};
use mosaic_core::m1::change::{ChangeBuilder, FileChange, FileKind};
use mosaic_core::m1::identity::Identity;
use mosaic_core::m1::signing::SigningKey;
use mosaic_core::m1_patch::line_graph::{LineGraph, Vertex, VertexId};
use mosaic_core::m1_patch::merge::{three_way_merge, StructuredConflict};
use mosaic_core::m1_patch::patch::{Op, Patch};
use mosaic_core::repo::{Repository, REPO_DIR};
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
}

#[derive(Subcommand)]
enum ImportCmd {
    /// Import a Git repository's history.
    Git { path: PathBuf },
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
        } => run(|| commit(&intent, &file, &branch)),
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
    }
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

fn commit(intent: &str, files: &[PathBuf], branch: &str) -> Result<(), AppError> {
    let mut repo = open_here()?;
    let (identity, key) = repo.load_identity()?;
    let head = repo.refs().get(branch).unwrap_or_default();
    let mut builder = ChangeBuilder::new(identity.clone(), key).intent(intent);
    for h in &head.0 {
        builder = builder.dep(mosaic_core::m1::change::ChangeId(*h));
    }
    for path in files {
        let bytes = fs::read(path)?;
        let kind = if std::str::from_utf8(&bytes).is_ok() {
            FileKind::Text
        } else {
            FileKind::Binary
        };
        builder = builder.file(FileChange {
            path: path.to_string_lossy().into_owned(),
            kind,
            patch: bytes,
            conflicts: Vec::new(),
        });
    }
    let change = builder.build()?;
    let id = repo.commit(change)?;
    repo.advance_branch(branch, id)?;
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
