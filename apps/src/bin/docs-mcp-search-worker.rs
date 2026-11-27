use docs_mcp_core::Result;
use docs_mcp_search::worker;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => worker::run(false)?,
        [flag] if flag == "--fail-setup" => worker::run(true)?,
        [flag] if flag == "--probe-denials" => {
            docs_mcp_git::sandbox::probe(docs_mcp_git::sandbox::Profile::FramedSearch, false)?
        }
        _ => return Err("invalid search worker arguments".into()),
    }
    Ok(())
}
