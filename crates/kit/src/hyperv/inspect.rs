//! hyperv inspect — Show detailed information about a persistent VM.

use clap::Parser;
use color_eyre::Result;

use super::vm;
use super::VmMetadata;

/// Output format options for inspect command.
#[derive(Debug, Clone, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum OutputFormat {
    Table,
    Json,
    Yaml,
    Xml,
}

/// Options for `vm inspect`.
#[derive(Parser, Debug)]
pub struct HypervInspectOpts {
    /// VM name
    pub name: String,

    /// Output format
    #[clap(long, value_enum, default_value_t = OutputFormat::Yaml)]
    pub format: OutputFormat,
}

pub fn run(opts: HypervInspectOpts) -> Result<()> {
    let meta = VmMetadata::load(&opts.name)?;
    let state = vm::get_vm_state(&meta.vm_name)
        .unwrap_or_else(|_| "unknown".to_string())
        .to_lowercase();

    match opts.format {
        OutputFormat::Yaml => {
            println!("name: {}", meta.name);
            if !meta.image.is_empty() {
                println!("image: {}", meta.image);
            }
            println!("status: {}", state);
            println!("memory_mb: {}", meta.memory_mb);
            println!("vcpus: {}", meta.vcpus);
            println!("disk_path: {}", meta.vhdx_path);
            println!("ssh_port: {}", meta.ssh_port);
            println!("ssh_key: {}", meta.ssh_key);
            if !meta.switch_name.is_empty() {
                println!("switch_name: {}", meta.switch_name);
            }
            println!("created: {}", meta.created);
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&meta)?);
        }
        OutputFormat::Xml => {
            return Err(color_eyre::eyre::eyre!(
                "Xml format is not supported for Hyper-V inspect"
            ));
        }
        OutputFormat::Table => {
            return Err(color_eyre::eyre::eyre!(
                "Table format is not supported for inspect command"
            ));
        }
    }
    Ok(())
}
