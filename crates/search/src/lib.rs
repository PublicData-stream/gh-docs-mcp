//! Phase 0 qualification of the stdin-only search worker.

pub fn phase0_probe(force_failure: bool) -> std::io::Result<()> {
    gh_docs_git::sandbox::probe(gh_docs_git::sandbox::Profile::Search, force_failure)
}
