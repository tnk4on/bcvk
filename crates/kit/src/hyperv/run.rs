//! hyperv run — Create and run a persistent VM.
//!
//! Accepts either a container image (runs to-disk internally) or
//! an existing VHDX disk image.

use std::path::Path;
use std::str::FromStr;

use clap::Parser;
use color_eyre::{eyre::bail, Result};
use tracing::info;

use super::vm;
use super::VmMetadata;

/// Port mapping from host to VM (format: host_port:guest_port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    pub host_port: u16,
    pub guest_port: u16,
}

impl FromStr for PortMapping {
    type Err = color_eyre::Report;

    fn from_str(s: &str) -> Result<Self> {
        let (host_part, guest_part) = s.split_once(':').ok_or_else(|| {
            color_eyre::eyre::eyre!(
                "Invalid port format '{}'. Expected format: host_port:guest_port",
                s
            )
        })?;

        let host_port = host_part.trim().parse::<u16>().map_err(|_| {
            color_eyre::eyre::eyre!(
                "Invalid host port '{}'. Must be a number between 1 and 65535",
                host_part
            )
        })?;

        let guest_port = guest_part.trim().parse::<u16>().map_err(|_| {
            color_eyre::eyre::eyre!(
                "Invalid guest port '{}'. Must be a number between 1 and 65535",
                guest_part
            )
        })?;

        Ok(PortMapping {
            host_port,
            guest_port,
        })
    }
}

/// Options for `vm run`.
#[derive(Parser, Debug)]
pub struct HypervRunOpts {
    /// Container image or disk image path (.vhdx)
    #[clap(default_value = "")]
    pub image_or_disk: String,

    /// VM name (default: derived from image or disk filename)
    #[clap(long, short)]
    pub name: Option<String>,

    /// Replace existing VM with same name (stop and remove if exists)
    #[clap(long, short = 'R')]
    pub replace: bool,

    /// Instance type (e.g., u1.nano, u1.small, u1.medium). Overrides cpus/memory if specified.
    #[clap(long)]
    pub itype: Option<crate::instancetypes::InstanceType>,

    /// Number of vCPUs (overridden by --itype if specified)
    #[clap(long, default_value = "2")]
    pub cpus: u32,

    /// Memory in MB (overridden by --itype if specified)
    #[clap(long, default_value = "4096")]
    pub memory: u32,

    /// Disk size for to-disk (e.g. "20G", "10240M")
    #[clap(long, default_value = "20G")]
    pub disk_size: String,

    /// Installation options (filesystem, root-size, etc.)
    #[clap(flatten)]
    pub install: crate::install_options::InstallOptions,

    /// Path to an existing SSH private key
    #[clap(long)]
    pub ssh_key: Option<String>,

    /// SSH port (default: auto-allocate)
    #[clap(long)]
    pub ssh_port: Option<u16>,

    /// Automatically SSH into the VM after creation
    #[clap(long)]
    pub ssh: bool,

    /// Wait for SSH to become available and verify connectivity (for testing)
    #[clap(long, conflicts_with = "ssh")]
    pub ssh_wait: bool,

    /// Keep the VM running in background after creation
    #[clap(long, short = 'd')]
    pub detach: bool,

    /// Display VM console in Hyper-V Manager
    #[clap(long)]
    pub gui: bool,

    /// Disable TPM 2.0 support (enabled by default)
    #[clap(long)]
    pub disable_tpm: bool,

    /// Port mapping from host to VM (format: host_port:guest_port, e.g., 8080:80)
    #[clap(long = "port", short = 'p', action = clap::ArgAction::Append)]
    pub port_mappings: Vec<PortMapping>,

    /// User-defined labels for organizing VMs (comma not allowed in labels)
    #[clap(long)]
    pub label: Vec<String>,

    /// Internal: run as service process (do not use directly)
    #[clap(long = "_internal", hide = true)]
    pub _internal: Option<String>,

    /// Use wslc-native mode (no podman machine required)
    #[clap(long)]
    pub native: bool,
}

