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

    /// PEM certificate chain. When given together with --tls-key, the server
    /// terminates HTTPS in-process (native TLS, no reverse proxy needed).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// PEM private key matching --tls-cert.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    mosaic_server::ensure_repo(&cli.repo)?;
    let addr: std::net::SocketAddr = cli.bind.parse()?;

    match (cli.tls_cert.as_ref(), cli.tls_key.as_ref()) {
        (Some(cert), Some(key)) => {
            let (bound, task) = mosaic_server::serve_tls(cli.repo.clone(), addr, cert, key)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> {
                    format!("TLS startup failed: {e}").into()
                })?;
            eprintln!("mosaic-serve listening on https://{bound}");
            eprintln!("  repo: {}", cli.repo.display());
            task.await?;
        }
        _ => {
            let (bound, handle) = mosaic_server::serve(cli.repo.clone(), addr).await?;
            eprintln!("mosaic-serve listening on http://{bound}");
            eprintln!("  repo: {}", cli.repo.display());
            handle.await?;
        }
    }
    Ok(())
}
