//! Generation-scoped exploration and the confined search worker boundary.

pub mod exploration;

pub fn phase0_probe(force_failure: bool) -> std::io::Result<()> {
    gh_docs_git::sandbox::probe(gh_docs_git::sandbox::Profile::Search, force_failure)
}
