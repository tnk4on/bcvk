//! Ephemeral VM launch flow for Windows using Hyper-V VHDX + NBD EROFS.
//!
//! Architecture:
//! 1. nbdkit erofs plugin serves EROFS rootfs via NBD (podman run -p)
//! 2. bcvk extracts boot files (kernel, initramfs, GRUB) to memory
//! 3. bcvk runs DHCP server on Internal Switch
//! 4. Hyper-V Gen2 VM VHDX boots → GRUB → kernel → dracut NBD → EROFS rootfs

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
use crate::dhcp_server::DhcpServer;
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
    image: String,
    vhdx_path: Option<String>,
    ssh_forward: Option<SshForward>,
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
        // Release container image overlay mount
        if let Ok(machine) = detect_machine_name() {
            let _ = Command::new("podman")
                .args(["machine", "ssh", &machine, "--", "podman", "image", "umount", &self.image])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
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
    if opts.gui && opts.detach {
        bail!("--gui and --detach cannot be used together");
    }

    if opts.detach {
        return run_detached(&opts);
    }

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

    // Ensure image exists and get digest for caching
    let digest_short = boot_files::ensure_image_and_get_digest(&opts.image)?;

    // 1. Start nbdkit
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

    let podman_ssh = boot_files::PodmanSsh {
        port: pm_ssh_port,
        key: pm_identity.clone(),
        rootful,
    };

    // image mount + nbdkit start を 1 コマンドで実行
    let mut ssh_param_str = String::new();
    if !ssh_pubkey.is_empty() {
        let param = format!("ssh_pubkey={}", ssh_pubkey);
        let escaped = shlex::try_quote(&param)
            .map_err(|e| color_eyre::eyre::eyre!("shell escape failed: {}", e))?;
        ssh_param_str = format!(" {}", escaped);
    }
    let podman_prefix = if rootful { "sudo podman" } else { "podman unshare podman" };
    let podman_run = if rootful { "sudo podman" } else { "podman" };
    let remote_script = format!(
        "{pfx} pull -q {image}; \
         MERGED=$({pfx} image mount {image}); \
         echo MERGED=$MERGED; \
         {run} rm -f {name} 2>/dev/null; \
         {run} run -d --name {name} --security-opt label=disable \
         -p {port}:10809 \
         -v $MERGED:$MERGED:ro \
         -v /var/tmp/bcvk:/bcvk:z,exec \
         quay.io/fedora/fedora:latest \
         sh -c \"dnf install -y nbdkit >/dev/null 2>&1; \
         exec nbdkit -fv -p 10809 -r /bcvk/libnbdkit_erofs_plugin.so \
         dir=$MERGED \
         'cmdline=root=/dev/vda2 ro rootfstype=erofs'{ssh}\"",
        pfx = podman_prefix,
        run = podman_run,
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

    // Parallel setup: switch + VM + firewall while waiting for nbdkit
    let switch_name = "bcvk";
    let host_ip = "10.0.0.1";
    let client_ip = "10.0.0.100";

    // Start parallel tasks
    let switch_handle = {
        let sn = switch_name.to_string();
        let hi = host_ip.to_string();
        std::thread::spawn(move || hyperv::ensure_internal_switch(&sn, &hi, 24))
    };
    let vm_handle = {
        let vn = vm_name.clone();
        let sn = switch_name.to_string();
        std::thread::spawn(move || -> Result<()> {
            hyperv::create_gen2_vm(&vn, memory_mb, vcpus, &sn)?;
            Ok(())
        })
    };
    let firewall_handle = {
        std::thread::spawn(move || hyperv::add_firewall_rules(nbd_port))
    };
    let vhdx_handle = {
        let ds = digest_short.clone();
        let mp = merged_path.clone();
        let spk = ssh_pubkey.clone();
        let hi = host_ip.to_string();
        let ps = podman_ssh.clone();
        std::thread::spawn(move || {
            boot_files::create_boot_vhdx(&ds, &mp, &ps, &spk, &hi, nbd_port)
        })
    };

    // Wait for nbdkit ready (concurrent with all parallel tasks)
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

    // Collect parallel results
    let switch = switch_handle.join().map_err(|_| color_eyre::eyre::eyre!("switch thread panicked"))??;
    info!("Internal Switch: {} ({})", switch.name, switch.host_ip);

    vm_handle.join().map_err(|_| color_eyre::eyre::eyre!("VM thread panicked"))??;
    firewall_handle.join().map_err(|_| color_eyre::eyre::eyre!("firewall thread panicked"))??;

    let vhdx_path = vhdx_handle.join().map_err(|_| color_eyre::eyre::eyre!("VHDX thread panicked"))??;

    // Attach VHDX and set boot device
    hyperv::add_vhdx_boot(&vm_name, &vhdx_path)?;

    let mut cleanup = VmCleanup {
        vm_name: vm_name.clone(),
        nbd_container: Some(nbd_container.clone()),
        name: name.clone(),
        image: opts.image.clone(),
        vhdx_path: Some(vhdx_path),
        ssh_forward: None,
    };

    // Run DHCP server + VM boot + SSH in async runtime
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let dhcp = DhcpServer::new(host_ip, client_ip)?;
        let dhcp_handle = dhcp.start_background();

        // Start VM from VHDX
        hyperv::start_vm(&vm_name)?;
        info!("VM {} started, VHDX booting...", vm_name);

        // Open Hyper-V console window if --gui
        if opts.gui {
            let _ = Command::new("vmconnect.exe")
                .args(["localhost", &vm_name])
                .spawn();
        }

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
            let status = crate::run_ephemeral_windows::run_ssh_interactive(ssh_port, &ssh_key_path, "root")?;
            let exit_code = status.code().unwrap_or(1);
            dhcp.stop();
            dhcp_handle.abort();
            drop(cleanup);
            std::process::exit(exit_code);
        }

        dhcp.stop();
        dhcp_handle.abort();
        Ok::<(), color_eyre::Report>(())
    })?;

    Ok(())
}

// --- Detached mode ---

#[cfg(target_os = "windows")]
fn run_detached(opts: &RunEphemeralOpts) -> Result<()> {
    let base = ephemeral_base_dir();
    std::fs::create_dir_all(&base)?;

    let vm_name = opts.name.clone().unwrap_or_else(|| {
        format!("{:08x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32)
    });
    let log_path = base.join(format!("bcvk-{}.log", vm_name));
    let log_file = std::fs::File::create(&log_path)?;

    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--detach" && a != "-d")
        .collect();
    if !args.contains(&"-K".to_string()) && !args.contains(&"--ssh-keygen".to_string()) {
        args.insert(args.len() - 1, "-K".to_string());
    }
    if opts.name.is_none() {
        args.insert(args.len() - 1, "--name".to_string());
        args.insert(args.len() - 1, vm_name.clone());
    }

    let _child = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()?;

    info!("started in background: {}", vm_name);
    info!("log: {}", log_path.display());
    println!("{}", vm_name);
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
            nbd_port: Some(10800),
            created: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: EphemeralVmMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "test-vm");
        assert_eq!(parsed.ssh_port, 2222);
        assert_eq!(parsed.nbd_container, Some("bcvk-nbd-test".to_string()));
    }
}