fn is_disk_path(input: &str) -> bool {
    input.ends_with(".vhdx") || input.ends_with(".vhd") || Path::new(input).exists()
}

fn validate_labels(labels: &[String]) -> Result<()> {
    for label in labels {
        if label.contains(',') {
            bail!("Label '{}' contains comma which is not allowed", label);
        }
    }
    Ok(())
}

pub fn run(opts: HypervRunOpts) -> Result<()> {
    // Internal service mode: run DHCP + SSH forward for a running VM
    if let Some(ref name) = opts._internal {
        return super::run_vm_service(name);
    }

    if !vm::is_hyper_v_enabled() {
        bail!("Hyper-V is not enabled on this system");
    }

    validate_labels(&opts.label)?;

    // Determine if input is a disk path or container image
    let (disk_path_str, image_name) = if is_disk_path(&opts.image_or_disk) {
        let p = Path::new(&opts.image_or_disk);
        if !p.exists() {
            bail!("disk image not found: {}", opts.image_or_disk);
        }
        (opts.image_or_disk.clone(), String::new())
    } else {
        // Container image: run to-disk to create VHDX
        let image = &opts.image_or_disk;
        let name = opts
            .name
            .clone()
            .unwrap_or_else(|| crate::vm_helpers::sanitize_vm_name(image));

        // Remove existing VM before to-disk (VHDX may be in use)
        let vm_name = format!("bcvk-{}", name);
        if opts.replace {
            if let Ok(state) = vm::get_vm_state(&vm_name) {
                if !state.is_empty() {
                    println!("Replacing existing VM '{}'...", name);
                    if state.contains("Running") {
                        super::stop(&name, false)?;
                    }
                    super::rm::run(super::rm::HypervRmOpts {
                        name: name.clone(),
                        force: true,
                        stop: false,
                    })?;
                }
            }
        }

        let vms_dir = VmMetadata::vms_dir();
        std::fs::create_dir_all(&vms_dir)?;
        let vhdx_path = vms_dir.join(format!("{}.vhdx", name));

        if !vhdx_path.exists() {
            if opts.native {
                // Native mode: use wslc COM Export → rootfs VHDX
                let session = crate::wslc_com::open_default_session()?;
                session.pull_image(image)?;
                let digest_short = session.inspect_image_digest(image)?;
                let cache_dir = crate::to_disk_windows::base_dir();
                let base_vhdx = super::rootfs_native::create_rootfs_vhdx(
                    &session, image, &digest_short, &cache_dir,
                )?;
                crate::to_disk_windows::create_differencing_vhdx(
                    &base_vhdx.to_string_lossy(),
                    &vhdx_path.to_string_lossy(),
                )?;
                // Generate SSH key for this VM
                let vm_key_path = std::path::PathBuf::from(format!("{}.key", vhdx_path.display()));
                let _pubkey = crate::vm_helpers::generate_ssh_keypair(&vm_key_path)?;
                println!("VM '{}' disk created (native) from: {}", name, base_vhdx.display());
            } else {
                let digest = crate::vm_helpers::ensure_image_and_get_digest(image)?;
                let base_disk = crate::to_disk_windows::find_or_create_base_disk(
                    image,
                    &digest,
                    &opts.install,
                    &opts.disk_size,
                    &None,
                    &[],
                )?;
                crate::to_disk_windows::create_differencing_vhdx(
                    &base_disk,
                    &vhdx_path.to_string_lossy(),
                )?;
                // Copy SSH key from base
                let base_key = format!("{}.key", base_disk);
                let vm_key = format!("{}.key", vhdx_path.to_string_lossy());
                if std::path::Path::new(&base_key).exists() {
                    std::fs::copy(&base_key, &vm_key)?;
                    let base_pub = format!("{}.key.pub", base_disk);
                    let vm_pub = format!("{}.key.pub", vhdx_path.to_string_lossy());
                    if std::path::Path::new(&base_pub).exists() {
                        std::fs::copy(&base_pub, &vm_pub)?;
                    }
                }
                println!("VM '{}' disk created from base: {}", name, base_disk);
            }
        } else {
            println!("Using cached disk image: {}", vhdx_path.display());
        }

        (vhdx_path.to_string_lossy().to_string(), image.clone())
    };

    let disk_path = Path::new(&disk_path_str);

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
            if opts.replace {
                // Replace mode: remove VM definition only (keep disk)
                println!("Replacing existing VM '{}'...", name);
                if let Ok(meta) = super::VmMetadata::load(&name) {
                    super::kill_vm_service(&meta);
                }
                if state.contains("Running") {
                    vm::turn_off_vm(&vm_name)?;
                }
                vm::remove_vm(&vm_name)?;
                vm::remove_internal_switch(&vm_name);
                super::VmMetadata::remove(&name);
            } else {
                bail!("VM '{}' already exists. Use --replace to replace it.", name);
            }
        }
    }

    let vms_dir = VmMetadata::vms_dir();
    std::fs::create_dir_all(&vms_dir)?;

    let ssh_key = find_ssh_key(&opts.ssh_key, disk_path)?;

    // Resolve cpus/memory from instance type
    let cpus = opts.itype.as_ref().map(|t| t.vcpus()).unwrap_or(opts.cpus);
    let memory = opts
        .itype
        .as_ref()
        .map(|t| t.memory_mb())
        .unwrap_or(opts.memory);

    info!("creating persistent VM: {} (disk: {})", name, disk_path_str);

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
        std::thread::spawn(move || -> Result<()> {
            vm::ensure_internal_switch(&sn, &hi, 24)?;
            vm::create_gen2_vm(&vn, memory, cpus, &sn)?;
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
    vm::attach_vhdx_at_slot(&vm_name, &vhdx_abs, 0)?;
    vm::set_boot_order_disk_first(&vm_name);

    let ssh_port = opts.ssh_port.unwrap_or_else(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .map(|l| l.local_addr().unwrap().port())
            .unwrap_or(2222)
    });

    let mut meta = VmMetadata {
        name: name.clone(),
        image: image_name,
        vm_name: vm_name.clone(),
        ssh_port,
        ssh_key: ssh_key.clone(),
        vcpus: cpus,
        memory_mb: memory,
        vhdx_path: vhdx_abs.clone(),
        switch_name: switch_name.clone(),
        subnet,
        service_pid: 0,
        gui: opts.gui,
        state: String::new(),
        labels: opts.label.clone(),
        port_mappings: opts
            .port_mappings
            .iter()
            .map(|p| (p.host_port, p.guest_port))
            .collect(),
        created: chrono::Utc::now().to_rfc3339(),
    };
    meta.save()?;

    vm::start_vm(&vm_name)?;
    info!("started VM: {} ({} vCPUs, {}MB)", vm_name, cpus, memory);

    super::spawn_vm_service(&name, &mut meta)?;

    println!("VM '{}' created successfully!", name);
    println!("  Disk: {}", vhdx_abs);
    if let Some(ref itype) = opts.itype {
        println!("  Instance Type: {}", itype);
    }
    println!("  Memory: {} MiB", memory);
    println!("  CPUs: {}", cpus);
    println!("SSH: ssh -p {} -i {} root@localhost", ssh_port, ssh_key);

    if opts.gui {
        if let Err(e) = std::process::Command::new("vmconnect.exe")
            .args(["localhost", &vm_name])
            .spawn()
        {
            tracing::debug!("failed to launch vmconnect: {}", e);
        }
    }

    if opts.ssh_wait {
        let key_path = std::path::Path::new(&ssh_key);
        crate::vm_helpers::wait_for_ssh(ssh_port, key_path, "root")?;
        println!("Ready; use bcvk vm ssh to connect");
        return Ok(());
    }

    if opts.ssh {
        let key_path = std::path::Path::new(&ssh_key);
        crate::vm_helpers::wait_for_ssh(ssh_port, key_path, "root")?;
        let status = crate::vm_helpers::run_ssh_interactive(ssh_port, key_path, "root")?;
        std::process::exit(status.code().unwrap_or(1));
    }

    println!("\nUse 'bcvk vm ssh {}' to connect", name);
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
