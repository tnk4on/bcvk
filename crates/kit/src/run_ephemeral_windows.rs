//! Ephemeral VM launch flow for Windows using Hyper-V VHDX + NBD over vsock.
//!
//! Architecture:
//! 1. bcvk-nbd --vsock listens on vsock port in podman machine (systemd-run)
//! 2. vsock relay on host connects to both VMs (Host-initiated, ~1 GB/s each)
//! 3. Hyper-V Gen2 VM boots → nbd.ko + nbd-vsock → EROFS rootfs
//! 4. Supports WSL2 (default) and Hyper-V podman machine backends

use color_eyre::{
    eyre::{bail, eyre},
    Result,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use tracing::{debug, info};

use crate::hyperv::boot_files;
use crate::hyperv::dhcp::DhcpServer;
use crate::hyperv::ssh_forward::SshForward;
use crate::hyperv::vm;
use crate::vm_helpers::{
    default_vcpus, detect_machine_name, detect_podman_vmtype, ensure_image_and_get_digest,
    is_machine_rootful, parse_memory_to_mb, run_ssh_command, run_ssh_interactive, wait_for_ssh,
};

const VM_PREFIX: &str = "bcvk-ephemeral-";

// --- Metadata ---

pub fn ephemeral_base_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk")
        .join("ephemeral")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EphemeralVmMetadata {
    pub name: String,
    pub image: String,
    pub vm_name: String,
    pub ssh_port: u16,
    pub ssh_key: String,
    pub nbd_container: Option<String>,
    pub vsock_port: Option<u32>,
    #[serde(default)]
    pub subnet: u8,
    pub created: String,
}

impl EphemeralVmMetadata {
    pub fn vms_dir() -> PathBuf {
        ephemeral_base_dir().join("vms")
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::vms_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.name));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn remove(name: &str) {
        let path = Self::vms_dir().join(format!("{}.json", name));
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove {}: {}", path.display(), e);
            }
        }
    }

    pub fn load(name: &str) -> Result<Self> {
        let path = Self::vms_dir().join(format!("{}.json", name));
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn list_all() -> Result<Vec<Self>> {
        let dir = Self::vms_dir();
        let mut vms = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry
                    .path()
                    .extension()
                    .map(|e| e == "json")
                    .unwrap_or(false)
                {
                    if let Ok(data) = std::fs::read_to_string(entry.path()) {
                        if let Ok(vm) = serde_json::from_str(&data) {
                            vms.push(vm);
                        }
                    }
                }
            }
        }
        Ok(vms)
    }
}

// --- Cleanup ---

struct VmCleanup {
    vm_name: String,
    nbd_container: Option<String>,
    name: String,
    image: String,
    vhdx_path: Option<String>,
    ssh_forward: Option<SshForward>,
    vsock_port: Option<u32>,
    switch_name: Option<String>,
}

impl Drop for VmCleanup {
    fn drop(&mut self) {
        debug!("cleaning up VM...");
        if let Some(ref fwd) = self.ssh_forward {
            fwd.stop();
        }
        if let Err(e) = vm::remove_vm(&self.vm_name) {
            tracing::debug!("failed to remove VM {}: {}", self.vm_name, e);
        }
        if let Some(ref name) = self.nbd_container {
            crate::nbd_windows::stop_nbd_server(name);
        }
        if let Some(ref vhdx) = self.vhdx_path {
            if let Err(e) = std::fs::remove_file(vhdx) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!("failed to remove VHDX {}: {}", vhdx, e);
                }
            }
        }
        if let Ok(machine) = detect_machine_name() {
            if let Err(e) = Command::new("podman")
                .args([
                    "machine",
                    "ssh",
                    &machine,
                    "--",
                    "podman",
                    "image",
                    "umount",
                    &self.image,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                tracing::debug!("failed to umount image {}: {}", self.image, e);
            }
        }
        if let Some(port) = self.vsock_port {
            if let Err(e) = vm::unregister_vsock_service(port) {
                tracing::debug!("failed to unregister vsock service port {}: {}", port, e);
            }
        }
        if let Some(ref sw) = self.switch_name {
            vm::remove_internal_switch(sw);
        }
        EphemeralVmMetadata::remove(&self.name);
    }
}

