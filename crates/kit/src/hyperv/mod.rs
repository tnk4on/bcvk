//! Persistent VM management for Windows using Hyper-V.
//!
//! Subcommands mirror the macOS vfkit/ and Linux libvirt/ module structure:
//! run, list, ssh, rm, stop, start.
//! Infrastructure modules (vm, boot_files, dhcp, ssh_forward, vsock_relay) are
//! shared with ephemeral mode (run_ephemeral_windows).

use std::path::PathBuf;

use clap::Subcommand;
use color_eyre::{eyre::bail, Result};
use tracing::info;

// Shared infrastructure (used by both ephemeral and persistent VMs)
pub(crate) mod boot_files;
pub(crate) mod dhcp;
pub(crate) mod ssh_forward;
#[allow(dead_code)]
pub(crate) mod vm;
#[allow(unsafe_code)]
pub(crate) mod vsock_relay;

// Persistent VM subcommands
mod inspect;
mod list;
mod rm;
mod rm_all;
mod run;
mod ssh;

/// Subcommands for persistent VM management via Hyper-V.
#[derive(Debug, Subcommand)]
pub enum HypervCommands {
    /// Run a persistent VM from a bootc image
    Run(run::HypervRunOpts),

    /// List all persistent VMs
    #[clap(name = "list", alias = "ls")]
    List(list::HypervListOpts),

    /// SSH into a running VM
    Ssh(ssh::HypervSshOpts),

    /// Stop a running VM
    Stop {
        /// VM name
        name: String,

        /// Force immediate power-off (TurnOff) instead of graceful shutdown
        #[clap(long, short = 'f')]
        force: bool,
    },

    /// Start a stopped VM
    Start {
        /// VM name
        name: String,

        /// Automatically SSH into the VM after starting
        #[clap(long)]
        ssh: bool,

        /// Display VM console in Hyper-V Manager
        #[clap(long)]
        gui: bool,
    },

    /// Remove a VM and its metadata
    #[clap(name = "rm")]
    Remove(rm::HypervRmOpts),

    /// Remove all persistent VMs
    #[clap(name = "rm-all")]
    RemoveAll(rm_all::HypervRmAllOpts),

    /// Show detailed information about a VM
    Inspect(inspect::HypervInspectOpts),
}

