//! Ephemeral VM launch flow for Windows using Hyper-V PXE + NBD EROFS.
//!
//! Architecture:
//! 1. nbdkit erofs plugin serves EROFS rootfs via NBD (podman run -p)
//! 2. bcvk extracts boot files (kernel, initramfs, GRUB) to memory
//! 3. bcvk runs DHCP + TFTP server on Internal Switch
//! 4. Hyper-V Gen2 VM PXE boots → GRUB → kernel → dracut NBD → EROFS rootfs

#[cfg(target_os = "windows")]
use color_eyre::{eyre::{bail, eyre}, Result};
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
use crate::hyperv;
#[cfg(target_os = "windows")]
use crate::pxe_server::PxeServer;
#[cfg(target_os = "windows")]
use crate::ssh_forward::SshForward;

#[cfg(target_os = "windows")]
const SSH_TIMEOUT: Duration = Duration::from_secs(240);

#[cfg(target_os = "windows")]
const SWITCH_NAME: &str = "bcvk-pxe";
#[cfg(target_os = "windows")]
const HOST_IP: &str = "10.0.0.1";
#[cfg(target_os = "windows")]
const CLIENT_IP: &str = "10.0.0.100";
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
    pub nbd_port: Option<u16>,
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
                if entry.path().extension().map(|e| e == "json").unwrap_or(false) {
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
    pxe_server: Option<PxeServer>,
    ssh_forward: Option<SshForward>,
}

#[cfg(target_os = "windows")]
impl Drop for VmCleanup {
    fn drop(&mut self) {
        debug!("cleaning up VM...");
        if let Some(ref fwd) = self.ssh_forward {
            fwd.stop();
        }
        if let Some(ref pxe) = self.pxe_server {
            pxe.stop();
        }
        let _ = hyperv::remove_vm(&self.vm_name);
        if let Some(ref name) = self.nbd_container {
            crate::nbdkit_macos::stop_nbdkit_container(name);
        }
        EphemeralVmMetadata::remove(&self.name);
    }
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
    /// Enable debug mode
    #[clap(long)]
    pub debug: bool,
}

