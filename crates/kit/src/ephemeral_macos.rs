//! Ephemeral VM management commands for macOS (vfkit backend).

use std::io::Write;
use std::process::{Command, Stdio};

use clap::Subcommand;
use color_eyre::Result;
use color_eyre::eyre::bail;

use crate::run_ephemeral_macos::{self, EphemeralVmMetadata};

#[derive(Debug, Subcommand)]
pub enum EphemeralCommands {
    /// Run bootc containers as ephemeral VMs
    #[clap(name = "run")]
    Run(run_ephemeral_macos::RunEphemeralOpts),

    /// Run ephemeral VM and SSH into it
    #[clap(name = "run-ssh")]
    RunSsh(run_ephemeral_macos::RunEphemeralOpts),

    /// Connect to a running ephemeral VM via SSH
    #[clap(name = "ssh")]
    Ssh {
        /// VM name
        name: String,
    },

    /// List ephemeral VM containers
    #[clap(name = "ps")]
    Ps {
        /// Output as JSON
        #[clap(long)]
        json: bool,
    },

    /// Remove all ephemeral VM containers
    #[clap(name = "rm-all")]
    RmAll {
        /// Force removal without confirmation
        #[clap(short, long)]
        force: bool,
    },
}

impl EphemeralCommands {
    pub fn run(self) -> Result<()> {
        match self {
            EphemeralCommands::Run(opts) => run_ephemeral_macos::run(opts),
            EphemeralCommands::RunSsh(opts) => run_ephemeral_macos::run(opts),
            EphemeralCommands::Ssh { name } => cmd_ssh(&name),
            EphemeralCommands::Ps { json } => cmd_ps(json),
            EphemeralCommands::RmAll { force } => cmd_rm_all(force),
        }
    }
}

fn cmd_ps(json: bool) -> Result<()> {
    let vms = EphemeralVmMetadata::list_all()?;
    let live: Vec<_> = vms.into_iter().filter(|vm| vm.is_alive()).collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&live)?);
        return Ok(());
    }

    if live.is_empty() {
        println!("No running ephemeral VMs.");
        return Ok(());
    }

    println!("{:<24} {:<50} {}", "NAME", "IMAGE", "SSH");
    for vm in &live {
        println!("{:<24} {:<50} ssh -p {} -i {} root@localhost",
            vm.name, vm.image, vm.ssh_port, vm.ssh_key);
    }
    Ok(())
}

fn cmd_rm_all(force: bool) -> Result<()> {
    let vms = EphemeralVmMetadata::list_all()?;
    if vms.is_empty() {
        println!("No ephemeral VMs found.");
        return Ok(());
    }

    if !force {
        println!("Found {} ephemeral VM(s):", vms.len());
        for vm in &vms {
            println!("  {} ({})", vm.name, if vm.is_alive() { "running" } else { "stopped" });
        }
        print!("Remove all ephemeral VMs? [y/N]: ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    for vm in &vms {
        if vm.is_alive() {
            let _ = Command::new("kill")
                .args([&vm.pid.to_string()])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
            if vm.gvproxy_pid > 0 {
                let _ = Command::new("kill")
                    .args([&vm.gvproxy_pid.to_string()])
                    .stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
        EphemeralVmMetadata::remove(&vm.name);
        println!("Removed {}", vm.name);
    }
    Ok(())
}

fn cmd_ssh(name: &str) -> Result<()> {
    let vm = EphemeralVmMetadata::load(name)?;
    if !vm.is_alive() {
        EphemeralVmMetadata::remove(name);
        bail!("VM '{}' is not running", name);
    }
    let key_path = std::path::Path::new(&vm.ssh_key);
    run_ephemeral_macos::run_ssh_interactive(vm.ssh_port, key_path, "root")?;
    Ok(())
}
