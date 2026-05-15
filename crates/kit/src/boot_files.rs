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
const NBD_VSOCK_BIN: &[u8] = include_bytes!("nbd-vsock.bin");

#[cfg(target_os = "windows")]
pub fn extract_boot_files(image: &str) -> Result<BootFiles> {
    info!("extracting boot files from {} (memory only)", image);

    let kernel = podman_cat(image, "/usr/lib/modules/*/vmlinuz")?;
    info!("kernel: {} bytes", kernel.len());

    let grub_efi = podman_run_stdout(
        image,
        "find /usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' | head -1 | xargs cat",
    )?;
    info!("GRUB EFI: {} bytes", grub_efi.len());

    // Write nbd-vsock binary into podman machine via SSH pipe
    let mut scp = Command::new("podman")
        .args(["machine", "ssh", "cat > /tmp/bcvk-nbd-vsock && chmod +x /tmp/bcvk-nbd-vsock"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(mut stdin) = scp.stdin.take() {
        use std::io::Write;
        stdin.write_all(NBD_VSOCK_BIN)?;
    }
    let scp_status = scp.wait()?;
    if !scp_status.success() {
        bail!("failed to copy nbd-vsock to podman machine");
    }
    info!("nbd-vsock binary copied to podman machine ({} bytes)", NBD_VSOCK_BIN.len());

    let script =
        "dnf install -y nbd >/dev/null 2>&1; \
         KVER=$(ls /usr/lib/modules/ | head -1); \
         echo INITRAMFS: kver=$KVER >&2; \
         mkdir -p /usr/lib/dracut/modules.d/99bcvk-vsock && \
         ls -la /tmp/nbd-vsock-host >&2 && \
         cp /tmp/nbd-vsock-host /usr/lib/dracut/modules.d/99bcvk-vsock/nbd-vsock && \
         chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/nbd-vsock && \
         printf '#!/bin/bash\\ncheck() { return 0; }\\ndepends() { return 0; }\\ninstall() {\\n  inst_multiple nbd-client blockdev\\n  inst_simple \"$moddir/nbd-vsock\" /usr/bin/nbd-vsock\\n  inst_hook pre-udev 00 \"$moddir/setup-nbd.sh\"\\n}\\n' > /usr/lib/dracut/modules.d/99bcvk-vsock/module-setup.sh && \
         printf '#!/bin/bash\\nmodprobe hv_sock 2>/dev/null\\nmodprobe nbd max_part=16 2>/dev/null\\nsleep 1\\n/usr/bin/nbd-vsock /dev/nbd0 2 10800\\nsleep 1\\nblockdev --rereadpt /dev/nbd0 2>/dev/null\\nls -la /dev/nbd0* >&2\\n' > /usr/lib/dracut/modules.d/99bcvk-vsock/setup-nbd.sh && \
         chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/*.sh && \
         cat /usr/lib/dracut/modules.d/99bcvk-vsock/module-setup.sh >&2 && \
         echo INITRAMFS: dracut start >&2 && \
         dracut --force --no-hostonly --add 'nbd network base bcvk-vsock' \
         --add-drivers 'hv_sock hv_utils hv_vmbus vsock nbd' \
         --kver $KVER /tmp/initramfs.img; \
         echo INITRAMFS: dracut exit=$? >&2; \
         test -f /tmp/initramfs.img && cat /tmp/initramfs.img";

    let output = Command::new("podman")
        .args([
            "run", "--rm", "--privileged",
            "-v", "/tmp/bcvk-nbd-vsock:/tmp/nbd-vsock-host:ro,z",
            image, "bash", "-c", script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        bail!("podman run initramfs build failed");
    }
    let initramfs = output.stdout;
    info!("initramfs (nbd): {} bytes", initramfs.len());

    let grub_cfg = "set timeout=0\nset default=0\nmenuentry bcvk {\n  \
         linux /boot/vmlinuz root=/dev/nbd0p2 rootfstype=erofs ro \
         console=ttyS0 console=tty0 selinux=0 net.ifnames=0 ip=dhcp \
         systemd.journald.storage=volatile\n  \
         initrd /boot/initramfs.img\n}"
        .to_string();
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
        .stderr(Stdio::inherit())
        .output()?;
    if !output.status.success() {
        bail!("podman run script failed");
    }
    Ok(output.stdout)
}