#[cfg(target_os = "windows")]
pub fn parse_memory_to_mb(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        Ok(num.parse::<u32>()? * 1024)
    } else if let Some(num) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        Ok(num.parse::<u32>()?)
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
pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    // Preflight checks
    if !hyperv::is_hyper_v_enabled() {
        bail!("Hyper-V is not enabled. Run: Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V-All");
    }

    let machine = crate::run_ephemeral_windows::detect_machine_name()?;
    info!("podman machine: {}", machine);

    let rootful = {
        let out = std::process::Command::new("podman")
            .args(["machine", "ssh", &machine, "--", "id", "-u"])
            .output()?;
        String::from_utf8_lossy(&out.stdout).trim() == "0"
    };

    let vm_name_suffix = opts.name.clone().unwrap_or_else(|| {
        format!("{:08x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32)
    });
    let vm_name = format!("{}{}", VM_PREFIX, vm_name_suffix);
    let name = vm_name_suffix.clone();

    let vcpus = opts.vcpus.unwrap_or_else(default_vcpus);
    let memory_mb = parse_memory_to_mb(&opts.memory)?;

    // SSH key
    let base_dir = ephemeral_base_dir();
    std::fs::create_dir_all(&base_dir)?;
    let ssh_key_path = base_dir.join(format!("{}-key", name));
    let ssh_pubkey = if opts.ssh_keygen || !opts.execute.is_empty() {
        let _ = std::fs::remove_file(&ssh_key_path);
        let _ = std::fs::remove_file(ssh_key_path.with_extension("pub"));
        let status = std::process::Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-f"])
            .arg(&ssh_key_path)
            .args(["-N", "", "-q"])
            .status()?;
        if !status.success() {
            bail!("ssh-keygen failed");
        }
        std::fs::read_to_string(ssh_key_path.with_extension("pub"))?
    } else {
        String::new()
    };

    // 1. Start nbdkit
    let nbd_port = crate::nbdkit_macos::find_available_nbd_port();
    let merged_path = crate::nbdkit_macos::get_merged_path(&machine, rootful, &opts.image)?;
    info!("overlay merged: {}", merged_path);

    let cmdline = "root=/dev/vda2 ro rootfstype=erofs"; // cmdline for nbdkit plugin (GPT disk layout)
    let nbd_container = crate::nbdkit_macos::start_nbdkit_erofs_plugin(
        &machine, &merged_path, cmdline, &ssh_pubkey, nbd_port, &name,
    )?;
    info!("nbdkit on port {}", nbd_port);

    // 2. Extract boot files to memory
    let boot_files = boot_files::extract_boot_files(&opts.image, HOST_IP, nbd_port)?;

    // 3. Internal Switch
    let switch = hyperv::ensure_internal_switch(SWITCH_NAME, HOST_IP, 24)?;

    // 4. PXE Server
    let pxe = PxeServer::new(HOST_IP, CLIENT_IP, boot_files)?;

    // 5. Create + start VM
    hyperv::create_gen2_vm(&vm_name, memory_mb, vcpus, &switch.name)?;
    hyperv::set_pxe_boot(&vm_name)?;

    let mut cleanup = VmCleanup {
        vm_name: vm_name.clone(),
        nbd_container: Some(nbd_container.clone()),
        name: name.clone(),
        pxe_server: None,
        ssh_forward: None,
    };

    // Run PXE server + VM boot + SSH in async runtime
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // Start PXE server in background
        let (dhcp_handle, tftp_handle) = pxe.start_background();

        // Start VM
        hyperv::start_vm(&vm_name)?;
        info!("VM {} started, PXE booting...", vm_name);

        // Wait for VM to get IP
        let vm_ip = loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if let Ok(Some(ip)) = hyperv::get_vm_ip(&vm_name) {
                break ip;
            }
            debug!("waiting for VM IP...");
        };
        info!("VM IP: {}", vm_ip);

        // SSH forward
        let ssh_fwd = SshForward::start(&vm_ip).await?;
        let ssh_port = ssh_fwd.port();
        info!("SSH forward: localhost:{} → {}:22", ssh_port, vm_ip);
        cleanup.ssh_forward = Some(ssh_fwd);

        // Save metadata
        let metadata = EphemeralVmMetadata {
            name: name.clone(),
            image: opts.image.clone(),
            vm_name: vm_name.clone(),
            ssh_port,
            ssh_key: ssh_key_path.to_string_lossy().to_string(),
            nbd_container: Some(nbd_container.clone()),
            nbd_port: Some(nbd_port),
            created: chrono::Utc::now().to_rfc3339(),
        };
        metadata.save()?;

        // Wait for SSH
        crate::run_ephemeral_windows::wait_for_ssh(ssh_port, &ssh_key_path, "root")?;
        info!("SSH connected!");

        // Execute commands or interactive
        if !opts.execute.is_empty() {
            for cmd in &opts.execute {
                crate::run_ephemeral_windows::run_ssh_command(ssh_port, &ssh_key_path, "root", cmd).map(|_| ())?;
            }
        } else if ssh_pubkey.is_empty() {
            info!("VM running. Use: bcvk ephemeral ssh {}", name);
        } else {
            crate::run_ephemeral_windows::run_ssh_interactive(ssh_port, &ssh_key_path, "root").map(|_| ())?;
        }

        pxe.stop();
        dhcp_handle.abort();
        tftp_handle.abort();
        Ok::<(), color_eyre::Report>(())
    })?;

    Ok(())
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
        let backoff = if attempt < 2 { 500 } else if attempt < 4 { 1000 } else { 2000 };
        std::thread::sleep(Duration::from_millis(backoff));
        attempt += 1;
    }
}

#[cfg(target_os = "windows")]
pub fn run_ssh_command(port: u16, key_path: &Path, user: &str, command: &str) -> Result<std::process::ExitStatus> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    let mut cmd = Command::new("ssh");
    cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
    ssh_opts.apply_to_command(&mut cmd);
    cmd.args(["-o", "BatchMode=yes", &user_host, command]);
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .status().map_err(|e| eyre!("ssh failed: {}", e))
}

#[cfg(target_os = "windows")]
pub fn run_ssh_interactive(port: u16, key_path: &Path, user: &str) -> Result<std::process::ExitStatus> {
    use crate::ssh_options::CommonSshOptions;
    let ssh_opts = CommonSshOptions::default();
    let user_host = format!("{}@localhost", user);
    let mut cmd = Command::new("ssh");
    cmd.args(["-p", &port.to_string(), "-i", &key_path.to_string_lossy()]);
    ssh_opts.apply_to_command(&mut cmd);
    cmd.args(["-t", &user_host]);
    cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .status().map_err(|e| eyre!("ssh failed: {}", e))
}