/// Spawn cleanup as a detached process so bcvk can exit immediately.
fn spawn_cleanup(c: &VmCleanup) {
    if let Err(e) = vm::stop_vm(&c.vm_name) {
        tracing::debug!("failed to stop VM {}: {}", c.vm_name, e);
    }
    if let Err(e) = vm::remove_vm(&c.vm_name) {
        tracing::debug!("failed to remove VM {}: {}", c.vm_name, e);
    }
    if let Some(ref sw) = c.switch_name {
        vm::remove_internal_switch(sw);
    }
    if let Some(ref name) = c.nbd_container {
        crate::nbd_windows::stop_nbd_server(name);
    }
    if let Some(ref vhdx) = c.vhdx_path {
        if let Err(e) = std::fs::remove_file(vhdx) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove VHDX {}: {}", vhdx, e);
            }
        }
    }
    if let Ok(machine) = detect_machine_name() {
        if let Err(e) = Command::new("podman")
            .args([
                "machine", "ssh", &machine, "--", "podman", "image", "umount", &c.image,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            tracing::debug!("failed to umount image {}: {}", c.image, e);
        }
    }
    if let Some(port) = c.vsock_port {
        if let Err(e) = vm::unregister_vsock_service(port) {
            tracing::debug!("failed to unregister vsock service port {}: {}", port, e);
        }
    }
    EphemeralVmMetadata::remove(&c.name);
}

// --- Options ---

#[derive(clap::Parser, Debug)]
pub struct RunEphemeralOpts {
    /// Container image to boot
    pub image: String,
    /// Instance type (e.g., u1.nano, u1.small, u1.medium). Overrides vcpus/memory if specified.
    #[clap(long)]
    pub itype: Option<crate::instancetypes::InstanceType>,
    /// Number of vCPUs (overridden by --itype if specified)
    #[clap(long)]
    pub vcpus: Option<u32>,
    /// Memory size (e.g. "4G", "2048M", or plain number for MB)
    #[clap(long, default_value = "4G")]
    pub memory: String,
    /// Generate a temporary SSH key pair for VM access
    #[clap(long = "ssh-keygen", short = 'K')]
    pub ssh_keygen: bool,
    /// Command(s) to execute via SSH after boot
    #[clap(long)]
    pub execute: Vec<String>,
    /// VM name for identification
    #[clap(long)]
    pub name: Option<String>,
    /// Additional kernel command line arguments
    #[clap(long = "karg")]
    pub kernel_args: Vec<String>,
    /// Display VM console in Hyper-V Manager
    #[clap(long)]
    pub gui: bool,
    /// Run in background
    #[clap(long, short = 'd')]
    pub detach: bool,
    /// Enable debug mode
    #[clap(long)]
    pub debug: bool,
}

// --- Main entry point ---

struct RunContext {
    machine: String,
    podman_ssh: boot_files::PodmanSsh,
    vm_name: String,
    name: String,
    vcpus: u32,
    memory_mb: u32,
    rootful: bool,
    base_dir: PathBuf,
    ssh_key_path: PathBuf,
    nbd_container_name: String,
    switch_name: String,
    subnet: u8,
    vsock_port: u32,
}

fn subnet_from_name(name: &str) -> u8 {
    let mut hash: u32 = 5381;
    for b in name.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    ((hash % 127) + 1) as u8
}

