//! Windows ephemeral VM commands (Hyper-V + PXE backend).

#[cfg(target_os = "windows")]
use clap::Subcommand;
#[cfg(target_os = "windows")]
use color_eyre::Result;

#[cfg(target_os = "windows")]
use crate::run_ephemeral_windows::{self, EphemeralVmMetadata, RunEphemeralOpts};

#[cfg(target_os = "windows")]
#[derive(clap::Parser, Debug)]
pub struct RunSshOpts {
    /// Container image to boot
    pub image: String,
    /// Remaining args passed to SSH
    #[clap(last = true)]
    pub args: Vec<String>,
}

#[cfg(target_os = "windows")]
#[derive(Debug, Subcommand)]
pub enum EphemeralCommands {
    /// Run bootc containers as ephemeral Hyper-V VMs
    Run(RunEphemeralOpts),
    /// Run ephemeral VM and SSH into it
    RunSsh(RunSshOpts),
    /// Connect to a running ephemeral VM via SSH
    Ssh {
        /// VM name
        name: String,
    },
    /// List ephemeral VMs
    Ps,
    /// Remove all ephemeral VMs
    RmAll,
}

#[cfg(target_os = "windows")]
impl EphemeralCommands {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Run(opts) => run_ephemeral_windows::run(opts),
            Self::RunSsh(opts) => {
                let mut run_opts = RunEphemeralOpts {
                    image: opts.image,
                    vcpus: None,
                    memory: "4G".to_string(),
                    ssh_keygen: true,
                    execute: opts.args,
                    name: None,
                    kernel_args: Vec::new(),
                    debug: false,
                };
                if run_opts.execute.is_empty() {
                    run_opts.execute = vec![];
                    run_opts.ssh_keygen = true;
                }
                run_ephemeral_windows::run(run_opts)
            }
            Self::Ssh { name } => {
                let meta = EphemeralVmMetadata::load(&name)?;
                let key_path = std::path::Path::new(&meta.ssh_key);
                crate::run_ephemeral_windows::run_ssh_interactive(meta.ssh_port, key_path, "root").map(|_| ())
            }
            Self::Ps => {
                let vms = EphemeralVmMetadata::list_all()?;
                if vms.is_empty() {
                    println!("No ephemeral VMs running.");
                } else {
                    println!("{:<20} {:<50} {:<8} {}", "NAME", "IMAGE", "SSH", "CREATED");
                    for vm in &vms {
                        println!("{:<20} {:<50} {:<8} {}",
                            vm.name, vm.image, vm.ssh_port, vm.created);
                    }
                }
                Ok(())
            }
            Self::RmAll => {
                let vms = EphemeralVmMetadata::list_all()?;
                for vm in &vms {
                    println!("removing {}...", vm.name);
                    let _ = crate::hyperv::remove_vm(&vm.vm_name);
                    if let Some(ref nbd) = vm.nbd_container {
                        let _ = std::process::Command::new("podman").args(["rm", "-f", nbd]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
                    }
                    EphemeralVmMetadata::remove(&vm.name);
                }
                println!("removed {} VMs", vms.len());
                Ok(())
            }
        }
    }
}
