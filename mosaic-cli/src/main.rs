use clap::{Parser, Subcommand};
use mosaic_core::chunker::{chunk_and_store, reassemble, Manifest};
use mosaic_core::storage::{Cas, FsCas};
use mosaic_core::Hash;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const REPO_DIR: &str = ".mosaic";
const CAS_DIR: &str = "objects";
const MANIFEST_DIR: &str = "manifests";

#[derive(Parser)]
#[command(name = "mos", version, about = "Mosaic VCS")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a fresh Mosaic repository in the current directory.
    Init,
    /// Store a file: chunk + CAS, print the manifest hash.
    Put {
        path: PathBuf,
    },
    /// Restore a file from a manifest hash to stdout (or a path).
    Cat {
        hash: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Show storage stats.
    Stats,
}

fn main() -> ExitCode {
    match Cli::parse().cmd {
        Cmd::Init => run(init),
        Cmd::Put { path } => run(|| put(&path)),
        Cmd::Cat { hash, out } => run(|| cat(&hash, out.as_deref())),
        Cmd::Stats => run(stats),
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
    let repo = Path::new(REPO_DIR);
    if repo.exists() {
        return Err(AppError::Msg(format!("{REPO_DIR} already exists")));
    }
    fs::create_dir_all(repo.join(CAS_DIR))?;
    fs::create_dir_all(repo.join(MANIFEST_DIR))?;
    println!("initialized empty Mosaic repository in {REPO_DIR}/");
    Ok(())
}

fn put(path: &Path) -> Result<(), AppError> {
    let (cas, manifest_dir) = open_repo()?;
    let file = fs::File::open(path)?;
    let manifest = chunk_and_store(&cas, file)?;
    let encoded = bincode::serialize(&manifest).map_err(AppError::bincode)?;
    let hash = cas.put(&encoded)?;
    fs::write(manifest_dir.join(hash.to_hex()), &encoded)?;
    println!("{hash}");
    println!(
        "  {} bytes across {} chunks",
        manifest.total_size,
        manifest.chunks.len()
    );
    Ok(())
}

fn cat(hash_str: &str, out: Option<&Path>) -> Result<(), AppError> {
    let (cas, _) = open_repo()?;
    let hash = Hash::from_hex(hash_str)?;
    let manifest_bytes = cas.get(&hash)?;
    let manifest: Manifest =
        bincode::deserialize(&manifest_bytes).map_err(AppError::bincode)?;
    let bytes = reassemble(&cas, &manifest)?;
    match out {
        Some(p) => fs::write(p, bytes)?,
        None => io::Write::write_all(&mut io::stdout().lock(), &bytes)?,
    }
    Ok(())
}

fn stats() -> Result<(), AppError> {
    let (cas, _) = open_repo()?;
    let mut count = 0u64;
    let mut bytes = 0u64;
    for outer in fs::read_dir(cas.root())? {
        let outer = outer?;
        if !outer.file_type()?.is_dir() {
            continue;
        }
        for inner in fs::read_dir(outer.path())? {
            let inner = inner?;
            count += 1;
            bytes += inner.metadata()?.len();
        }
    }
    println!("objects: {count}");
    println!("on-disk bytes: {bytes}");
    Ok(())
}

fn open_repo() -> Result<(FsCas, PathBuf), AppError> {
    let repo = Path::new(REPO_DIR);
    if !repo.exists() {
        return Err(AppError::Msg(format!(
            "no {REPO_DIR}/ here; run `mos init` first"
        )));
    }
    let cas = FsCas::open(repo.join(CAS_DIR))?;
    Ok((cas, repo.join(MANIFEST_DIR)))
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
