//! vm ssh — SSH into a running persistent VM.

use clap::Parser;
use color_eyre::{Result, eyre::bail};
use super::VmMetadata;
use crate::run_ephemeral_macos::run_ssh_interactive;

#[derive(Parser, Debug)]
pub struct VmSshOpts {
    /// VM name
    pub name: String,
}

pub fn run(opts: VmSshOpts) -> Result<()> {
    let vm = VmMetadata::load(&opts.name)?;
    if !vm.is_alive() {
        bail!("VM '{}' is not running", opts.name);
    }
    let key_path = std::path::Path::new(&vm.ssh_key);
    run_ssh_interactive(vm.ssh_port, key_path, &vm.ssh_user)?;
    Ok(())
}