impl RunContext {
    fn new(opts: &RunEphemeralOpts) -> Result<Self> {
        if !vm::is_hyper_v_enabled() {
            bail!("Hyper-V is not enabled. Run: Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All");
        }

        let machine = detect_machine_name()?;
        info!("podman machine: {}", machine);

        let inspect_out = Command::new("podman")
            .args(["machine", "inspect", &machine])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()?;
        let inspect_json = String::from_utf8_lossy(&inspect_out.stdout);
        let rootful = is_machine_rootful(&machine);
        let pm_ssh_port = inspect_json
            .lines()
            .find(|l| l.contains("\"Port\""))
            .and_then(|l| {
                l.trim()
                    .trim_matches(|c: char| !c.is_ascii_digit())
                    .parse::<u16>()
                    .ok()
            })
            .unwrap_or(22);
        let pm_identity = inspect_json
            .lines()
            .find(|l| l.contains("\"IdentityPath\""))
            .map(|l| l.split('"').nth(3).unwrap_or("").to_string())
            .unwrap_or_default();
        info!(
            "Podman Machine SSH: port={}, key={}",
            pm_ssh_port, pm_identity
        );

        let podman_ssh = boot_files::PodmanSsh {
            port: pm_ssh_port,
            key: pm_identity,
            rootful,
        };

        let vm_name_suffix = opts.name.clone().unwrap_or_else(|| {
            format!(
                "{:08x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as u32
            )
        });
        let vm_name = format!("{}{}", VM_PREFIX, vm_name_suffix);
        let name = vm_name_suffix;
        let vcpus = opts
            .itype
            .as_ref()
            .map(|t| t.vcpus())
            .or(opts.vcpus)
            .unwrap_or_else(default_vcpus);
        let memory_mb = opts
            .itype
            .as_ref()
            .map(|t| t.memory_mb())
            .unwrap_or_else(|| parse_memory_to_mb(&opts.memory).unwrap_or(4096));
        let base_dir = ephemeral_base_dir();
        std::fs::create_dir_all(&base_dir)?;
        let ssh_key_path = base_dir.join(format!("{}-key", name));
        let nbd_container_name = format!("bcvk-nbd-{}", name);
        let switch_name = vm_name.clone();
        let subnet = subnet_from_name(&name);
        let vsock_port = VSOCK_PORT_BASE + subnet as u32;

        Ok(Self {
            machine,
            podman_ssh,
            vm_name,
            name,
            vcpus,
            memory_mb,
            rootful,
            base_dir,
            ssh_key_path,
            nbd_container_name,
            switch_name,
            subnet,
            vsock_port,
        })
    }
}

struct SetupResult {
    ssh_pubkey: String,
    digest_short: String,
    podman_vm_guid: String,
    merged_path: String,
    switch_vm_handle: Option<std::thread::JoinHandle<Result<(vm::SwitchInfo, ())>>>,
}

const VSOCK_PORT_BASE: u32 = 1030;

fn setup_image_and_guid(ctx: &RunContext, opts: &RunEphemeralOpts) -> Result<SetupResult> {
    let need_ssh = opts.ssh_keygen || !opts.execute.is_empty();
    let ssh_key_path = ctx.ssh_key_path.clone();
    let ssh_handle = std::thread::spawn(move || -> Result<String> {
        if need_ssh {
            let pub_path = PathBuf::from(format!("{}.pub", ssh_key_path.display()));
            for p in [&ssh_key_path, &pub_path] {
                if let Err(e) = std::fs::remove_file(p) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::debug!("failed to remove {}: {}", p.display(), e);
                    }
                }
            }
            let status = Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-f"])
                .arg(&ssh_key_path)
                .args(["-N", "", "-q"])
                .status()?;
            if !status.success() {
                bail!("ssh-keygen failed");
            }
            Ok(std::fs::read_to_string(&pub_path)?)
        } else {
            Ok(String::new())
        }
    });

    // Switch must be ready before VM creation (NIC needs the switch)
    let switch_handle = {
        let sn = ctx.switch_name.clone();
        let vn = ctx.vm_name.clone();
        let mem = ctx.memory_mb;
        let cpu = ctx.vcpus;
        let hi = format!("10.0.{}.1", ctx.subnet);
        std::thread::spawn(move || -> Result<(vm::SwitchInfo, ())> {
            let sw = vm::ensure_internal_switch(&sn, &hi, 24)?;
            let vm = vm::create_gen2_vm(&vn, mem, cpu, &sn)?;
            Ok((sw, vm))
        })
    };

    let machine_clone = ctx.machine.clone();
    let vsock_port_for_reg = ctx.vsock_port;
    let guid_handle = std::thread::spawn(move || -> Result<String> {
        vm::register_vsock_service(vsock_port_for_reg)?;
        let vmtype = detect_podman_vmtype()?;
        info!("podman machine VM type: {}", vmtype);
        let guid = match vmtype.as_str() {
            "wsl" => vm::get_wsl_vm_guid(&machine_clone)?,
            "hyperv" => vm::get_vm_guid(&machine_clone)?,
            other => bail!("unsupported podman machine VM type: {}", other),
        };
        info!("podman machine VM GUID: {}", guid);
        Ok(guid)
    });

    // Ensure image is pulled and get digest (before parallel mount)
    let digest_short = ensure_image_and_get_digest(&opts.image)?;
    info!("image digest: {}...", digest_short);

    // Mount image via SSH to podman machine
    let ps_mount = ctx.podman_ssh.clone();
    let image_mount = opts.image.clone();
    let rootful = ctx.rootful;
    let mount_handle = std::thread::spawn(move || -> Result<String> {
        let pfx = if rootful {
            "sudo podman"
        } else {
            "podman unshare podman"
        };
        let script = format!(
            "MERGED=$({pfx} image mount {image}); echo MERGED=$MERGED",
            pfx = pfx,
            image = image_mount,
        );
        let out = ps_mount.ssh_cmd(&script)?;
        let stdout = String::from_utf8_lossy(&out);
        let merged = stdout
            .lines()
            .find(|l| l.starts_with("MERGED="))
            .map(|l| l.trim_start_matches("MERGED=").trim().to_string())
            .unwrap_or_default();
        if merged.is_empty() {
            bail!("failed to get MERGED path");
        }
        Ok(merged)
    });

    let ssh_pubkey = ssh_handle
        .join()
        .map_err(|_| eyre!("ssh-keygen panicked"))??;
    let podman_vm_guid = guid_handle.join().map_err(|_| eyre!("guid panicked"))??;
    let merged_path = mount_handle.join().map_err(|_| eyre!("mount panicked"))??;
    info!("image mounted at: {}", merged_path);

    Ok(SetupResult {
        ssh_pubkey,
        digest_short,
        podman_vm_guid,
        merged_path,
        switch_vm_handle: Some(switch_handle),
    })
}

