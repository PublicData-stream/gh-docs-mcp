#[tokio::main]
async fn main() -> gh_docs_core::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    gh_docs_test_support::phase1::run(&args).await
}
