//! Foundation Git worker profile for a fixed synthetic bare repository.

#[doc(hidden)]
pub mod sandbox;

pub fn phase0_probe(force_failure: bool) -> std::io::Result<()> {
    sandbox::probe(sandbox::Profile::Git, force_failure)
}
