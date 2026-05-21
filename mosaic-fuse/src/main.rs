use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mosaic-mount",
    about = "Mount a Mosaic branch as a read-only FUSE filesystem"
)]
struct Cli {
    /// Path to the Mosaic repository (the directory containing .mosaic/).
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Branch whose latest state to materialize.
    #[arg(long, default_value = "main")]
    branch: String,

    /// Mount point (must already exist).
    mount_point: PathBuf,

    /// Add AllowOther mount option (requires user_allow_other in
    /// /etc/fuse.conf if running unprivileged).
    #[arg(long)]
    allow_other: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let fs = mosaic_fuse::MosaicFs::from_branch(&cli.repo, &cli.branch)?;
    eprintln!(
        "mosaic-mount: branch={} file_count={} dir_count={}",
        cli.branch,
        fs.file_count(),
        fs.dir_count(),
    );
    eprintln!("mounted at {}", cli.mount_point.display());

    let mut options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("mosaic".into()),
        fuser::MountOption::AutoUnmount,
    ];
    if cli.allow_other {
        options.push(fuser::MountOption::AllowOther);
    }
    fuser::mount2(fs, &cli.mount_point, &options)?;
    Ok(())
}
