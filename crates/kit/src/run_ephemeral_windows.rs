//! Ephemeral VM launch flow for Windows using Hyper-V VHDX + NBD over vsock.
//!
//! Architecture:
//! 1. nbdkit --vsock listens directly on vsock port 1030 in podman machine
//! 2. vsock relay on host connects to both VMs (Host-initiated, ~1 GB/s each)
//! 3. Hyper-V Gen2 VM boots → standard nbd.ko + nbd-vsock (socketpair relay) → EROFS rootfs
//! 4. Supports WSL2 (default) and Hyper-V podman machine backends

#[cfg(target_os = "windows")]
use color_eyre::{
    eyre::{bail, eyre},
    Result,
};
#[cfg(target_os = "windows")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "windows")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};
#[cfg(target_os = "windows")]
use std::time::Duration;
#[cfg(target_os = "windows")]
use tracing::{debug, info};

#[cfg(target_os = "windows")]
use crate::boot_files;
#[cfg(target_os = "windows")]
use crate::dhcp_server::DhcpServer;
#[cfg(target_os = "windows")]
use crate::hyperv;
#[cfg(target_os = "windows")]
use crate::ssh_forward::SshForward;

#[cfg(target_os = "windows")]
const SSH_TIMEOUT: Duration = Duration::from_secs(240);

#[cfg(target_os = "windows")]
const VM_PREFIX: &str = "bcvk-ephemeral-";

// --- Metadata ---

#[cfg(target_os = "windows")]
pub fn ephemeral_base_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("C:\\Users\\Public"))
        .join("bcvk")
        .join("ephemeral")
}

#[cfg(target_os = "windows")]
#[derive(Debug, Serialize, Deserialize)]
pub struct EphemeralVmMetadata {
    pub name: String,
    pub image: String,
    pub vm_name: String,
    pub ssh_port: u16,
    pub ssh_key: String,
    pub nbd_container: Option<String>,
    pub vsock_port: Option<u32>,
    pub created: String,
}

#[cfg(target_os = "windows")]
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
        let _ = std::fs::remove_file(path);
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

#[cfg(target_os = "windows")]
struct VmCleanup {
    vm_name: String,
    nbd_container: Option<String>,
    name: String,
    image: String,
    vhdx_path: Option<String>,
    ssh_forward: Option<SshForward>,
    vsock_port: Option<u32>,
}

#[cfg(target_os = "windows")]
impl Drop for VmCleanup {
    fn drop(&mut self) {
        debug!("cleaning up VM...");
        if let Some(ref fwd) = self.ssh_forward {
            fwd.stop();
        }
        let _ = hyperv::remove_vm(&self.vm_name);
        if let Some(ref name) = self.nbd_container {
            stop_nbdkit_container(name);
        }
        if let Some(ref vhdx) = self.vhdx_path {
            let _ = std::fs::remove_file(vhdx);
        }
        if let Ok(machine) = detect_machine_name() {
            let _ = Command::new("podman")
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
                .status();
        }
        if let Some(port) = self.vsock_port {
            let _ = hyperv::unregister_vsock_service(port);
        }
        EphemeralVmMetadata::remove(&self.name);
    }
}

