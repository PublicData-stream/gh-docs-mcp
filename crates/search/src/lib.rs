//! Generation-scoped exploration and the confined search worker boundary.

pub mod exploration;
#[doc(hidden)]
pub mod worker;

pub fn phase0_probe(force_failure: bool) -> std::io::Result<()> {
    docs_mcp_git::sandbox::probe(docs_mcp_git::sandbox::Profile::Search, force_failure)
}
