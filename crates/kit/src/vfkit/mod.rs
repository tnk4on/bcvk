//! Persistent VM management for macOS using vfkit + EFI boot.
//!
//! Subcommands mirror the Linux libvirt/ module structure:
//! run, list, ssh, stop, start, rm, rm-all, inspect

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clap::Subcommand;
use color_eyre::Result;

pub mod run;
pub mod list;
pub mod ssh;
pub mod stop;
pub mod start;
pub mod rm;
pub mod rm_all;
pub mod inspect;

#[derive(Debug, Subcommand)]
pub enum VmCommands {
    /// Run a persistent VM from a disk image
    Run(run::VmRunOpts),

    /// List all persistent VMs
    #[clap(name = "list", alias = "ls")]
    List {
        #[clap(long)]
        json: bool,
    },

    /// SSH into a running VM
    Ssh(ssh::VmSshOpts),

    /// Stop a running VM
    Stop {
        /// VM name
        name: String,
    },

    /// Start a stopped VM
    Start(start::VmStartOpts),

    /// Remove a VM and its metadata
    #[clap(name = "rm")]
    Remove(rm::VmRmOpts),

    /// Remove all VMs
    #[clap(name = "rm-all")]
    RemoveAll {
        #[clap(short, long)]
        force: bool,
    },

    /// Show detailed VM information
    Inspect {
        /// VM name
        name: String,
    },
}

impl VmCommands {
    pub fn run(self) -> Result<()> {
        match self {
            VmCommands::Run(opts) => run::run(opts),
            VmCommands::List { json } => list::run(json),
            VmCommands::Ssh(opts) => ssh::run(opts),
            VmCommands::Stop { name } => stop::run(&name),
            VmCommands::Start(opts) => start::run(opts),
            VmCommands::Remove(opts) => rm::run(opts),
            VmCommands::RemoveAll { force } => rm_all::run(force),
            VmCommands::Inspect { name } => inspect::run(&name),
        }
    }
}

// --- VM Metadata ---

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct VmMetadata {
    pub name: String,
    pub disk_image: String,
    pub vfkit_pid: u32,
    pub gvproxy_pid: u32,
    pub ssh_port: u16,
    pub ssh_key: String,
    pub ssh_user: String,
    pub cpus: u32,
    pub memory: u32,
    pub efi_store: String,
    pub serial_log: String,
    pub gui: bool,
    pub created: String,
    pub state: String,
}

impl VmMetadata {
    pub fn vms_dir() -> PathBuf {
        dirs::home_dir()
            .expect("cannot determine home directory")
            .join(".local/share/bcvk/vms")
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::vms_dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", self.name));
        fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn load(name: &str) -> Result<Self> {
        let path = Self::vms_dir().join(format!("{}.json", name));
        let data = fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn remove(name: &str) {
        let path = Self::vms_dir().join(format!("{}.json", name));
        let _ = fs::remove_file(path);
    }

    pub fn list_all() -> Result<Vec<Self>> {
        let dir = Self::vms_dir();
        if !dir.exists() { return Ok(Vec::new()); }
        let mut vms = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(meta) = serde_json::from_str::<Self>(&data) {
                    vms.push(meta);
                }
            }
        }
        Ok(vms)
    }

    pub fn is_alive(&self) -> bool {
        if self.vfkit_pid == 0 { return false; }
        Command::new("kill")
            .args(["-0", &self.vfkit_pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