/// Spawn cleanup as a detached process so bcvk can exit immediately.
#[cfg(target_os = "windows")]
fn spawn_cleanup(c: &VmCleanup) {
    let script = format!(
        "Stop-VM -Name '{}' -TurnOff -Force -ErrorAction SilentlyContinue; \
         Remove-VM -Name '{}' -Force -ErrorAction SilentlyContinue",
        c.vm_name, c.vm_name
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Some(ref name) = c.nbd_container {
        let _ = Command::new("podman")
            .args(["rm", "-f", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    if let Some(ref vhdx) = c.vhdx_path {
        let _ = std::fs::remove_file(vhdx);
    }
    if let Ok(machine) = detect_machine_name() {
        let _ = Command::new("podman")
            .args([
                "machine", "ssh", &machine, "--", "podman", "image", "umount", &c.image,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    if let Some(port) = c.vsock_port {
        let _ = hyperv::unregister_vsock_service(port);
    }
    EphemeralVmMetadata::remove(&c.name);
}

// --- Options ---

#[cfg(target_os = "windows")]
#[derive(clap::Parser, Debug)]
pub struct RunEphemeralOpts {
    /// Container image to boot
    pub image: String,
    /// Number of vCPUs
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

/// Parse memory specification string (e.g. "4G", "2048M") to megabytes.
#[cfg(target_os = "windows")]
pub fn parse_memory_to_mb(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        Ok((n.parse::<f64>()? * 1024.0) as u32)
    } else if let Some(n) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        Ok(n.parse::<f64>()? as u32)
    } else {
        Ok(s.parse::<u32>()?)
    }
}

#[cfg(target_os = "windows")]
fn default_vcpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(2)
        .min(4)
}

// --- Main entry point ---

#[cfg(target_os = "windows")]
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
}

#[cfg(target_os = "windows")]
impl RunContext {
    fn new(opts: &RunEphemeralOpts) -> Result<Self> {
        if !hyperv::is_hyper_v_enabled() {
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
        let rootful =
            inspect_json.contains("\"Rootful\": true") || inspect_json.contains("\"Rootful\":true");
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
        let vcpus = opts.vcpus.unwrap_or_else(default_vcpus);
        let memory_mb = parse_memory_to_mb(&opts.memory)?;
        let base_dir = ephemeral_base_dir();
        std::fs::create_dir_all(&base_dir)?;
        let ssh_key_path = base_dir.join(format!("{}-key", name));
        let nbd_container_name = format!("bcvk-nbd-{}", name);

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
        })
    }
}

#[cfg(target_os = "windows")]
struct Phase0Result {
    ssh_pubkey: String,
    digest_short: String,
    podman_vm_guid: String,
    merged_path: String,
    switch_handle: Option<std::thread::JoinHandle<Result<hyperv::SwitchInfo>>>,
    vm_handle: Option<std::thread::JoinHandle<Result<()>>>,
}

#[cfg(target_os = "windows")]
const VSOCK_PORT: u32 = 1030;

#[cfg(target_os = "windows")]
fn run_phase0(ctx: &RunContext, opts: &RunEphemeralOpts) -> Result<Phase0Result> {
    let need_ssh = opts.ssh_keygen || !opts.execute.is_empty();
    let ssh_key_path = ctx.ssh_key_path.clone();
    let ssh_handle = std::thread::spawn(move || -> Result<String> {
        if need_ssh {
            let _ = std::fs::remove_file(&ssh_key_path);
            let _ = std::fs::remove_file(ssh_key_path.with_extension("pub"));
            let status = Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-f"])
                .arg(&ssh_key_path)
                .args(["-N", "", "-q"])
                .status()?;
            if !status.success() {
                bail!("ssh-keygen failed");
            }
            Ok(std::fs::read_to_string(ssh_key_path.with_extension("pub"))?)
        } else {
            Ok(String::new())
        }
    });

    let switch_handle = {
        let sn = "bcvk".to_string();
        let hi = "10.0.0.1".to_string();
        std::thread::spawn(move || hyperv::ensure_internal_switch(&sn, &hi, 24))
    };

    let vm_handle = {
        let vn = ctx.vm_name.clone();
        let sn = "bcvk".to_string();
        let mem = ctx.memory_mb;
        let cpu = ctx.vcpus;
        std::thread::spawn(move || hyperv::create_gen2_vm(&vn, mem, cpu, &sn))
    };

    let machine_clone = ctx.machine.clone();
    let guid_handle = std::thread::spawn(move || -> Result<String> {
        hyperv::register_vsock_service(VSOCK_PORT)?;
        let vmtype = detect_podman_vmtype()?;
        info!("podman machine VM type: {}", vmtype);
        let guid = match vmtype.as_str() {
            "wsl" => hyperv::get_wsl_vm_guid(&machine_clone)?,
            "hyperv" => hyperv::get_vm_guid(&machine_clone)?,
            other => bail!("unsupported podman machine VM type: {}", other),
        };
        info!("podman machine VM GUID: {}", guid);
        Ok(guid)
    });

    let image_clone = opts.image.clone();
    let digest_handle =
        std::thread::spawn(move || boot_files::ensure_image_and_get_digest(&image_clone));

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
            "{pfx} pull -q {image}; MERGED=$({pfx} image mount {image}); echo MERGED=$MERGED",
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
    let digest_short = digest_handle
        .join()
        .map_err(|_| eyre!("digest panicked"))??;
    let podman_vm_guid = guid_handle.join().map_err(|_| eyre!("guid panicked"))??;
    let merged_path = mount_handle.join().map_err(|_| eyre!("mount panicked"))??;
    info!("image mounted at: {}", merged_path);

    Ok(Phase0Result {
        ssh_pubkey,
        digest_short,
        podman_vm_guid,
        merged_path,
        switch_handle: Some(switch_handle),
        vm_handle: Some(vm_handle),
    })
}

#[cfg(target_os = "windows")]
struct Phase1Result {
    vhdx_path: String,
    nbdkit_handle: Option<std::thread::JoinHandle<Result<Vec<u8>>>>,
}

#[cfg(target_os = "windows")]
fn run_phase1(ctx: &RunContext, p0: &Phase0Result) -> Result<Phase1Result> {
    let mut ssh_param_str = String::new();
    if !p0.ssh_pubkey.is_empty() {
        let param = format!("ssh_pubkey={}", p0.ssh_pubkey);
        let escaped = shlex::try_quote(&param)
            .map_err(|e| color_eyre::eyre::eyre!("shell escape failed: {}", e))?;
        ssh_param_str = format!(" {}", escaped);
    }

    let nbdkit_handle = {
        let ps = ctx.podman_ssh.clone();
        let run_cmd = if ctx.rootful { "sudo podman" } else { "podman" };
        let nbd_name = ctx.nbd_container_name.clone();
        let merged = p0.merged_path.clone();
        let ssh = ssh_param_str.clone();
        let run_str = run_cmd.to_string();
        let container_name = nbd_name.clone();
        std::thread::spawn(move || -> Result<Vec<u8>> {
            let nbdkit_script = format!(
                "if ! {run} image exists localhost/bcvk-nbdkit:latest 2>/dev/null; then \
                   echo BUILDING_NBDKIT_IMAGE; \
                   printf 'FROM quay.io/fedora/fedora:latest\\nRUN dnf install -y nbdkit && dnf clean all\\n' | \
                   {run} build -t localhost/bcvk-nbdkit:latest -f - /tmp; \
                 fi; \
                 for c in $({run} ps -a --filter name=bcvk-nbd- --format '{{{{.Names}}}}' 2>/dev/null); do {run} rm -f -t 0 $c 2>/dev/null; done; \
                 {run} rm -f -t 0 {name} 2>/dev/null; \
                 {run} run -d --name {name} --privileged \
                 --network=host --device /dev/vsock \
                 -v {merged}:{merged}:ro \
                 -v /var/tmp/bcvk:/bcvk:z,exec \
                 localhost/bcvk-nbdkit:latest \
                 nbdkit -fv --threads 4 --vsock -p 1030 -r /bcvk/libnbdkit_erofs_plugin.so \
                 dir={merged} \
                 'cmdline=root=PARTLABEL=bcvk-root ro rootfstype=erofs'{ssh}",
                run = run_str,
                name = nbd_name,
                merged = merged,
                ssh = ssh,
            );
            let result = ps.ssh_cmd(&nbdkit_script)?;
            for _ in 0..25 {
                std::thread::sleep(std::time::Duration::from_millis(200));
                let check = ps.ssh_cmd(&format!(
                    "{} ps --filter name={} --format '{{{{.Status}}}}'",
                    run_str, container_name
                ));
                if let Ok(out) = check {
                    if String::from_utf8_lossy(&out).contains("Up") {
                        return Ok(result);
                    }
                }
            }
            bail!("nbdkit container '{}' failed to start", container_name)
        })
    };

    let vhdx_path = boot_files::create_boot_vhdx(
        &p0.digest_short,
        &p0.merged_path,
        &ctx.podman_ssh,
        &p0.ssh_pubkey,
        VSOCK_PORT,
    )?;

    Ok(Phase1Result {
        vhdx_path,
        nbdkit_handle: Some(nbdkit_handle),
    })
}

#[cfg(target_os = "windows")]
fn run_phase2(
    ctx: &RunContext,
    p0: &Phase0Result,
    p1: &Phase1Result,
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

    let ephemeral_vm_guid = hyperv::attach_and_start_vm(&ctx.vm_name, &p1.vhdx_path)?;
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
                    Ok(f) => { file = Some(f); break; }
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
                    let _ = writeln!(log, "{}", l);
                    let _ = log.flush();
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
        vsock_port: Some(VSOCK_PORT),
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

    rt.block_on(async move {
        let dhcp = DhcpServer::new("10.0.0.1", "10.0.0.100")?;
        let dhcp_handle = dhcp.start_background();
        info!("VM {} started, VHDX booting...", vm_name);
        elapsed!("VM started");

        let _vsock_relay = crate::vsock_relay::VsockRelay::start(
            VSOCK_PORT,
            1,
            &podman_vm_guid,
            &ephemeral_vm_guid,
        )
        .await?;
        info!("vsock relay connected (port {})", VSOCK_PORT);
        elapsed!("relay connected");

        if gui {
            let _ = Command::new("vmconnect.exe")
                .args(["localhost", &vm_name])
                .spawn();
        }

        info!("serial log: {}", serial_log_path.display());

        let vm_ip = "10.0.0.100".to_string();
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
            vsock_port: Some(VSOCK_PORT),
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

#[cfg(target_os = "windows")]
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
    let phase0 = run_phase0(&ctx, &opts)?;
    elapsed!("image mount + setup");

    let mut phase0 = phase0;
    let phase1 = run_phase1(&ctx, &phase0)?;
    elapsed!("VHDX created");

    let switch = phase0
        .switch_handle
        .take()
        .unwrap()
        .join()
        .map_err(|_| eyre!("switch panicked"))??;
    info!("Internal Switch: {} ({})", switch.name, switch.host_ip);
    elapsed!("switch ready");
    phase0
        .vm_handle
        .take()
        .unwrap()
        .join()
        .map_err(|_| eyre!("VM panicked"))??;
    elapsed!("VM created");

    let mut phase1 = phase1;
    if let Err(e) = phase1
        .nbdkit_handle
        .take()
        .unwrap()
        .join()
        .map_err(|_| eyre!("nbdkit panicked"))?
    {
        info!("nbdkit warning: {}", e);
    }
    elapsed!("nbdkit ready");

    // VM start + serial + relay + DHCP + SSH
    run_phase2(&ctx, &phase0, &phase1, &opts, &t_start)
}

// --- Detached mode ---

#[cfg(target_os = "windows")]
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

    let _child = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()?;

    // Save metadata so bcvk ephemeral stop/ssh can find this VM
    let metadata = EphemeralVmMetadata {
        name: vm_name.clone(),
        image: opts.image.clone(),
        vm_name: format!("{}{}", VM_PREFIX, vm_name),
        ssh_port: 0, // Will be updated by child process once SSH is ready
        ssh_key: base
            .join(format!("{}-key", vm_name))
            .to_string_lossy()
            .to_string(),
        nbd_container: None, // Will be set by child process
        vsock_port: Some(1030),
        created: chrono::Utc::now().to_rfc3339(),
    };
    metadata.save()?;

    info!("started in background: {}", vm_name);
    info!("log: {}", log_path.display());
    println!("{}", vm_name);
    Ok(())
}

#[cfg(target_os = "windows")]
fn stop_nbdkit_container(name: &str) {
    let _ = Command::new("podman")
        .args(["rm", "-f", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// --- Shared helpers (ported from run_ephemeral_macos.rs, no Unix deps) ---

#[cfg(target_os = "windows")]
pub fn detect_machine_name() -> Result<String> {
    let output = Command::new("podman")
        .args(["machine", "info", "--format", "{{.Host.CurrentMachine}}"])
        .output()?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        bail!("no podman machine is running");
    }
    Ok(name)
}

#[cfg(target_os = "windows")]
pub fn detect_podman_vmtype() -> Result<String> {
    let output = Command::new("podman")
        .args(["machine", "info", "--format", "{{.Host.VMType}}"])
        .output()?;
    let vmtype = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_lowercase();
    if vmtype.is_empty() {
        bail!("could not detect podman machine VM type");
    }
    Ok(vmtype)
}

#[cfg(target_os = "windows")]
pub fn wait_for_ssh(port: u16, key_path: &Path, user: &str) -> Result<()> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    info!("waiting for SSH on port {}...", port);
    let start = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        if start.elapsed() > SSH_TIMEOUT {
            bail!("SSH connection timeout ({}s)", SSH_TIMEOUT.as_secs());
        }
        let mut cmd = Command::new("ssh");
        cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
        ssh_opts.apply_to_command(&mut cmd);
        cmd.args(["-o", "BatchMode=yes", &user_host, "true"]);
        if let Ok(s) = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status() {
            if s.success() {
                info!("SSH connected after {}s", start.elapsed().as_secs());
                return Ok(());
            }
        }
        let backoff = if attempt < 2 {
            500
        } else if attempt < 4 {
            1000
        } else {
            2000
        };
        std::thread::sleep(Duration::from_millis(backoff));
        attempt += 1;
    }
}

#[cfg(target_os = "windows")]
pub fn run_ssh_command(
    port: u16,
    key_path: &Path,
    user: &str,
    command: &str,
) -> Result<std::process::ExitStatus> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    let mut cmd = Command::new("ssh");
    cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
    ssh_opts.apply_to_command(&mut cmd);
    cmd.args(["-o", "BatchMode=yes", &user_host, command]);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| eyre!("ssh failed: {}", e))
}

#[cfg(target_os = "windows")]
pub fn run_ssh_interactive(
    port: u16,
    key_path: &Path,
    user: &str,
) -> Result<std::process::ExitStatus> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    let mut cmd = Command::new("ssh");
    cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
    ssh_opts.apply_to_command(&mut cmd);
    cmd.args(["-t", &user_host]);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| eyre!("ssh failed: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_memory_to_mb() {
        assert_eq!(parse_memory_to_mb("4G").unwrap(), 4096);
        assert_eq!(parse_memory_to_mb("4g").unwrap(), 4096);
        assert_eq!(parse_memory_to_mb("2048M").unwrap(), 2048);
        assert_eq!(parse_memory_to_mb("2048m").unwrap(), 2048);
        assert_eq!(parse_memory_to_mb("1024").unwrap(), 1024);
        assert_eq!(parse_memory_to_mb("  8G  ").unwrap(), 8192);
    }

    #[test]
    fn test_parse_memory_to_mb_errors() {
        assert!(parse_memory_to_mb("abc").is_err());
        assert!(parse_memory_to_mb("").is_err());
    }

    #[test]
    fn test_default_vcpus() {
        let vcpus = default_vcpus();
        assert!(vcpus >= 1 && vcpus <= 4);
    }

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
            created: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: EphemeralVmMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-vm");
        assert_eq!(parsed.ssh_port, 2222);
        assert_eq!(parsed.nbd_container, Some("bcvk-nbd-test".to_string()));
    }
}
