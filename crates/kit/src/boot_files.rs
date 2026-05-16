//! Extract boot files from bootc container image.
//!
//! Uses the container's existing initramfs and appends CPIO archives
//! for NBD setup, overlay services, and SSH configuration.
//! No dracut, no kernel patch — follows Linux/macOS pattern.

#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};
#[cfg(target_os = "windows")]
use tracing::{debug, info};

#[cfg(target_os = "windows")]
use crate::pxe_server::BootFiles;

#[cfg(target_os = "windows")]
const NBD_TCP_BIN: &[u8] = include_bytes!("nbd-vsock.bin");

#[cfg(target_os = "windows")]
const PASSWORD_HASH: &str =
    "$6$bcvksalt$2g2axTGKGM92b6AvQiSXWoYYU3x6nqdhaMJWfCO6iKn0.fTA6DI5sXk.G86OYvNgXXbrYByeMOIMyLcUUA8/1.";

/// Extract boot files using the already-mounted image overlay.
#[cfg(target_os = "windows")]
pub fn extract_boot_files(
    merged_path: &str,
    ssh_pubkey: &str,
    nbd_host: &str,
    nbd_port: u16,
    client_ip: &str,
    gateway_ip: &str,
) -> Result<BootFiles> {
    info!("extracting boot files from {}", merged_path);

    let meta = podman_machine_cmd(&format!(
        "KVER=$(ls {m}/usr/lib/modules/ | head -1); \
         echo KVER=$KVER; \
         GRUB=$(find {m}/usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' 2>/dev/null | head -1); \
         echo GRUB=$GRUB",
        m = merged_path,
    ))?;
    let meta_str = String::from_utf8_lossy(&meta);

    let kver = meta_str.lines()
        .find(|l| l.starts_with("KVER="))
        .map(|l| l.trim_start_matches("KVER=").trim().to_string())
        .unwrap_or_default();
    let grub_path = meta_str.lines()
        .find(|l| l.starts_with("GRUB="))
        .map(|l| l.trim_start_matches("GRUB=").trim().to_string())
        .unwrap_or_default();

    if kver.is_empty() { bail!("kernel version not found"); }
    info!("kernel version: {}", kver);

    let kernel = podman_machine_cmd(&format!(
        "cat {}/usr/lib/modules/{}/vmlinuz", merged_path, kver
    ))?;
    info!("kernel: {} bytes", kernel.len());

    let grub_efi = if grub_path.is_empty() {
        bail!("GRUB EFI not found");
    } else {
        podman_machine_cmd(&format!("cat {}", grub_path))?
    };
    info!("GRUB EFI: {} bytes", grub_efi.len());

    // Extract container's existing initramfs
    let initramfs_path = format!("{}/usr/lib/modules/{}/initramfs.img", merged_path, kver);
    let mut initramfs = podman_machine_cmd(&format!("cat {}", initramfs_path))?;
    if initramfs.is_empty() { bail!("initramfs not found"); }
    info!("container initramfs: {} bytes", initramfs.len());

    // Append NBD setup CPIO
    let nbd_cpio = create_nbd_tcp_cpio(nbd_host, nbd_port, client_ip, gateway_ip)?;
    append_cpio(&mut initramfs, &nbd_cpio);

    // Append overlay CPIO
    let overlay_cpio = crate::cpio::create_windows_overlay_cpio()?;
    append_cpio(&mut initramfs, &overlay_cpio);

    // Append SSH CPIO
    if !ssh_pubkey.is_empty() {
        let ssh_cpio = crate::cpio::create_windows_ssh_cpio(ssh_pubkey.trim(), PASSWORD_HASH)?;
        append_cpio(&mut initramfs, &ssh_cpio);
    }

    info!("final initramfs: {} bytes", initramfs.len());

    // Method 3: kernel ip= static config (processed by kernel, no userspace needed)
    let grub_cfg = format!(
        "set timeout=0\nset default=0\nmenuentry bcvk {{\n  \
         linux /boot/vmlinuz root=/dev/nbd0p2 rootfstype=erofs ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 \
         rd.neednet=1 \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}}"
    );
    debug!("grub.cfg:\n{}", grub_cfg);

    if kernel.is_empty() || grub_efi.is_empty() || initramfs.is_empty() {
        bail!("failed to extract one or more boot files");
    }

    Ok(BootFiles { grub_efi, kernel, initramfs, grub_cfg })
}

