//! hyperv list — List all persistent VMs.

use color_eyre::Result;

use super::vm;
use super::VmMetadata;

pub fn run(json: bool) -> Result<()> {
    let vms = VmMetadata::list_all()?;

    if json {
        let enriched: Vec<serde_json::Value> = vms
            .iter()
            .map(|vm| {
                let state = vm::get_vm_state(&vm.vm_name)
                    .unwrap_or_else(|_| "unknown".to_string())
                    .to_lowercase();
                serde_json::json!({
                    "name": vm.name,
                    "image": vm.image,
                    "state": state,
                    "ssh_port": vm.ssh_port,
                    "vcpus": vm.vcpus,
                    "memory_mb": vm.memory_mb,
                    "created": vm.created,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
        return Ok(());
    }

    if vms.is_empty() {
        println!("No persistent VMs found.");
        return Ok(());
    }

    println!("{:<20} {:<10} {:<40} SSH", "NAME", "STATE", "IMAGE");
    for vm in &vms {
        let state = vm::get_vm_state(&vm.vm_name)
            .unwrap_or_else(|_| "unknown".to_string())
            .to_lowercase();
        println!(
            "{:<20} {:<10} {:<40} ssh -p {} -i {} root@localhost",
            vm.name, state, vm.image, vm.ssh_port, vm.ssh_key
        );
    }
    Ok(())
}
