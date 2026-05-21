#[tokio::main]
async fn main() {
    mosaic_lsp::run_stdio().await;
}
