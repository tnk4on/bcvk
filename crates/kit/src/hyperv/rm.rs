//! hyperv rm — Remove a persistent VM and its metadata.

use clap::Parser;
use color_eyre::{eyre::bail, Result};
use tracing::info;

use super::vm;
use super::VmMetadata;

/// Options for `vm rm`.
#[derive(Parser, Debug)]
pub struct HypervRmOpts {
    /// VM name
    pub name: String,

    /// Force removal without confirmation
    #[clap(short, long)]
    pub force: bool,
}

pub fn run(opts: HypervRmOpts) -> Result<()> {
    let meta = VmMetadata::load(&opts.name)?;

    let state = vm::get_vm_state(&meta.vm_name).unwrap_or_default();
    if state.contains("Running") && !opts.force {
        bail!(
            "VM '{}' is running. Use --force to remove, or stop it first with 'bcvk vm stop {}'",
            opts.name,
            opts.name
        );
    }

    super::kill_vm_service(&meta);
    vm::remove_vm(&meta.vm_name)?;
    info!("removed Hyper-V VM: {}", meta.vm_name);

    if !meta.vhdx_path.is_empty() {
        let _ = std::fs::remove_file(&meta.vhdx_path);
    }

    let key_path = std::path::Path::new(&meta.ssh_key);
    let _ = std::fs::remove_file(key_path);
    let pub_path = std::path::PathBuf::from(format!("{}.pub", meta.ssh_key));
    let _ = std::fs::remove_file(pub_path);

    if !meta.switch_name.is_empty() {
        vm::remove_internal_switch(&meta.switch_name);
    }

    VmMetadata::remove(&opts.name);
    println!("Removed VM '{}'", opts.name);
    Ok(())
}