struct BootDiskResult {
    vhdx_path: String,
    nbd_handle: Option<std::thread::JoinHandle<Result<String>>>,
}

fn create_boot_disk(ctx: &RunContext, p0: &SetupResult) -> Result<BootDiskResult> {
    let nbd_handle = {
        let ps = ctx.podman_ssh.clone();
        let name = ctx.name.clone();
        let merged = p0.merged_path.clone();
        let ssh_pubkey = p0.ssh_pubkey.clone();
        let vsock_port = ctx.vsock_port;
        std::thread::spawn(move || -> Result<String> {
            crate::nbd_windows::deploy_nbd_server(&ps)?;
            let cmdline = "root=PARTLABEL=bcvk-root ro rootfstype=erofs";
            crate::nbd_windows::start_nbd_server(
                &ps,
                &merged,
                cmdline,
                &ssh_pubkey,
                vsock_port,
                &name,
            )
        })
    };

    let vhdx_vm = ctx.base_dir.join(format!("esp-{}.vhdx", ctx.name));
    let vhdx_path = boot_files::create_boot_vhdx(
        &p0.digest_short,
        &p0.merged_path,
        &ctx.podman_ssh,
        &p0.ssh_pubkey,
        ctx.vsock_port,
        &vhdx_vm,
    )?;

    Ok(BootDiskResult {
        vhdx_path,
        nbd_handle: Some(nbd_handle),
    })
}

