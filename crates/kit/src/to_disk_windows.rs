//! Install bootc images to VHDX disk using Hyper-V ephemeral VMs (Windows).
//!
//! Creates a VHDX disk image with `bootc install to-disk` running inside
//! an ephemeral VM. SSH public key is embedded via `--root-ssh-authorized-keys`.

use std::path::Path;
use std::process::{Command, Stdio};

use camino::Utf8PathBuf;
use clap::Parser;
use color_eyre::{eyre::bail, Result};
use tracing::info;

/// Options for `to-disk` on Windows.
#[derive(Parser, Debug)]
pub struct ToDiskWindowsOpts {
    /// Container image to install
    pub image: String,

    /// Output VHDX path
    pub output: Utf8PathBuf,

    /// Disk size (e.g. "20G", "10240M")
    #[clap(long, default_value = "20G")]
    pub disk_size: String,

    /// Installation options
    #[clap(flatten)]
    pub install: crate::install_options::InstallOptions,

    /// Number of vCPUs for the installation VM
    #[clap(long, default_value = "4")]
    pub cpus: u32,

    /// Memory for the installation VM (e.g. "16G"); needs enough RAM for image pull
    #[clap(long, default_value = "16G")]
    pub memory: String,
}

pub fn run(opts: ToDiskWindowsOpts) -> Result<()> {
    if opts.output.exists() {
        bail!("output file already exists: {}", opts.output);
    }

    let parent = opts.output.parent().map(|p| p.as_std_path()).unwrap_or(Path::new("."));
    let output_abs = std::fs::canonicalize(parent)?
        .join(opts.output.file_name().unwrap())
        .to_string_lossy()
        .to_string()
        .trim_start_matches(r"\\?\")
        .to_string();

    // Phase 1: Generate SSH keypair
    let key_path = format!("{}.key", output_abs);
    let pub_path = format!("{}.key.pub", output_abs);
    info!("generating SSH keypair: {}", key_path);
    let _ = std::fs::remove_file(&key_path);
    let _ = std::fs::remove_file(&pub_path);
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f", &key_path])
        .status()?;
    if !status.success() {
        bail!("ssh-keygen failed");
    }
    let pub_key_content = std::fs::read_to_string(&pub_path)?;
    info!("SSH keypair generated");

    // Phase 2: Create target VHDX
    let size_bytes = parse_size(&opts.disk_size)?;
    info!("creating VHDX: {} ({} bytes)", output_abs, size_bytes);
    let ps_result = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "New-VHD -Path '{}' -SizeBytes {} -Dynamic | Out-Null; Write-Host 'OK'",
                output_abs, size_bytes
            ),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !ps_result.status.success() {
        let stderr = String::from_utf8_lossy(&ps_result.stderr);
        bail!("New-VHD failed: {}", stderr.trim());
    }

    // Phase 3: Launch ephemeral VM (detached)
    let vm_name = format!(
        "todisk-{:08x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32
    );
    let hyper_vm_name = format!("bcvk-ephemeral-{}", vm_name);

    info!("launching ephemeral VM: {}", vm_name);
    let exe = std::env::current_exe()?;
    let launch = Command::new(&exe)
        .args([
            "ephemeral", "run", "-d", "-K",
            "--name", &vm_name,
            "--vcpus", &opts.cpus.to_string(),
            "--memory", &opts.memory,
            &opts.image,
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !launch.success() {
        let _ = std::fs::remove_file(&output_abs);
        bail!("failed to launch ephemeral VM");
    }

    // Phase 4: Wait for SSH, then hot-plug target VHDX
    info!("waiting for ephemeral VM to boot...");
    std::thread::sleep(std::time::Duration::from_secs(60));

    info!("attaching target VHDX to VM at SCSI slot 1...");
    let attach_result = std::thread::spawn({
        let hv = hyper_vm_name.clone();
        let vhdx = output_abs.clone();
        move || crate::hyperv::vm::attach_vhdx_at_slot(&hv, &vhdx, 1)
    })
    .join()
    .map_err(|_| color_eyre::eyre::eyre!("attach thread panicked"))?;

    if let Err(e) = attach_result {
        info!("attach failed: {}, retrying...", e);
        std::thread::sleep(std::time::Duration::from_secs(5));
        std::thread::spawn({
            let hv = hyper_vm_name.clone();
            let vhdx = output_abs.clone();
            move || crate::hyperv::vm::attach_vhdx_at_slot(&hv, &vhdx, 1)
        })
        .join()
        .map_err(|_| color_eyre::eyre::eyre!("attach thread panicked"))??;
    }
    info!("target VHDX attached");

    // Phase 5: Run bootc install via SSH
    let install_script = build_install_script(&opts.image, &pub_key_content, &opts.install);
    info!("running bootc install in ephemeral VM...");
    let install_result = Command::new(&exe)
        .args([
            "ephemeral", "ssh", &vm_name, "--",
            "/bin/bash", "-c", &install_script,
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    // Phase 6: Cleanup
    let _ = Command::new(&exe)
        .args(["ephemeral", "rm-all"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(ref mut stdin) = c.stdin {
                let _ = stdin.write_all(b"y\n");
            }
            c.wait()
        });

    match install_result {
        Ok(status) if status.success() => {
            info!("installation completed: {}", output_abs);
            println!("Disk image created: {}", output_abs);
            println!("SSH key: {}", key_path);
            println!("\nTo run:");
            println!("  bcvk vm run --name myvm {}", output_abs);
            Ok(())
        }
        Ok(status) => {
            let _ = std::fs::remove_file(&output_abs);
            bail!("installation failed (exit code: {:?})", status.code());
        }
        Err(e) => {
            let _ = std::fs::remove_file(&output_abs);
            bail!("failed to run install command: {}", e);
        }
    }
}

fn build_install_script(image: &str, pub_key: &str, install: &crate::install_options::InstallOptions) -> String {
    let pub_key_escaped = pub_key.trim().replace('\'', "'\\''");
    let bootc_args = install.to_bootc_args().join(" ");

    format!(
        r#"set -euo pipefail

echo 'Setting up storage...'
# Mount large tmpfs for container storage (backed by VM RAM)
mount -t tmpfs -o size=12G tmpfs /var/tmp
mkdir -p /var/tmp/containers
rm -rf /var/lib/containers
ln -sr /var/tmp/containers /var/lib/containers

echo 'Writing SSH public key...'
echo '{key}' > /tmp/bcvk-ssh-key.pub

echo 'Starting bootc installation...'
echo 'Pulling image into ephemeral VM (this may take a few minutes)...'
podman run --rm -i --privileged --pid=host \
    -v /dev:/dev -v /var/lib/containers:/var/lib/containers \
    -v /var/tmp:/var/tmp -v /sys:/sys:ro \
    -v /tmp:/tmp:z \
    --security-opt label=type:unconfined_t \
    docker://{image} \
    bootc install to-disk \
    --generic-image \
    --skip-fetch-check \
    --filesystem ext4 \
    --root-ssh-authorized-keys /tmp/bcvk-ssh-key.pub \
    {args} \
    /dev/sdb

echo 'Installation completed successfully!'
"#,
        key = pub_key_escaped,
        image = image,
        args = bootc_args,
    )
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('G').or(s.strip_suffix("GB")) {
        Ok(n.trim().parse::<u64>()? * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M').or(s.strip_suffix("MB")) {
        Ok(n.trim().parse::<u64>()? * 1024 * 1024)
    } else {
        Ok(s.parse::<u64>()?)
    }
}
