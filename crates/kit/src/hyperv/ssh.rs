//! hyperv ssh — SSH into a running persistent VM.

use clap::Parser;
use color_eyre::{eyre::bail, Result};

use super::vm;
use super::VmMetadata;

/// Options for `vm ssh`.
#[derive(Parser, Debug)]
pub struct HypervSshOpts {
    /// VM name
    pub name: String,

    /// Additional SSH arguments
    #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

pub fn run(opts: HypervSshOpts) -> Result<()> {
    let meta = VmMetadata::load(&opts.name)?;

    let state = vm::get_vm_state(&meta.vm_name)?;
    if !state.contains("Running") && !state.contains("Starting") {
        bail!(
            "VM '{}' is not running (state: {})",
            opts.name,
            state.to_lowercase()
        );
    }

    let key_path = std::path::Path::new(&meta.ssh_key);

    if opts.args.is_empty() {
        crate::vm_helpers::run_ssh_interactive(meta.ssh_port, key_path, "root")?;
    } else {
        let combined = shlex::try_join(opts.args.iter().map(|s| s.as_str()))
            .map_err(|e| color_eyre::eyre::eyre!("failed to escape SSH args: {}", e))?;
        let status =
            crate::vm_helpers::run_ssh_command(meta.ssh_port, key_path, "root", &combined)?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    }
    Ok(())
}
