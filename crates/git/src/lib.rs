//! Native object access and worker confinement for bare Git repositories.

mod process;
pub mod reader;
#[doc(hidden)]
pub mod sandbox;
pub mod store;

pub fn phase0_probe(force_failure: bool) -> std::io::Result<()> {
    sandbox::probe(sandbox::Profile::Git, force_failure)
}
