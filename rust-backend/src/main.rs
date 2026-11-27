#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let token = std::env::var("GUVERCIN_API_TOKEN")
        .ok()
        .filter(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("Set GUVERCIN_API_TOKEN to a randomly generated 64-character hex string"))?;
    let port = std::env::var("GUVERCIN_API_PORT").unwrap_or_else(|_| "5500".into()).parse()?;
    let port = rust_backend::run(None, token, port)
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    // If GUVERCIN_KEEP_ALIVE is set, block until Ctrl+C so the server stays alive for manual testing
    if std::env::var("GUVERCIN_KEEP_ALIVE").is_ok() {
        tracing::info!(
            "Backend running on port {}. GUVERCIN_KEEP_ALIVE set; blocking until Ctrl+C",
            port
        );
        tokio::signal::ctrl_c().await?;
    }

    Ok(())
}
