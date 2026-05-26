//! hyperv run — Create and start a persistent VM from a bootc image.

use std::path::PathBuf;

use clap::Parser;
use color_eyre::{eyre::bail, Result};
use tracing::info;

use super::vm;
use super::VmMetadata;

/// Options for `vm run`.
#[derive(Parser, Debug)]
pub struct HypervRunOpts {
    /// Container image reference
    pub image: String,

    /// VM name (default: derived from image)
    #[clap(long, short)]
    pub name: Option<String>,

    /// Number of vCPUs
    #[clap(long, default_value = "4")]
    pub cpus: u32,

    /// Memory in MB
    #[clap(long, default_value = "4096")]
    pub memory: u32,

    /// Hyper-V virtual switch name
    #[clap(long, default_value = "Default Switch")]
    pub switch: String,
}

pub fn run(opts: HypervRunOpts) -> Result<()> {
    if !vm::is_hyper_v_enabled() {
        bail!("Hyper-V is not enabled on this system");
    }

    let name = opts.name.unwrap_or_else(|| {
        opts.image
            .rsplit('/')
            .next()
            .unwrap_or("vm")
            .replace(':', "-")
    });

    let vm_name = format!("bcvk-{}", name);

    if let Ok(state) = vm::get_vm_state(&vm_name) {
        if !state.is_empty() {
            bail!(
                "VM '{}' already exists (state: {})",
                name,
                state.to_lowercase()
            );
        }
    }

    info!("creating persistent VM: {} ({})", name, opts.image);

    let vms_dir = VmMetadata::vms_dir();
    std::fs::create_dir_all(&vms_dir)?;

    let ssh_key_path = vms_dir.join(format!("{}-key", name));
    let pub_path = PathBuf::from(format!("{}.pub", ssh_key_path.display()));
    let _ = std::fs::remove_file(&ssh_key_path);
    let _ = std::fs::remove_file(&pub_path);

    let status = std::process::Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(&ssh_key_path)
        .status()?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }

    vm::create_gen2_vm(&vm_name, opts.memory, opts.cpus, &opts.switch)?;
    info!(
        "created Hyper-V Gen2 VM: {} ({} vCPUs, {}MB)",
        vm_name, opts.cpus, opts.memory
    );

    let ssh_port = std::net::TcpListener::bind("127.0.0.1:0")
        .map(|l| l.local_addr().unwrap().port())
        .unwrap_or(2222);

    let meta = VmMetadata {
        name: name.clone(),
        image: opts.image,
        vm_name,
        ssh_port,
        ssh_key: ssh_key_path.to_string_lossy().to_string(),
        vcpus: opts.cpus,
        memory_mb: opts.memory,
        vhdx_path: String::new(),
        created: chrono::Utc::now().to_rfc3339(),
    };
    meta.save()?;

    println!(
        "VM '{}' created. Use 'bcvk vm ssh {}' to connect.",
        name, name
    );
    println!("Note: persistent VM boot with bootc image is not yet implemented.");
    println!(
        "VM metadata saved to: {}",
        VmMetadata::vms_dir()
            .join(format!("{}.json", name))
            .display()
    );

    Ok(())
}
