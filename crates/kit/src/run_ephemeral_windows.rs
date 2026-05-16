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
            stop_nbdkit_container(name);
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

    // rootful check: podman machine inspect (no SSH needed)
    let rootful = {
        let out = Command::new("podman")
            .args(["machine", "inspect", &machine])
            .stdout(Stdio::piped()).stderr(Stdio::null())
            .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout.contains("\"Rootful\": true") || stdout.contains("\"Rootful\":true")
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

    // 1. Start nbdkit (Windows: podman リモートクライアント経由、podman machine ssh 不使用)
    let nbd_port = find_available_nbd_port();
    let nbd_container_name = format!("bcvk-nbd-{}", name);

    // 既存コンテナを削除
    let _ = Command::new("podman")
        .args(["rm", "-f", &nbd_container_name])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();

    // nbdkit: Podman Machine に直接 SSH して起動 (podman machine ssh ではなく ssh コマンド)
    // podman machine inspect から SSH ポートとキーを取得
    let inspect_out = Command::new("podman")
        .args(["machine", "inspect", &machine])
        .stdout(Stdio::piped()).stderr(Stdio::null())
        .output()?;
    let inspect_json = String::from_utf8_lossy(&inspect_out.stdout);

    // SSH port を抽出
    let pm_ssh_port = inspect_json.lines()
        .find(|l| l.contains("\"Port\""))
        .and_then(|l| l.trim().trim_matches(|c: char| !c.is_ascii_digit()).parse::<u16>().ok())
        .unwrap_or(22);
    // SSH identity path を抽出
    let pm_identity = inspect_json.lines()
        .find(|l| l.contains("\"IdentityPath\""))
        .map(|l| l.split('"').nth(3).unwrap_or("").to_string())
        .unwrap_or_default();

    info!("Podman Machine SSH: port={}, key={}", pm_ssh_port, pm_identity);

    fn shell_escape(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    // image mount + nbdkit start を 1 コマンドで実行
    let mut ssh_param_str = String::new();
    if !ssh_pubkey.is_empty() {
        ssh_param_str = format!(" {}", shell_escape(&format!("ssh_pubkey={}", ssh_pubkey)));
    }
    let remote_script = format!(
        "sudo podman pull -q {image}; \
         MERGED=$(sudo podman image mount {image}); \
         echo MERGED=$MERGED; \
         sudo podman rm -f {name} 2>/dev/null; \
         sudo podman run -d --name {name} --security-opt label=disable \
         -p {port}:10809 \
         -v $MERGED:$MERGED:ro \
         -v /var/tmp/bcvk:/bcvk:z,exec \
         quay.io/fedora/fedora:latest \
         sh -c \"dnf install -y nbdkit >/dev/null 2>&1; \
         exec nbdkit -fv -p 10809 -r /bcvk/libnbdkit_erofs_plugin.so \
         dir=$MERGED \
         'cmdline=root=/dev/vda2 ro rootfstype=erofs'{ssh}\"",
        image = opts.image,
        name = nbd_container_name,
        port = nbd_port,
        ssh = ssh_param_str,
    );

    let output = Command::new("ssh")
        .args([
            "-o", "ConnectTimeout=10",
            "-o", "ServerAliveInterval=5",
            "-o", "ServerAliveCountMax=3",
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR",
            "-i", &pm_identity,
            "-p", &pm_ssh_port.to_string(),
            "core@127.0.0.1",
            &remote_script,
        ])
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to start nbdkit: {} {}", stderr.trim(), stdout.trim());
    }
    // Extract MERGED path from nbdkit output
    let merged_path = stdout.lines()
        .find(|l| l.starts_with("MERGED="))
        .map(|l| l.trim_start_matches("MERGED=").trim().to_string())
        .unwrap_or_default();
    if merged_path.is_empty() {
        bail!("failed to get MERGED path from nbdkit output");
    }
    info!("image mounted at: {}", merged_path);
    let nbd_container = nbd_container_name.clone();
    info!("nbdkit container started on port {}", nbd_port);

    // nbdkit ready 待ち
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::time::Instant::now() > deadline {
            bail!("nbdkit did not become ready in 30s");
        }
        if std::net::TcpStream::connect(format!("127.0.0.1:{}", nbd_port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    info!("nbdkit ready on port {}", nbd_port);

    // 2. Internal Switch for PXE (bcvk is sole DHCP server, no ICS competition)
    let switch_name = "bcvk-pxe";
    let host_ip = "10.0.0.1";
    let client_ip = "10.0.0.100";
    let switch = hyperv::ensure_internal_switch(switch_name, host_ip, 24)?;
    info!("Internal Switch: {} ({})", switch.name, switch.host_ip);

    // Extract boot files — VM connects to host via TCP for NBD
    let boot_files = boot_files::extract_boot_files(&merged_path, &ssh_pubkey, host_ip, nbd_port)?;

    // 4. PXE Server (full DHCP + TFTP on Internal Switch)
    let pxe = PxeServer::new(host_ip, client_ip, boot_files)?;

    // 5. Firewall rules
    hyperv::add_pxe_firewall_rules(nbd_port)?;

    // 6. Create VM on Internal Switch (1 NIC)
    hyperv::create_gen2_vm(&vm_name, memory_mb, vcpus, switch_name)?;
    hyperv::set_pxe_boot(&vm_name)?;

    let mut cleanup = VmCleanup {
        vm_name: vm_name.clone(),
        nbd_container: Some(nbd_container.clone()),
        name: name.clone(),
        pxe_server: None,
        ssh_forward: None,
    };

    // Run PXE server + NBD proxy + VM boot + SSH in async runtime
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // Start PXE server in background
        let (dhcp_handle, tftp_handle) = pxe.start_background();

        // Start VM (NBD via TCP directly, no hv_sock proxy needed)
        hyperv::start_vm(&vm_name)?;
        info!("VM {} started, PXE booting...", vm_name);

        // Serial console log: read named pipe → log file (background)
        let serial_log_path = base_dir.join(format!("serial-{}.log", name));
        let pipe_path = format!("\\\\.\\pipe\\bcvk-serial-{}", vm_name);
        let serial_log_path_clone = serial_log_path.clone();
        let debug_mode = opts.debug;
        let _serial_handle = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, BufReader, Write};
            let pipe = match std::fs::File::open(&pipe_path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("serial pipe open failed ({}): {}", pipe_path, e);
                    return;
                }
            };
            let mut log = match std::fs::File::create(&serial_log_path_clone) {
                Ok(f) => f,
                Err(_) => return,
            };
            let reader = BufReader::new(pipe);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let _ = writeln!(log, "{}", l);
                        let _ = log.flush();
                        if debug_mode {
                            eprintln!("[serial] {}", l);
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        info!("serial log: {}", serial_log_path.display());

        // Wait for VM to get IP, fall back to DHCP-assigned IP
        let ip_deadline = std::time::Instant::now() + Duration::from_secs(30);
        let vm_ip = loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let name_clone = vm_name.clone();
            let result = tokio::task::spawn_blocking(move || {
                hyperv::get_vm_ip(&name_clone)
            }).await?;
            if let Ok(Some(ip)) = result {
                break ip;
            }
            if std::time::Instant::now() > ip_deadline {
                info!("Hyper-V did not report VM IP, using DHCP-assigned {}", client_ip);
                break client_ip.to_string();
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

// --- nbdkit helpers ---

#[cfg(target_os = "windows")]
fn find_available_nbd_port() -> u16 {
    for port in 10800..10900 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    10800
}

#[cfg(target_os = "windows")]
fn stop_nbdkit_container(name: &str) {
    let _ = Command::new("podman")
        .args(["rm", "-f", name])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
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
