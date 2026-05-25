//! Persistent VM management for Windows using Hyper-V.
//!
//! Subcommands mirror the Linux libvirt/ module structure:
//! run, list, ssh, rm (TBD).
//! Infrastructure modules (vm, boot_files, dhcp, ssh_forward, vsock_relay) are
//! shared with ephemeral mode (run_ephemeral_windows).

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

// Persistent VM subcommands (TBD)
#[allow(dead_code)]
mod list;
#[allow(dead_code)]
mod rm;
#[allow(dead_code)]
mod run;
#[allow(dead_code)]
mod ssh;

/// Subcommands for persistent VM management via Hyper-V.
#[allow(dead_code)]
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

    /// Remove a VM and its metadata
    #[clap(name = "rm")]
    Remove(rm::HypervRmOpts),
}

#[allow(dead_code)]
impl HypervCommands {
    pub fn run(self) -> Result<()> {
        match self {
            HypervCommands::Run(opts) => run::run(opts),
            HypervCommands::List { json } => list::run(json),
            HypervCommands::Ssh(opts) => ssh::run(opts),
            HypervCommands::Remove(opts) => rm::run(opts),
        }
    }
}
