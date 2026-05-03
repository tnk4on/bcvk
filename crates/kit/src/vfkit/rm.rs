//! vm rm — Remove a persistent VM and its metadata.

use std::fs;

use clap::Parser;
use color_eyre::Result;
use tracing::info;

use super::VmMetadata;

#[derive(Parser, Debug)]
pub struct VmRmOpts {
    /// VM name
    pub name: String,
    /// Force removal even if running
    #[clap(short, long)]
    pub force: bool,
}

pub fn run(opts: VmRmOpts) -> Result<()> {
    let meta = VmMetadata::load(&opts.name)?;

    if meta.is_alive() {
        if !opts.force {
            color_eyre::eyre::bail!(
                "VM '{}' is running. Stop it first or use --force", opts.name
            );
        }
        info!("force stopping VM '{}'...", opts.name);
        crate::vfkit::stop::run(&opts.name)?;
    }

    if !meta.efi_store.is_empty() {
        let _ = fs::remove_file(&meta.efi_store);
    }
    if !meta.serial_log.is_empty() {
        let _ = fs::remove_file(&meta.serial_log);
    }

    let vms_dir = VmMetadata::vms_dir();
    let _ = fs::remove_file(vms_dir.join(format!("{}-gvproxy.sock", meta.name)));
    let _ = fs::remove_file(vms_dir.join(format!("{}-gvproxy-svc.sock", meta.name)));

    VmMetadata::remove(&opts.name);
    println!("Removed '{}'", opts.name);
    Ok(())
}
