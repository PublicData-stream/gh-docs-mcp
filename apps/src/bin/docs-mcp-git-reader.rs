fn main() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    docs_mcp_git::reader::worker(&args)
}
