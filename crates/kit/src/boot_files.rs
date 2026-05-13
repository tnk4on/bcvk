//! Extract boot files from bootc container image via podman run.
//!
//! All files are captured to memory via stdout pipe — no disk writes.

#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::process::{Command, Stdio};
#[cfg(target_os = "windows")]
use tracing::{debug, info};

#[cfg(target_os = "windows")]
use crate::pxe_server::BootFiles;

#[cfg(target_os = "windows")]
pub fn extract_boot_files(image: &str, nbd_host: &str, nbd_port: u16) -> Result<BootFiles> {
    info!("extracting boot files from {} (memory only)", image);

    let kernel = podman_cat(image, "/usr/lib/modules/*/vmlinuz")?;
    info!("kernel: {} bytes", kernel.len());

    let grub_efi = podman_run_stdout(
        image,
        "find /usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' | head -1 | xargs cat",
    )?;
    info!("GRUB EFI: {} bytes", grub_efi.len());

    let initramfs = podman_run_stdout(
        image,
        "dnf install -y nbd >/dev/null 2>&1; \
         KVER=$(ls /usr/lib/modules/ | head -1); \
         dracut --force --no-hostonly --add nbd --add network --add base \
         --kver $KVER /tmp/initramfs.img 2>/dev/null; \
         cat /tmp/initramfs.img",
    )?;
    info!("initramfs (nbd): {} bytes", initramfs.len());

    let grub_cfg = format!(
        "set timeout=0\nset default=0\nmenuentry bcvk {{\n  \
         linux /boot/vmlinuz root=nbd:{host}:{port}:erofs:ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 ip=dhcp \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}}",
        host = nbd_host,
        port = nbd_port,
    );
    debug!("grub.cfg:\n{}", grub_cfg);

    if kernel.is_empty() || grub_efi.is_empty() || initramfs.is_empty() {
        bail!("failed to extract one or more boot files");
    }

    Ok(BootFiles {
        grub_efi,
        kernel,
        initramfs,
        grub_cfg,
    })
}

#[cfg(target_os = "windows")]
fn podman_cat(image: &str, glob_path: &str) -> Result<Vec<u8>> {
    let output = Command::new("podman")
        .args(["run", "--rm", image, "bash", "-c", &format!("cat {}", glob_path)])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("podman run cat {} failed", glob_path);
    }
    Ok(output.stdout)
}

#[cfg(target_os = "windows")]
fn podman_run_stdout(image: &str, script: &str) -> Result<Vec<u8>> {
    let output = Command::new("podman")
        .args(["run", "--rm", "--privileged", image, "bash", "-c", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("podman run script failed");
    }
    Ok(output.stdout)
}
