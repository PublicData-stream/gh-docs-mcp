#[tokio::main]
async fn main() -> docs_mcp_core::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    docs_mcp_test_support::phase1::run(&args).await
}