impl HypervCommands {
    pub fn run(self) -> Result<()> {
        match self {
            HypervCommands::Run(opts) => run::run(opts),
            HypervCommands::List(opts) => list::run(opts),
            HypervCommands::Ssh(opts) => ssh::run(opts),
            HypervCommands::Stop { name, force } => stop(&name, force),
            HypervCommands::Start { name, ssh, gui } => start(&name, ssh, gui),
            HypervCommands::Remove(opts) => rm::run(opts),
            HypervCommands::RemoveAll(opts) => rm_all::run(opts),
            HypervCommands::Inspect(opts) => inspect::run(opts),
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
    pub gui: bool,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub port_mappings: Vec<(u16, u16)>,
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

fn stop(name: &str, force: bool) -> Result<()> {
    let mut meta = VmMetadata::load(name)?;
    let state = vm::get_vm_state(&meta.vm_name)?;
    if !state.contains("Running") {
        println!(
            "VM '{}' is already stopped (state: {})",
            name,
            state.to_lowercase()
        );
        return Ok(());
    }
    println!("Stopping VM '{}'...", name);
    kill_vm_service(&meta);
    if force {
        vm::turn_off_vm(&meta.vm_name)?;
    } else {
        vm::stop_vm(&meta.vm_name)?;
        let mut stopped = false;
        for _ in 0..60 {
            let s = vm::get_vm_state(&meta.vm_name).unwrap_or_default();
            if s.contains("Off") || s.is_empty() {
                stopped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        if !stopped {
            info!("ACPI shutdown timed out after 60s, forcing power off");
            vm::turn_off_vm(&meta.vm_name)?;
        }
    }
    meta.state = "stopped".into();
    meta.service_pid = 0;
    meta.save()?;
    println!("VM '{}' stopped successfully", name);
    Ok(())
}

fn start(name: &str, ssh: bool, gui: bool) -> Result<()> {
    let mut meta = VmMetadata::load(name)?;
    let state = vm::get_vm_state(&meta.vm_name)?;
    let use_gui = gui || meta.gui;
    if state.contains("Running") {
        if ssh {
            println!("Connecting to running VM...");
            let key_path = std::path::Path::new(&meta.ssh_key);
            crate::vm_helpers::wait_for_ssh(meta.ssh_port, key_path, "root")?;
            let status = crate::vm_helpers::run_ssh_interactive(meta.ssh_port, key_path, "root")?;
            std::process::exit(status.code().unwrap_or(1));
        }
        if use_gui {
            if let Err(e) = std::process::Command::new("vmconnect.exe")
                .args(["localhost", &meta.vm_name])
                .spawn()
            {
                tracing::debug!("failed to launch vmconnect: {}", e);
            }
        }
        bail!("VM '{}' is already running", name);
    }
    kill_vm_service(&meta);
    println!("Starting VM '{}'...", name);
    vm::start_vm(&meta.vm_name)?;
    meta.gui = use_gui;
    spawn_vm_service(name, &mut meta)?;
    println!("VM '{}' started successfully", name);
    println!("SSH will be available once the service process reports ready.");
    println!(
        "Use 'bcvk vm ssh {}' to connect (may take a few seconds).",
        name
    );
    if use_gui {
        if let Err(e) = std::process::Command::new("vmconnect.exe")
            .args(["localhost", &meta.vm_name])
            .spawn()
        {
            tracing::debug!("failed to launch vmconnect: {}", e);
        }
    }
    if ssh {
        let updated = VmMetadata::load(name).unwrap_or(meta);
        let key_path = std::path::Path::new(&updated.ssh_key);
        let status = crate::vm_helpers::run_ssh_interactive(updated.ssh_port, key_path, "root")?;
        std::process::exit(status.code().unwrap_or(1));
    }
    println!("Use 'bcvk vm ssh {}' to connect.", name);
    Ok(())
}

pub(crate) fn spawn_vm_service(name: &str, meta: &mut VmMetadata) -> Result<()> {
    let exe = std::env::current_exe()?;
    let vms_dir = VmMetadata::vms_dir();
    let log_path = vms_dir.join(format!("{}.log", name));
    let log_file = std::fs::File::create(&log_path)?;

    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let child = std::process::Command::new(exe)
        .args(["vm", "run", "--_internal", name])
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
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
    #[allow(unsafe_code)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        let handle = OpenProcess(PROCESS_TERMINATE, false, meta.service_pid);
        match handle {
            Ok(h) => {
                if let Err(e) = TerminateProcess(h, 1) {
                    tracing::debug!("TerminateProcess failed: {}", e);
                }
                if let Err(e) = CloseHandle(h) {
                    tracing::debug!("CloseHandle failed: {}", e);
                }
            }
            Err(e) => {
                tracing::debug!("failed to open process {}: {}", meta.service_pid, e);
            }
        }
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

        let _ssh_fwd = match ssh_forward::SshForward::start_on_port(&client_ip, meta.ssh_port).await
        {
            Ok(fwd) => fwd,
            Err(e) => {
                tracing::info!(
                    "persistent VM '{}': port {} bind failed ({}), using random port",
                    name,
                    meta.ssh_port,
                    e
                );
                let fwd = ssh_forward::SshForward::start(&client_ip).await?;
                let new_port = fwd.port();
                // Update metadata with new port
                if let Ok(mut updated_meta) = VmMetadata::load(name) {
                    updated_meta.ssh_port = new_port;
                    let _ = updated_meta.save();
                }
                fwd
            }
        };
        let actual_port = _ssh_fwd.port();
        tracing::info!("SSH forward: localhost:{}  {}:22", actual_port, client_ip);
        tracing::info!(
            "persistent VM '{}': SSH forward on port {}",
            name,
            actual_port
        );

        // Additional port forwarding
        let mut _port_fwds = Vec::new();
        for (host_port, guest_port) in &meta.port_mappings {
            let fwd = ssh_forward::SshForward::start_on_ports(&client_ip, *host_port, *guest_port)
                .await?;
            tracing::info!(
                "persistent VM '{}': port forward {}:{}",
                name,
                host_port,
                guest_port
            );
            _port_fwds.push(fwd);
        }

        // Wait for VM to reach Running state before checking SSH
        for _ in 0..60 {
            let s = vm::get_vm_state(&meta.vm_name).unwrap_or_default();
            if s.contains("Running") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        let key_path = std::path::Path::new(&meta.ssh_key);
        crate::vm_helpers::wait_for_ssh(actual_port, key_path, "root")?;
        tracing::info!("persistent VM '{}': SSH ready", name);

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let state = vm::get_vm_state(&meta.vm_name).unwrap_or_default();
            if !state.contains("Running") && !state.contains("Starting") && !state.is_empty() {
                tracing::info!(
                    "persistent VM '{}': VM stopped (state: {}), exiting service",
                    name,
                    state
                );
                break;
            }
        }
        dhcp.stop();
        Ok::<(), color_eyre::Report>(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vm_metadata(name: &str) -> VmMetadata {
        VmMetadata {
            name: name.to_string(),
            image: "quay.io/fedora/fedora-bootc:latest".to_string(),
            vm_name: format!("bcvk-{}", name),
            ssh_port: 2222,
            ssh_key: "/tmp/key".to_string(),
            vcpus: 4,
            memory_mb: 4096,
            vhdx_path: "/tmp/disk.vhdx".to_string(),
            switch_name: format!("bcvk-{}", name),
            subnet: 128,
            service_pid: 0,
            gui: false,
            state: "running".to_string(),
            labels: vec![],
            port_mappings: vec![],
            created: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn test_vm_metadata_roundtrip() {
        let meta = sample_vm_metadata("test-vm");
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let loaded: VmMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.name, "test-vm");
        assert_eq!(loaded.vm_name, "bcvk-test-vm");
        assert_eq!(loaded.vcpus, 4);
        assert_eq!(loaded.memory_mb, 4096);
        assert_eq!(loaded.ssh_port, 2222);
        assert_eq!(loaded.state, "running");
    }

    #[test]
    fn test_vm_metadata_save_load_remove() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("myvm.json");
        let meta = sample_vm_metadata("myvm");
        std::fs::write(&json_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        let data = std::fs::read_to_string(&json_path).unwrap();
        let loaded: VmMetadata = serde_json::from_str(&data).unwrap();
        assert_eq!(loaded.name, "myvm");
        assert_eq!(loaded.ssh_port, 2222);
        std::fs::remove_file(&json_path).unwrap();
        assert!(!json_path.exists());
    }

    #[test]
    fn test_vm_metadata_list_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            let meta = sample_vm_metadata(&format!("vm-{i}"));
            let path = dir.path().join(format!("vm-{i}.json"));
            std::fs::write(&path, serde_json::to_string(&meta).unwrap()).unwrap();
        }
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let mut vms = Vec::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<VmMetadata>(&data) {
                    vms.push(meta);
                }
            }
        }
        assert_eq!(vms.len(), 3);
        let mut names: Vec<_> = vms.iter().map(|v| v.name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["vm-0", "vm-1", "vm-2"]);
    }
}
