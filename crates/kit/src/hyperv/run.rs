//! hyperv run — Start a persistent VM from a disk image (VHDX).

use std::path::Path;

use clap::Parser;
use color_eyre::{eyre::bail, Result};
use tracing::info;

use super::vm;
use super::VmMetadata;

/// Options for `vm run`.
#[derive(Parser, Debug)]
pub struct HypervRunOpts {
    /// Disk image path (.vhdx)
    pub disk: String,

    /// VM name (default: derived from disk filename)
    #[clap(long, short)]
    pub name: Option<String>,

    /// Number of vCPUs
    #[clap(long, default_value = "4")]
    pub cpus: u32,

    /// Memory in MB
    #[clap(long, default_value = "4096")]
    pub memory: u32,

    /// Path to an existing SSH private key
    #[clap(long)]
    pub ssh_key: Option<String>,

    /// SSH port (default: auto-allocate)
    #[clap(long)]
    pub ssh_port: Option<u16>,
}

pub fn run(opts: HypervRunOpts) -> Result<()> {
    if !vm::is_hyper_v_enabled() {
        bail!("Hyper-V is not enabled on this system");
    }

    let disk_path = Path::new(&opts.disk);
    if !disk_path.exists() {
        bail!("disk image not found: {}", opts.disk);
    }

    let name = opts.name.unwrap_or_else(|| {
        disk_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("vm")
            .to_string()
    });

    let vm_name = format!("bcvk-{}", name);

    if let Ok(state) = vm::get_vm_state(&vm_name) {
        if !state.is_empty() {
            bail!(
                "VM '{}' already exists (state: {}). Remove it first with 'bcvk vm rm {}'",
                name,
                state.to_lowercase(),
                name
            );
        }
    }

    let vms_dir = VmMetadata::vms_dir();
    std::fs::create_dir_all(&vms_dir)?;

    let ssh_key = match &opts.ssh_key {
        Some(p) => {
            if !Path::new(p).exists() {
                bail!("SSH key not found: {}", p);
            }
            p.clone()
        }
        None => {
            let key_path = vms_dir.join(format!("{}-key", name));
            if !key_path.exists() {
                info!("generating SSH keypair...");
                let pub_path = std::path::PathBuf::from(format!("{}.pub", key_path.display()));
                let _ = std::fs::remove_file(&key_path);
                let _ = std::fs::remove_file(&pub_path);
                let status = std::process::Command::new("ssh-keygen")
                    .args(["-t", "ed25519", "-N", "", "-q", "-f"])
                    .arg(&key_path)
                    .status()?;
                if !status.success() {
                    bail!("ssh-keygen failed");
                }
            }
            key_path.to_string_lossy().to_string()
        }
    };

    info!("creating persistent VM: {} (disk: {})", name, opts.disk);

    // Per-VM internal switch with unique subnet (hash of name)
    let switch_name = vm_name.clone();
    let subnet = {
        let mut hash: u32 = 5381;
        for b in name.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(b as u32);
        }
        ((hash % 127) + 128) as u8
    };
    let host_ip = format!("10.0.{}.1", subnet);
    vm::ensure_internal_switch(&switch_name, &host_ip, 24)?;
    info!("Internal Switch: {} ({})", switch_name, host_ip);

    vm::create_gen2_vm(&vm_name, opts.memory, opts.cpus, &switch_name)?;

    let vhdx_abs = std::fs::canonicalize(disk_path)?
        .to_string_lossy()
        .to_string()
        .trim_start_matches(r"\\?\")
        .to_string();
    vm::attach_vhdx(&vm_name, &vhdx_abs)?;

    let ssh_port = opts.ssh_port.unwrap_or_else(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .map(|l| l.local_addr().unwrap().port())
            .unwrap_or(2222)
    });

    let meta = VmMetadata {
        name: name.clone(),
        image: String::new(),
        vm_name: vm_name.clone(),
        ssh_port,
        ssh_key,
        vcpus: opts.cpus,
        memory_mb: opts.memory,
        vhdx_path: vhdx_abs.clone(),
        switch_name: switch_name.clone(),
        subnet,
        created: chrono::Utc::now().to_rfc3339(),
    };
    meta.save()?;

    vm::start_vm(&vm_name)?;
    info!(
        "started VM: {} ({} vCPUs, {}MB)",
        vm_name, opts.cpus, opts.memory
    );

    println!("VM '{}' started from {}", name, opts.disk);
    println!("Use 'bcvk vm ssh {}' to connect.", name);
    println!("Use 'bcvk vm stop {}' to stop.", name);

    Ok(())
}
