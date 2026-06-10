//! Native mode ephemeral VM launch for Windows.
//!
//! Uses wslc COM API for image management and direct VHDX attachment
//! to Hyper-V — no podman machine, no NBD, no vsock relay.

use color_eyre::eyre::bail;
use color_eyre::Result;
use std::path::PathBuf;
use tracing::info;

use crate::hyperv::{boot_files, boot_files_native, dhcp, rootfs_native, ssh_forward::SshForward, vm};
use crate::run_ephemeral_windows::RunEphemeralOpts;
use crate::vm_helpers::{
    default_vcpus, parse_memory_to_mb, run_ssh_command, run_ssh_interactive, wait_for_ssh,
};
use crate::wslc_com;

fn subnet_from_name(name: &str) -> u8 {
    let mut hash: u32 = 5381;
    for b in name.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u32);
    }
    ((hash % 127) + 1) as u8
}

pub fn run(opts: RunEphemeralOpts) -> Result<()> {
    if opts.gui && opts.detach {
        bail!("--gui and --detach cannot be used together");
    }

    let t_start = std::time::Instant::now();
    macro_rules! elapsed {
        ($label:expr) => {
            info!("{}: {:.2}s", $label, t_start.elapsed().as_secs_f64());
        };
    }

    info!(image = %opts.image, "starting ephemeral VM (native mode, wslc COM)");

    // ── 1. wslc COM: pull + inspect ───────────────────────────────
    let session = wslc_com::open_default_session()?;
    session.pull_image(&opts.image)?;
    let digest_short = session.inspect_image_digest(&opts.image)?;
    elapsed!("image ready");

    // ── 2. Compute names and paths ────────────────────────────────
    let vm_name_suffix = opts
        .name
        .clone()
        .unwrap_or_else(|| digest_short[..8].to_string());
    let name = format!("native-{vm_name_suffix}");
    let vm_name = format!("bcvk-{name}");

    let cache_base = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bcvk")
        .join("cache");

    let boot_cache = cache_base.join(format!("boot-{digest_short}"));

    let vcpus = opts
        .itype
        .map(|t| t.vcpus())
        .or(opts.vcpus)
        .unwrap_or_else(default_vcpus);
    let memory_mb = opts
        .itype
        .map(|t| t.memory_mb())
        .map(Ok)
        .unwrap_or_else(|| parse_memory_to_mb(&opts.memory))?;

    // ── 3. rootfs VHDX (cached) ───────────────────────────────────
    let rootfs_vhdx = rootfs_native::create_rootfs_vhdx(
        &session,
        &opts.image,
        &digest_short,
        &cache_base,
    )?;
    elapsed!("rootfs VHDX ready");

    // ── 4. Boot files (cached) ────────────────────────────────────
    let boot_assets =
        boot_files_native::fetch_boot_files_native(&rootfs_vhdx, &boot_cache)?;
    elapsed!("boot files ready");

    // ── 5. SSH keypair ────────────────────────────────────────────
    let ssh_key_path = cache_base.join(format!("{name}-key"));
    let need_ssh = opts.ssh_keygen || !opts.execute.is_empty();
    let ssh_pubkey = if need_ssh {
        crate::vm_helpers::generate_ssh_keypair(&ssh_key_path)?
    } else {
        String::new()
    };

    // ── 6. Hyper-V switch + VM ────────────────────────────────────
    let subnet = subnet_from_name(&name);
    let server_ip = format!("10.0.{subnet}.1");
    let switch_name = format!("bcvk-{name}");
    let _switch_info =
        vm::ensure_internal_switch(&switch_name, &server_ip, 24)?;

    vm::create_gen2_vm(&vm_name, memory_mb, vcpus, &switch_name)?;
    elapsed!("Hyper-V VM created");

    // ── 7. ESP VHDX ───────────────────────────────────────────────
    let esp_path = boot_cache.join("esp-native.vhdx");
    boot_files::create_boot_vhdx_native(&boot_assets, &ssh_pubkey, &esp_path)?;
    elapsed!("ESP VHDX ready");

    // ── 8. Attach disks + boot ────────────────────────────────────
    vm::attach_vhdx_at_slot(&vm_name, &esp_path.to_string_lossy(), 0)?;
    vm::attach_vhdx_at_slot(&vm_name, &rootfs_vhdx.to_string_lossy(), 1)?;
    vm::set_boot_order_disk_first(&vm_name);
    vm::start_vm(&vm_name)?;
    elapsed!("VM started");

    // ── 9. Networking (async, tokio) ──────────────────────────────
    let client_ip = format!("10.0.{subnet}.100");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let dhcp =
            dhcp::DhcpServer::new(&server_ip, &client_ip, &switch_name)?;
        let _dhcp_handle = dhcp.start_background();

        let ssh_fwd = SshForward::start(&client_ip).await?;
        let ssh_port = ssh_fwd.port();
        info!("SSH forward: localhost:{ssh_port} → {client_ip}:22");

        // ── 10. Wait for SSH ──────────────────────────────────────
        info!(port = ssh_port, "waiting for SSH...");
        wait_for_ssh(ssh_port, &ssh_key_path, "root")?;
        elapsed!("SSH ready");

        info!(
            "VM ready: ssh -p {} -i {} root@localhost",
            ssh_port,
            ssh_key_path.display()
        );

        // ── 11. Execute or interactive ────────────────────────────
        if !opts.execute.is_empty() {
            for cmd in &opts.execute {
                run_ssh_command(ssh_port, &ssh_key_path, "root", cmd)
                    .map(|_| ())?;
            }
        } else if need_ssh {
            let status =
                run_ssh_interactive(ssh_port, &ssh_key_path, "root")?;
            dhcp.stop();
            std::process::exit(status.code().unwrap_or(1));
        } else {
            info!("VM running. Use: bcvk ephemeral ssh {name}");
        }

        dhcp.stop();
        Ok::<(), color_eyre::Report>(())
    })?;

    // Cleanup
    let _ = vm::stop_vm(&vm_name);
    let _ = vm::remove_vm(&vm_name);
    vm::remove_internal_switch(&switch_name);

    Ok(())
}
