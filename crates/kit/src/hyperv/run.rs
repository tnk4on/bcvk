//! hyperv run — Start a persistent VM using Hyper-V (TBD).

use clap::Parser;
use color_eyre::Result;

/// Options for `hyperv run`.
#[derive(Parser, Debug)]
pub struct HypervRunOpts {
    /// Container image reference
    pub image: String,
}

pub fn run(_opts: HypervRunOpts) -> Result<()> {
    todo!("hyperv persistent VM run")
}
