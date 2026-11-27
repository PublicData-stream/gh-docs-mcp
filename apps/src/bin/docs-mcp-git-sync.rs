fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os();
    let remote = args
        .next()
        .and_then(|arg| {
            std::path::Path::new(&arg)
                .file_name()
                .map(|n| n == "git-remote-docsmcp")
        })
        .unwrap_or(false);
    if docs_mcp_sync::git_child(&args.collect::<Vec<_>>(), remote).is_err() {
        return if remote {
            std::process::ExitCode::FAILURE
        } else {
            std::process::ExitCode::from(125)
        };
    }
    std::process::ExitCode::SUCCESS
}
