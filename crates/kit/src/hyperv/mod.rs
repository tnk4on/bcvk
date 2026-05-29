//! Persistent VM management for Windows using Hyper-V.
//!
//! Subcommands mirror the macOS vfkit/ and Linux libvirt/ module structure:
//! run, list, ssh, rm, stop, start.
//! Infrastructure modules (vm, boot_files, dhcp, ssh_forward, vsock_relay) are
//! shared with ephemeral mode (run_ephemeral_windows).

use std::path::PathBuf;

use clap::Subcommand;
use color_eyre::Result;

// Shared infrastructure (used by both ephemeral and persistent VMs)
pub(crate) mod boot_files;
pub(crate) mod dhcp;
pub(crate) mod ssh_forward;
#[allow(dead_code)]
pub(crate) mod vm;
#[allow(unsafe_code)]
pub(crate) mod vsock_relay;

// Persistent VM subcommands
mod list;
mod rm;
mod run;
mod ssh;

/// Subcommands for persistent VM management via Hyper-V.
#[derive(Debug, Subcommand)]
pub enum HypervCommands {
    /// Run a persistent VM from a bootc image
    Run(run::HypervRunOpts),

    /// List all persistent VMs
    #[clap(name = "list", alias = "ls")]
    List {
        /// Output in JSON format
        #[clap(long)]
        json: bool,
    },

    /// SSH into a running VM
    Ssh(ssh::HypervSshOpts),

    /// Stop a running VM
    Stop {
        /// VM name
        name: String,
    },

    /// Start a stopped VM
    Start {
        /// VM name
        name: String,
    },

    /// Remove a VM and its metadata
    #[clap(name = "rm")]
    Remove(rm::HypervRmOpts),
}

impl HypervCommands {
    pub fn run(self) -> Result<()> {
        match self {
            HypervCommands::Run(opts) => run::run(opts),
            HypervCommands::List { json } => list::run(json),
            HypervCommands::Ssh(opts) => ssh::run(opts),
            HypervCommands::Stop { name } => stop(&name),
            HypervCommands::Start { name } => start(&name),
            HypervCommands::Remove(opts) => rm::run(opts),
        }
    }
}

// --- VM Metadata ---

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct VmMetadata {
    pub name: String,
    pub image: String,
    pub vm_name: String,
    pub ssh_port: u16,
    pub ssh_key: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub vhdx_path: String,
    #[serde(default)]
    pub switch_name: String,
    #[serde(default)]
    pub subnet: u8,
    #[serde(default)]
    pub service_pid: u32,
    #[serde(default)]
    pub state: String,
    pub created: String,
}

impl VmMetadata {
    pub fn vms_dir() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("bcvk")
            .join("vms")
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::vms_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.name));
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(name: &str) -> Result<Self> {
        let path = Self::vms_dir().join(format!("{}.json", name));
        let data = std::fs::read_to_string(&path)
            .map_err(|_| color_eyre::eyre::eyre!("VM '{}' not found", name))?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn remove(name: &str) {
        let path = Self::vms_dir().join(format!("{}.json", name));
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("failed to remove log {}: {}", path.display(), e);
            }
        }
    }

    pub fn list_all() -> Result<Vec<Self>> {
        let dir = Self::vms_dir();
        let mut vms = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "json") {
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

// --- Stop / Start (inline, simple enough to not need separate files) ---

fn stop(name: &str) -> Result<()> {
    let mut meta = VmMetadata::load(name)?;
    let state = vm::get_vm_state(&meta.vm_name)?;
    if !state.contains("Running") {
        println!(
            "VM '{}' is not running (state: {})",
            name,
            state.to_lowercase()
        );
        return Ok(());
    }
    kill_vm_service(&meta);
    vm::stop_vm(&meta.vm_name)?;
    meta.state = "stopped".into();
    meta.service_pid = 0;
    meta.save()?;
    println!("Stopped VM '{}'", name);
    Ok(())
}

fn start(name: &str) -> Result<()> {
    let mut meta = VmMetadata::load(name)?;
    let state = vm::get_vm_state(&meta.vm_name)?;
    if state.contains("Running") {
        println!("VM '{}' is already running", name);
        return Ok(());
    }
    vm::start_vm(&meta.vm_name)?;
    spawn_vm_service(name, &mut meta)?;
    println!("Started VM '{}'", name);
    println!("Use 'bcvk vm ssh {}' to connect.", name);
    Ok(())
}

pub(crate) fn spawn_vm_service(name: &str, meta: &mut VmMetadata) -> Result<()> {
    let exe = std::env::current_exe()?;
    let vms_dir = VmMetadata::vms_dir();
    let log_path = vms_dir.join(format!("{}.log", name));
    let log_file = std::fs::File::create(&log_path)?;

    let child = std::process::Command::new(exe)
        .args(["vm", "run", "--_internal", name])
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .spawn()?;

    meta.service_pid = child.id();
    meta.state = "running".into();
    meta.save()?;
    tracing::info!(
        "service process started (PID {}), log: {}",
        child.id(),
        log_path.display()
    );
    Ok(())
}

pub(crate) fn kill_vm_service(meta: &VmMetadata) {
    if meta.service_pid == 0 {
        return;
    }
    if let Err(e) = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &meta.service_pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        tracing::debug!("failed to kill service process {}: {}", meta.service_pid, e);
    }
}

pub(crate) fn run_vm_service(name: &str) -> Result<()> {
    let meta = VmMetadata::load(name)?;
    let server_ip = format!("10.0.{}.1", meta.subnet);
    let client_ip = format!("10.0.{}.100", meta.subnet);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let dhcp = dhcp::DhcpServer::new(&server_ip, &client_ip, &meta.switch_name)?;
        let _dhcp_handle = dhcp.start_background();
        tracing::info!("persistent VM '{}': DHCP started on {}", name, server_ip);

        let _ssh_fwd = ssh_forward::SshForward::start_on_port(&client_ip, meta.ssh_port).await?;
        tracing::info!(
            "persistent VM '{}': SSH forward on port {}",
            name,
            meta.ssh_port
        );

        let key_path = std::path::Path::new(&meta.ssh_key);
        crate::run_ephemeral_windows::wait_for_ssh(meta.ssh_port, key_path, "root")?;
        tracing::info!("persistent VM '{}': SSH ready", name);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let state = vm::get_vm_state(&meta.vm_name).unwrap_or_default();
            if !state.contains("Running") {
                tracing::info!("persistent VM '{}': VM stopped, exiting service", name);
                break;
            }
        }
        dhcp.stop();
        Ok::<(), color_eyre::Report>(())
    })?;
    Ok(())
}
