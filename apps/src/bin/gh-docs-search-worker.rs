use gh_docs_core::Result;
use gh_docs_search::worker;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => worker::run(false)?,
        [flag] if flag == "--fail-setup" => worker::run(true)?,
        [flag] if flag == "--probe-denials" => {
            gh_docs_git::sandbox::probe(gh_docs_git::sandbox::Profile::FramedSearch, false)?
        }
        _ => return Err("invalid search worker arguments".into()),
    }
    Ok(())
}
