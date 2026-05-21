use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mosaic-serve", about = "HTTP sync server for a Mosaic repository")]
struct Cli {
    /// Path to the directory containing the .mosaic/ subdirectory.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Address to bind. Use 0.0.0.0:7700 for all interfaces.
    #[arg(long, default_value = "127.0.0.1:7700")]
    bind: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    mosaic_server::ensure_repo(&cli.repo)?;
    let addr: std::net::SocketAddr = cli.bind.parse()?;
    let (bound, handle) = mosaic_server::serve(cli.repo.clone(), addr).await?;
    eprintln!("mosaic-serve listening on http://{bound}");
    eprintln!("  repo: {}", cli.repo.display());
    handle.await?;
    Ok(())
}
