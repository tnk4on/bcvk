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
        let _ = std::fs::remove_file(path);
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
    let meta = VmMetadata::load(name)?;
    let state = vm::get_vm_state(&meta.vm_name)?;
    if !state.contains("Running") {
        println!(
            "VM '{}' is not running (state: {})",
            name,
            state.to_lowercase()
        );
        return Ok(());
    }
    vm::stop_vm(&meta.vm_name)?;
    println!("Stopped VM '{}'", name);
    Ok(())
}

fn start(name: &str) -> Result<()> {
    let meta = VmMetadata::load(name)?;
    let state = vm::get_vm_state(&meta.vm_name)?;
    if state.contains("Running") {
        println!("VM '{}' is already running", name);
        return Ok(());
    }
    vm::start_vm(&meta.vm_name)?;
    println!("Started VM '{}'", name);
    Ok(())
}
