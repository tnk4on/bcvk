//! Windows ephemeral VM commands (Hyper-V backend).

#[cfg(target_os = "windows")]
use clap::Subcommand;
#[cfg(target_os = "windows")]
use color_eyre::Result;
#[cfg(target_os = "windows")]
use std::io::Write;

#[cfg(target_os = "windows")]
use crate::run_ephemeral_windows::{self, EphemeralVmMetadata, RunEphemeralOpts};

#[cfg(target_os = "windows")]
#[derive(clap::Parser, Debug)]
pub struct RunSshOpts {
    #[command(flatten)]
    pub run_opts: RunEphemeralOpts,

    /// SSH command to execute (optional, defaults to interactive shell)
    #[arg(trailing_var_arg = true)]
    pub ssh_args: Vec<String>,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Subcommand)]
pub enum EphemeralCommands {
    /// Run bootc containers as ephemeral Hyper-V VMs
    #[clap(name = "run")]
    Run(RunEphemeralOpts),

    /// Run ephemeral VM and SSH into it
    #[clap(name = "run-ssh")]
    RunSsh(RunSshOpts),

    /// Connect to a running ephemeral VM via SSH
    #[clap(name = "ssh")]
    Ssh {
        /// VM name
        name: String,

        /// Additional SSH arguments
        #[clap(allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// List ephemeral VMs
    #[clap(name = "ps")]
    Ps {
        /// Output as JSON
        #[clap(long)]
        json: bool,
    },

    /// Remove all ephemeral VMs
    #[clap(name = "rm-all")]
    RmAll {
        /// Force removal without confirmation
        #[clap(short, long)]
        force: bool,
    },
}

#[cfg(target_os = "windows")]
impl EphemeralCommands {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Run(opts) => run_ephemeral_windows::run(opts),
            Self::RunSsh(mut opts) => {
                opts.run_opts.ssh_keygen = true;
                if !opts.ssh_args.is_empty() {
                    let combined = shlex::try_join(opts.ssh_args.iter().map(|s| s.as_str()))
                        .map_err(|e| color_eyre::eyre::eyre!("failed to escape SSH args: {}", e))?;
                    opts.run_opts.execute.push(combined);
                }
                run_ephemeral_windows::run(opts.run_opts)
            }
            Self::Ssh { name, args } => cmd_ssh(&name, &args),
            Self::Ps { json } => cmd_ps(json),
            Self::RmAll { force } => cmd_rm_all(force),
        }
    }
}

#[cfg(target_os = "windows")]
fn cmd_ssh(name: &str, args: &[String]) -> Result<()> {
    let vm = EphemeralVmMetadata::load(name)?;

    let key_path = std::path::Path::new(&vm.ssh_key);
    if args.is_empty() {
        run_ephemeral_windows::run_ssh_interactive(vm.ssh_port, key_path, "root")?;
    } else {
        let combined = shlex::try_join(args.iter().map(|s| s.as_str()))
            .map_err(|e| color_eyre::eyre::eyre!("failed to escape SSH command: {}", e))?;
        let status =
            run_ephemeral_windows::run_ssh_command(vm.ssh_port, key_path, "root", &combined)?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn cmd_ps(json: bool) -> Result<()> {
    let vms = EphemeralVmMetadata::list_all()?;

    // Check VM state and remove dead entries
    let live: Vec<_> = vms
        .into_iter()
        .filter(|vm| match crate::hyperv::get_vm_state(&vm.vm_name) {
            Ok(state) if state.contains("Running") => true,
            _ => {
                EphemeralVmMetadata::remove(&vm.name);
                false
            }
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&live)?);
        return Ok(());
    }

    if live.is_empty() {
        println!("No running ephemeral VMs.");
        return Ok(());
    }

    println!("{:<24} {:<50} SSH", "NAME", "IMAGE");
    for vm in &live {
        println!(
            "{:<24} {:<50} ssh -p {} -i {} root@localhost",
            vm.name, vm.image, vm.ssh_port, vm.ssh_key
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn cmd_rm_all(force: bool) -> Result<()> {
    let vms = EphemeralVmMetadata::list_all()?;
    if vms.is_empty() {
        println!("No ephemeral VMs found.");
        return Ok(());
    }

    if !force {
        println!("Found {} ephemeral VM(s):", vms.len());
        for vm in &vms {
            let state =
                crate::hyperv::get_vm_state(&vm.vm_name).unwrap_or_else(|_| "unknown".to_string());
            println!("  {} ({})", vm.name, state.to_lowercase());
        }
        print!("Remove all ephemeral VMs? [y/N]: ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    for vm in &vms {
        let _ = crate::hyperv::remove_vm(&vm.vm_name);
        if let Some(ref nbd) = vm.nbd_container {
            let _ = std::process::Command::new("podman")
                .args(["rm", "-f", nbd])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        EphemeralVmMetadata::remove(&vm.name);
        println!("Removed {}", vm.name);
    }

    // Sweep orphaned nbdkit containers
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", "--filter", "name=bcvk-nbd-"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    Ok(())
}
