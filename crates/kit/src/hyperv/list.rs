//! hyperv list — List persistent VMs.

use clap::Parser;
use color_eyre::Result;

use super::inspect::OutputFormat;
use super::vm;
use super::VmMetadata;

/// Options for `vm list`.
#[derive(Parser, Debug)]
pub struct HypervListOpts {
    /// VM name to query (returns only this VM)
    pub domain_name: Option<String>,

    /// Output format
    #[clap(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,

    /// Show all VMs including stopped ones
    #[clap(long, short = 'a')]
    pub all: bool,

    /// Filter VMs by label
    #[clap(long)]
    pub label: Option<String>,
}

pub fn run(opts: HypervListOpts) -> Result<()> {
    let all_vms = if let Some(ref name) = opts.domain_name {
        match VmMetadata::load(name) {
            Ok(meta) => vec![meta],
            Err(e) => {
                return Err(color_eyre::eyre::eyre!(
                    "Failed to get VM '{}': {}",
                    name,
                    e
                ));
            }
        }
    } else {
        VmMetadata::list_all()?
    };

    let mut vms: Vec<(VmMetadata, String)> = all_vms
        .into_iter()
        .map(|vm_meta| {
            let state = vm::get_vm_state(&vm_meta.vm_name)
                .unwrap_or_else(|_| "unknown".to_string())
                .to_lowercase();
            (vm_meta, state)
        })
        .filter(|(_, state)| opts.all || opts.domain_name.is_some() || state == "running")
        .collect();

    // Filter by label if specified
    if let Some(ref filter_label) = opts.label {
        vms.retain(|(vm_meta, _)| vm_meta.labels.contains(filter_label));
    }

    match opts.format {
        OutputFormat::Table => {
            if vms.is_empty() {
                if opts.all {
                    println!("No VMs found");
                    println!("Tip: Create VMs with 'bcvk vm run <image>'");
                } else {
                    println!("No running VMs found");
                    println!("Use --all to see stopped VMs or 'bcvk vm run <image>' to create one");
                }
                return Ok(());
            }

            println!("{:<20} {:<10} {:<40} SSH", "NAME", "STATE", "IMAGE");
            for (vm_meta, state) in &vms {
                let image = if vm_meta.image.len() > 38 {
                    format!("{}...", &vm_meta.image[..35])
                } else if vm_meta.image.is_empty() {
                    "<none>".to_string()
                } else {
                    vm_meta.image.clone()
                };
                println!(
                    "{:<20} {:<10} {:<40} :{}",
                    vm_meta.name, state, image, vm_meta.ssh_port
                );
            }
        }
        OutputFormat::Json => {
            if opts.domain_name.is_some() && !vms.is_empty() {
                println!("{}", serde_json::to_string_pretty(&vms[0].0)?);
            } else {
                let enriched: Vec<&VmMetadata> = vms.iter().map(|(m, _)| m).collect();
                println!("{}", serde_json::to_string_pretty(&enriched)?);
            }
        }
        OutputFormat::Yaml => {
            return Err(color_eyre::eyre::eyre!(
                "YAML format is not supported for list command"
            ));
        }
        OutputFormat::Xml => {
            return Err(color_eyre::eyre::eyre!(
                "XML format is not supported for list command"
            ));
        }
    }
    Ok(())
}
