//! hyperv list — List persistent VMs.

use clap::Parser;
use color_eyre::Result;

use super::vm;
use super::VmMetadata;

/// Options for `vm list`.
#[derive(Parser, Debug)]
pub struct HypervListOpts {
    /// Output in JSON format
    #[clap(long)]
    pub json: bool,

    /// Show all VMs (including stopped)
    #[clap(long, short = 'a')]
    pub all: bool,
}

pub fn run(opts: HypervListOpts) -> Result<()> {
    let all_vms = VmMetadata::list_all()?;

    let vms: Vec<(VmMetadata, String)> = all_vms
        .into_iter()
        .map(|vm_meta| {
            let state = vm::get_vm_state(&vm_meta.vm_name)
                .unwrap_or_else(|_| "unknown".to_string())
                .to_lowercase();
            (vm_meta, state)
        })
        .filter(|(_, state)| opts.all || state == "running")
        .collect();

    if opts.json {
        let enriched: Vec<serde_json::Value> = vms
            .iter()
            .map(|(vm_meta, state)| {
                serde_json::json!({
                    "name": vm_meta.name,
                    "image": vm_meta.image,
                    "state": state,
                    "ssh_port": vm_meta.ssh_port,
                    "vcpus": vm_meta.vcpus,
                    "memory_mb": vm_meta.memory_mb,
                    "created": vm_meta.created,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
        return Ok(());
    }

    if vms.is_empty() {
        if opts.all {
            println!("No VMs found");
            println!("Tip: Create VMs with 'bcvk vm run <disk.vhdx>'");
        } else {
            println!("No running VMs found");
            println!("Use --all to see stopped VMs or 'bcvk vm run <disk.vhdx>' to create one");
        }
        return Ok(());
    }

    println!("{:<20} {:<10} {:<40} SSH", "NAME", "STATE", "IMAGE");
    for (vm_meta, state) in &vms {
        println!(
            "{:<20} {:<10} {:<40} ssh -p {} -i {} root@localhost",
            vm_meta.name, state, vm_meta.image, vm_meta.ssh_port, vm_meta.ssh_key
        );
    }
    Ok(())
}
