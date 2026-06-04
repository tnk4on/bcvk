//! hyperv rm — Remove a persistent VM and its metadata.

use clap::Parser;
use color_eyre::Result;
use tracing::info;

use super::vm;
use super::VmMetadata;

/// Options for `vm rm`.
#[derive(Parser, Debug)]
pub struct HypervRmOpts {
    /// VM name
    pub name: String,

    /// Force removal without confirmation (also stops running VMs)
    #[clap(long, short = 'f')]
    pub force: bool,

    /// Stop domain if it's running (implied by --force)
    #[clap(long)]
    pub stop: bool,
}

pub fn run(opts: HypervRmOpts) -> Result<()> {
    let meta = VmMetadata::load(&opts.name)?;

    let state = vm::get_vm_state(&meta.vm_name).unwrap_or_default();

    // Check if VM is running
    if state.contains("Running") {
        // --force implies --stop
        if opts.stop || opts.force {
            println!("Stopping running VM '{}'...", opts.name);
            super::kill_vm_service(&meta);
            vm::turn_off_vm(&meta.vm_name)?;
        } else {
            return Err(color_eyre::eyre::eyre!(
                "VM '{}' is running. Use --stop or --force to remove a running VM, or stop it first.",
                opts.name
            ));
        }
    }

    // Confirmation prompt
    if !opts.force {
        println!(
            "This will permanently delete VM '{}' and its data:",
            opts.name
        );
        if !meta.image.is_empty() {
            println!("  Image: {}", meta.image);
        }
        println!("  Disk: {}", meta.vhdx_path);
        println!("  Status: {}", state.to_lowercase());
        println!();
        println!("Are you sure? This cannot be undone. Use --force to skip this prompt.");
        return Ok(());
    }

    println!("Removing VM '{}'...", opts.name);

    super::kill_vm_service(&meta);
    vm::remove_vm(&meta.vm_name)?;
    info!("removed Hyper-V VM: {}", meta.vm_name);

    let vms_dir = VmMetadata::vms_dir();
    if !meta.vhdx_path.is_empty() && std::path::Path::new(&meta.vhdx_path).starts_with(&vms_dir) {
        if let Err(e) = std::fs::remove_file(&meta.vhdx_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove VHDX {}: {}", meta.vhdx_path, e);
            }
        }
    }

    let key_path = std::path::Path::new(&meta.ssh_key);
    if key_path.starts_with(&vms_dir) {
        if let Err(e) = std::fs::remove_file(key_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove SSH key: {}", e);
            }
        }
        let pub_path = std::path::PathBuf::from(format!("{}.pub", meta.ssh_key));
        if let Err(e) = std::fs::remove_file(pub_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove SSH pubkey: {}", e);
            }
        }
    }

    if !meta.switch_name.is_empty() {
        vm::remove_internal_switch(&meta.switch_name);
    }

    VmMetadata::remove(&opts.name);
    println!("VM '{}' removed successfully", opts.name);
    Ok(())
}
