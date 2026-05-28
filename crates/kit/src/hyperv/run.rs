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

    /// Internal: run as service process (do not use directly)
    #[clap(long = "_internal", hide = true)]
    pub _internal: Option<String>,
}

pub fn run(opts: HypervRunOpts) -> Result<()> {
    // Internal service mode: run DHCP + SSH forward for a running VM
    if let Some(ref name) = opts._internal {
        return super::run_vm_service(name);
    }

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

    let ssh_key = find_ssh_key(&opts.ssh_key, disk_path)?;

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
    {
        let sn = switch_name.clone();
        let hi = host_ip.clone();
        let vn = vm_name.clone();
        let mem = opts.memory;
        let cpu = opts.cpus;
        std::thread::spawn(move || -> Result<()> {
            vm::ensure_internal_switch(&sn, &hi, 24)?;
            vm::create_gen2_vm(&vn, mem, cpu, &sn)?;
            Ok(())
        })
        .join()
        .map_err(|_| color_eyre::eyre::eyre!("VM setup thread panicked"))??;
    }
    info!("Internal Switch: {} ({})", switch_name, host_ip);

    let vhdx_abs = std::fs::canonicalize(disk_path)?
        .to_string_lossy()
        .to_string()
        .trim_start_matches(r"\\?\")
        .to_string();
    vm::attach_vhdx(&vm_name, &vhdx_abs)?;
    vm::set_boot_order_disk_first(&vm_name);

    let ssh_port = opts.ssh_port.unwrap_or_else(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .map(|l| l.local_addr().unwrap().port())
            .unwrap_or(2222)
    });

    let mut meta = VmMetadata {
        name: name.clone(),
        image: String::new(),
        vm_name: vm_name.clone(),
        ssh_port,
        ssh_key: ssh_key.clone(),
        vcpus: opts.cpus,
        memory_mb: opts.memory,
        vhdx_path: vhdx_abs.clone(),
        switch_name: switch_name.clone(),
        subnet,
        service_pid: 0,
        state: String::new(),
        created: chrono::Utc::now().to_rfc3339(),
    };
    meta.save()?;

    vm::start_vm(&vm_name)?;
    info!(
        "started VM: {} ({} vCPUs, {}MB)",
        vm_name, opts.cpus, opts.memory
    );

    super::spawn_vm_service(&name, &mut meta)?;

    println!("VM '{}' started from {}", name, opts.disk);
    println!("SSH: ssh -p {} -i {} root@localhost", ssh_port, ssh_key);
    println!("Use 'bcvk vm ssh {}' to connect.", name);
    println!("Use 'bcvk vm stop {}' to stop.", name);

    Ok(())
}

fn find_ssh_key(explicit: &Option<String>, disk_path: &Path) -> Result<String> {
    // 1. Explicit --ssh-key
    if let Some(p) = explicit {
        if !Path::new(p).exists() {
            bail!("SSH key not found: {}", p);
        }
        return Ok(p.clone());
    }

    // 2. Auto-generated key from to-disk: <disk>.key
    let auto_key = disk_path.with_extension("vhdx.key");
    if auto_key.exists() {
        info!("using auto-generated SSH key: {}", auto_key.display());
        return Ok(auto_key.to_string_lossy().to_string());
    }

    // 3. User's default SSH keys
    if let Some(home) = dirs::home_dir() {
        for name in &["id_ed25519", "id_rsa"] {
            let key = home.join(".ssh").join(name);
            if key.exists() {
                info!("using SSH key: {}", key.display());
                return Ok(key.to_string_lossy().to_string());
            }
        }
    }

    bail!(
        "No SSH key found. Use --ssh-key to specify, or create a disk with:\n  \
         bcvk to-disk --root-ssh-authorized-keys ~/.ssh/id_ed25519.pub <image> <disk>"
    );
}
