//! hyperv rm — Remove a persistent VM and its metadata (TBD).

use clap::Parser;
use color_eyre::Result;

/// Options for `hyperv rm`.
#[derive(Parser, Debug)]
pub struct HypervRmOpts {
    /// VM name
    pub name: String,
}

pub fn run(_opts: HypervRmOpts) -> Result<()> {
    todo!("hyperv persistent VM rm")
}