/// Create CPIO with nbd-tcp binary + systemd service.
/// setup-nbd.sh tries: 1) kernel ip= already configured, 2) ip cmd static, 3) udhcpc
#[cfg(target_os = "windows")]
fn create_nbd_tcp_cpio(host: &str, port: u16, client_ip: &str, gateway_ip: &str) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let mut buf = Vec::new();

    let dirs = [
        "usr", "usr/bin", "usr/lib", "usr/lib/bcvk",
        "usr/lib/systemd", "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
        "etc", "etc/NetworkManager", "etc/NetworkManager/system-connections",
    ];
    for dir in &dirs {
        let b = NewcBuilder::new(dir).mode(0o755).set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    // nbd-tcp binary
    let b = NewcBuilder::new("usr/bin/nbd-tcp").mode(0o755).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, NBD_TCP_BIN.len() as u32);
    w.write_all(NBD_TCP_BIN)?;
    w.finish()?;

    // NetworkManager connection profile for static IP
    let nm_conn = format!(
        "[connection]\n\
         id=bcvk-nbd\n\
         type=ethernet\n\
         interface-name=eth0\n\
         autoconnect=true\n\
         \n\
         [ipv4]\n\
         method=manual\n\
         addresses={client}/24\n\
         gateway={gw}\n\
         \n\
         [ipv6]\n\
         method=disabled\n",
        client = client_ip, gw = gateway_ip
    );
    let b = NewcBuilder::new("etc/NetworkManager/system-connections/bcvk-nbd.nmconnection")
        .mode(0o600).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, nm_conn.len() as u32);
    w.write_all(nm_conn.as_bytes())?;
    w.finish()?;

    // setup-nbd.sh — wait for NM to configure network, then connect
    let setup = format!(
        "#!/bin/bash\n\
modprobe nbd max_part=16 2>/dev/null\n\
\n\
# Wait for network (NM should have configured it via rd.neednet=1)\n\
for i in $(seq 1 30); do\n\
  /usr/bin/nbd-tcp /dev/nbd0 {host} {port} 2>/dev/kmsg && break\n\
  sleep 2\n\
done\n\
sleep 1\n\
blockdev --rereadpt /dev/nbd0 2>/dev/null\n",
        host = host, port = port
    );
    let b = NewcBuilder::new("usr/lib/bcvk/setup-nbd.sh").mode(0o755).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, setup.len() as u32);
    w.write_all(setup.as_bytes())?;
    w.finish()?;

    // systemd service — run after udev and modules, script retries until network is up
    let service =
        "[Unit]\n\
         Description=Setup NBD TCP connection\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=sysroot.mount initrd-root-device.target\n\
         After=systemd-udevd.service systemd-modules-load.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         TimeoutStartSec=120\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-nbd.sh\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/bcvk-setup-nbd.service").mode(0o644).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, service.len() as u32);
    w.write_all(service.as_bytes())?;
    w.finish()?;

    let dropin = b"[Unit]\nWants=bcvk-setup-nbd.service\n";
    let b = NewcBuilder::new("usr/lib/systemd/system/initrd-root-device.target.d/bcvk-setup-nbd.conf").mode(0o644).set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, dropin.len() as u32);
    w.write_all(dropin)?;
    w.finish()?;

    Ok(cpio::newc::trailer(buf)?)
}

#[cfg(target_os = "windows")]
fn append_cpio(initramfs: &mut Vec<u8>, cpio: &[u8]) {
    let aligned = (initramfs.len() + 3) & !3;
    initramfs.resize(aligned, 0);
    initramfs.extend_from_slice(cpio);
}

#[cfg(target_os = "windows")]
fn podman_machine_cmd(cmd: &str) -> Result<Vec<u8>> {
    let output = Command::new("podman")
        .args(["machine", "ssh", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("podman machine ssh failed: {}", cmd);
    }
    Ok(output.stdout)
}
