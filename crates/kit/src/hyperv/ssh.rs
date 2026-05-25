//! hyperv ssh — SSH into a running persistent VM (TBD).

use clap::Parser;
use color_eyre::Result;

/// Options for `hyperv ssh`.
#[derive(Parser, Debug)]
pub struct HypervSshOpts {
    /// VM name
    pub name: String,
}

pub fn run(_opts: HypervSshOpts) -> Result<()> {
    todo!("hyperv persistent VM ssh")
}
