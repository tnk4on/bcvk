//! hyperv rm-all — Remove all persistent VMs and their metadata.

use clap::Parser;
use color_eyre::Result;

use super::vm;
use super::VmMetadata;

/// Options for `vm rm-all`.
#[derive(Parser, Debug)]
pub struct HypervRmAllOpts {
    /// Force removal without confirmation
    #[clap(long, short = 'f')]
    pub force: bool,

    /// Remove VMs even if they are running
    #[clap(long)]
    pub stop: bool,

    /// Filter VMs by label (only remove VMs with this label)
    #[clap(long)]
    pub label: Option<String>,
}

pub fn run(opts: HypervRmAllOpts) -> Result<()> {
    let mut vms = VmMetadata::list_all()?;

    // Filter by label if specified
    if let Some(ref filter_label) = opts.label {
        vms.retain(|v| v.labels.contains(filter_label));
    }

    if vms.is_empty() {
        if let Some(ref label) = opts.label {
            println!("No VMs found with label '{}'", label);
        } else {
            println!("No VMs found");
        }
        return Ok(());
    }

    // Confirmation prompt
    if !opts.force {
        println!(
            "This will permanently delete {} VM{} and their data:",
            vms.len(),
            if vms.len() == 1 { "" } else { "s" }
        );
        for vm_meta in &vms {
            let state = vm::get_vm_state(&vm_meta.vm_name)
                .unwrap_or_else(|_| "unknown".to_string())
                .to_lowercase();
            println!("  - {} ({})", vm_meta.name, state);
            if !vm_meta.image.is_empty() {
                println!("    Image: {}", vm_meta.image);
            }
            println!("    Disk: {}", vm_meta.vhdx_path);
        }
        println!();
        println!("Are you sure? This cannot be undone. Use --force to skip this prompt.");
        return Ok(());
    }

    let mut removed_count = 0;
    let mut error_count = 0;

    for vm_meta in &vms {
        println!("Removing VM '{}'...", vm_meta.name);

        // Stop if running
        let state = vm::get_vm_state(&vm_meta.vm_name).unwrap_or_default();
        if state.contains("Running") {
            if opts.stop || opts.force {
                println!("  Stopping running VM...");
                super::kill_vm_service(vm_meta);
                if let Err(e) = vm::turn_off_vm(&vm_meta.vm_name) {
                    eprintln!("  Failed to stop VM '{}': {}", vm_meta.name, e);
                    error_count += 1;
                    continue;
                }
            } else {
                eprintln!(
                    "  Skipping '{}': VM is running. Use --stop to force removal.",
                    vm_meta.name
                );
                error_count += 1;
                continue;
            }
        } else {
            super::kill_vm_service(vm_meta);
        }

        // Delegate to rm::run for actual cleanup
        if let Err(e) = super::rm::run(super::rm::HypervRmOpts {
            name: vm_meta.name.clone(),
            force: true,
            stop: false,
        }) {
            eprintln!("  Failed to remove VM '{}': {}", vm_meta.name, e);
            error_count += 1;
            continue;
        }

        removed_count += 1;
    }

    println!();
    println!(
        "Summary: {} VM{} removed, {} error{}",
        removed_count,
        if removed_count == 1 { "" } else { "s" },
        error_count,
        if error_count == 1 { "" } else { "s" }
    );

    if error_count > 0 {
        Err(color_eyre::eyre::eyre!(
            "Failed to remove {} VM{}",
            error_count,
            if error_count == 1 { "" } else { "s" }
        ))
    } else {
        Ok(())
    }
}