fn start_vm_and_services(
    ctx: &RunContext,
    p0: &SetupResult,
    p1: &BootDiskResult,
    opts: &RunEphemeralOpts,
    t_start: &std::time::Instant,
) -> Result<()> {
    macro_rules! elapsed {
        ($label:expr) => {
            info!(
                "[timing] {}: {:.1}s",
                $label,
                t_start.elapsed().as_secs_f64()
            );
        };
    }

    let ephemeral_vm_guid = vm::attach_and_start_vm(&ctx.vm_name, &p1.vhdx_path)?;
    elapsed!("VHDX attach + VM start");

    // Serial pipe reader
    let serial_log_path = ctx.base_dir.join(format!("serial-{}.log", ctx.name));
    let serial_pipe_path = format!("\\\\.\\pipe\\bcvk-serial-{}", ctx.vm_name);
    let serial_log_clone = serial_log_path.clone();
    let serial_debug = opts.debug;
    let _serial_thread = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        let pipe = {
            let mut file = None;
            for _ in 0..10 {
                match std::fs::File::open(&serial_pipe_path) {
                    Ok(f) => {
                        file = Some(f);
                        break;
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
                }
            }
            match file {
                Some(f) => f,
                None => {
                    tracing::warn!("serial pipe not available: {}", serial_pipe_path);
                    return;
                }
            }
        };
        let mut log = match std::fs::File::create(&serial_log_clone) {
            Ok(f) => f,
            Err(_) => return,
        };
        let reader = BufReader::new(pipe);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if let Err(e) = writeln!(log, "{}", l) {
                        tracing::debug!("failed to write serial log: {}", e);
                    }
                    if let Err(e) = log.flush() {
                        tracing::debug!("failed to flush serial log: {}", e);
                    }
                    if serial_debug {
                        eprintln!("[serial] {}", l);
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut cleanup = VmCleanup {
        vm_name: ctx.vm_name.clone(),
        nbd_container: Some(ctx.nbd_container_name.clone()),
        name: ctx.name.clone(),
        image: opts.image.clone(),
        vhdx_path: Some(p1.vhdx_path.clone()),
        ssh_forward: None,
        vsock_port: Some(ctx.vsock_port),
        switch_name: Some(ctx.switch_name.clone()),
    };

    info!("ephemeral VM GUID: {}", ephemeral_vm_guid);

    let rt = tokio::runtime::Runtime::new()?;
    let vm_name = ctx.vm_name.clone();
    let name = ctx.name.clone();
    let ssh_key_path = ctx.ssh_key_path.clone();
    let podman_vm_guid = p0.podman_vm_guid.clone();
    let nbd_container = ctx.nbd_container_name.clone();
    let image = opts.image.clone();
    let gui = opts.gui;
    let execute = opts.execute.clone();
    let ssh_pubkey = p0.ssh_pubkey.clone();
    let subnet = ctx.subnet;

    rt.block_on(async move {
        let server_ip = format!("10.0.{}.1", subnet);
        let client_ip = format!("10.0.{}.100", subnet);
        let dhcp = DhcpServer::new(&server_ip, &client_ip, &vm_name)?;
        let dhcp_handle = dhcp.start_background();
        info!("VM {} started, VHDX booting...", vm_name);
        elapsed!("VM started");

        let _vsock_relay = crate::hyperv::vsock_relay::VsockRelay::start(
            ctx.vsock_port,
            1,
            &podman_vm_guid,
            &ephemeral_vm_guid,
        )
        .await?;
        info!("vsock relay connected (port {})", ctx.vsock_port);
        elapsed!("relay connected");

        if gui {
            if let Err(e) = Command::new("vmconnect.exe")
                .args(["localhost", &vm_name])
                .spawn()
            {
                tracing::debug!("failed to launch vmconnect: {}", e);
            }
        }

        info!("serial log: {}", serial_log_path.display());

        let vm_ip = client_ip;
        info!("VM IP: {}", vm_ip);

        let ssh_fwd = SshForward::start(&vm_ip).await?;
        let ssh_port = ssh_fwd.port();
        info!("SSH forward: localhost:{} → {}:22", ssh_port, vm_ip);
        cleanup.ssh_forward = Some(ssh_fwd);

        let metadata = EphemeralVmMetadata {
            name: name.clone(),
            image: image.clone(),
            vm_name: vm_name.clone(),
            ssh_port,
            ssh_key: ssh_key_path.to_string_lossy().to_string(),
            nbd_container: Some(nbd_container.clone()),
            vsock_port: Some(ctx.vsock_port),
            subnet,
            created: chrono::Utc::now().to_rfc3339(),
        };
        metadata.save()?;

        wait_for_ssh(ssh_port, &ssh_key_path, "root")?;
        info!("SSH connected!");
        elapsed!("SSH connected (total)");

        if !execute.is_empty() {
            for cmd in &execute {
                run_ssh_command(ssh_port, &ssh_key_path, "root", cmd).map(|_| ())?;
            }
        } else if ssh_pubkey.is_empty() {
            info!("VM running. Use: bcvk ephemeral ssh {}", name);
        } else {
            let status = run_ssh_interactive(ssh_port, &ssh_key_path, "root")?;
            let exit_code = status.code().unwrap_or(1);
            dhcp.stop();
            dhcp_handle.abort();
            spawn_cleanup(&cleanup);
            std::mem::forget(cleanup);
            std::process::exit(exit_code);
        }

        dhcp.stop();
        dhcp_handle.abort();
        Ok::<(), color_eyre::Report>(())
    })?;

    Ok(())
}

pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    if opts.gui && opts.detach {
        bail!("--gui and --detach cannot be used together");
    }

    if opts.detach {
        return run_detached(&opts);
    }

    let t_start = std::time::Instant::now();
    macro_rules! elapsed {
        ($label:expr) => {
            info!(
                "[timing] {}: {:.1}s",
                $label,
                t_start.elapsed().as_secs_f64()
            );
        };
    }

    let ctx = RunContext::new(&opts)?;
    let setup = setup_image_and_guid(&ctx, &opts)?;
    elapsed!("image mount + setup");

    let mut setup = setup;
    let boot_disk = create_boot_disk(&ctx, &setup)?;
    elapsed!("VHDX created");

    let (switch, _) = setup
        .switch_vm_handle
        .take()
        .unwrap()
        .join()
        .map_err(|_| eyre!("switch+VM panicked"))??;
    info!("Internal Switch: {} ({})", switch.name, switch.host_ip);
    elapsed!("switch ready");
    elapsed!("VM created");

    let mut boot_disk = boot_disk;
    if let Some(handle) = boot_disk.nbd_handle.take() {
        match handle
            .join()
            .map_err(|_| eyre!("nbd server thread panicked"))?
        {
            Ok(unit) => info!("nbd server ready: {}", unit),
            Err(e) => info!("nbd server warning: {}", e),
        }
    }
    elapsed!("nbd server ready");

    // VM start + serial + relay + DHCP + SSH
    start_vm_and_services(&ctx, &setup, &boot_disk, &opts, &t_start)
}

// --- Detached mode ---

fn run_detached(opts: &RunEphemeralOpts) -> Result<()> {
    let base = ephemeral_base_dir();
    std::fs::create_dir_all(&base)?;

    let vm_name = opts.name.clone().unwrap_or_else(|| {
        format!(
            "{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32
        )
    });
    let log_path = base.join(format!("bcvk-{}.log", vm_name));
    let log_file = std::fs::File::create(&log_path)?;

    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach" && a != "-d")
        .collect();
    if !args.contains(&"-K".to_string()) && !args.contains(&"--ssh-keygen".to_string()) {
        args.push("-K".to_string());
    }
    if opts.name.is_none() {
        args.push("--name".to_string());
        args.push(vm_name.clone());
    }
    // Keep child process alive to prevent VM cleanup
    if opts.execute.is_empty() {
        args.push("--execute".to_string());
        args.push("sleep infinity".to_string());
    }

    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Temporarily disable handle inheritance on parent stdout/stderr
    #[allow(unsafe_code)]
    unsafe {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{
            SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
        };
        let stdout_h = HANDLE(std::io::stdout().as_raw_handle() as *mut _);
        let stderr_h = HANDLE(std::io::stderr().as_raw_handle() as *mut _);
        let _ = SetHandleInformation(stdout_h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        let _ = SetHandleInformation(stderr_h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
    }

    let _child = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()?;

    // Restore handle inheritance
    #[allow(unsafe_code)]
    unsafe {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
        let stdout_h = HANDLE(std::io::stdout().as_raw_handle() as *mut _);
        let stderr_h = HANDLE(std::io::stderr().as_raw_handle() as *mut _);
        let _ = SetHandleInformation(stdout_h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
        let _ = SetHandleInformation(stderr_h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT);
    }

    // Save metadata so bcvk ephemeral stop/ssh can find this VM
    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        vm_name: format!("{}{}", VM_PREFIX, vm_name),
        ssh_port: 0,
        ssh_key: base
            .join(format!("{}-key", vm_name))
            .to_string_lossy()
            .to_string(),
        nbd_container: None,
        vsock_port: Some(1030),
        subnet: 0,
        created: chrono::Utc::now().to_rfc3339(),
    };
    metadata.save()?;

    info!("started in background: {}", vm_name);
    info!("log: {}", log_path.display());
    println!("{}", vm_name);
    Ok(())
}

// --- Shared helpers (ported from run_ephemeral_macos.rs, no Unix deps) ---

// Shared helpers moved to vm_helpers.rs:
// detect_machine_name, detect_podman_vmtype, is_machine_rootful,
// ensure_image_and_get_digest, wait_for_ssh, run_ssh_command, run_ssh_interactive

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ephemeral_vm_metadata_roundtrip() {
        let meta = EphemeralVmMetadata {
            name: "test-vm".to_string(),
            image: "quay.io/test:latest".to_string(),
            vm_name: "bcvk-ephemeral-test".to_string(),
            ssh_port: 2222,
            ssh_key: "/tmp/test-key".to_string(),
            nbd_container: Some("bcvk-nbd-test".to_string()),
            vsock_port: Some(1030),
            subnet: 1,
            created: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: EphemeralVmMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-vm");
        assert_eq!(parsed.ssh_port, 2222);
        assert_eq!(parsed.nbd_container, Some("bcvk-nbd-test".to_string()));
    }
}
