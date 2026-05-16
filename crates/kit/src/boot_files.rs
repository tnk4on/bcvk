//! Extract boot files from bootc container image via podman run.
//!
//! Uses the already-mounted container image overlay (MERGED path from
//! podman image mount) to read files directly via podman machine ssh,
//! avoiding expensive container creation for each file.

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
const PASSWORD_HASH: &str =
    "$6$bcvksalt$2g2axTGKGM92b6AvQiSXWoYYU3x6nqdhaMJWfCO6iKn0.fTA6DI5sXk.G86OYvNgXXbrYByeMOIMyLcUUA8/1.";

/// Extract boot files using the already-mounted image overlay.
/// `merged_path` is from `podman image mount` (inside podman machine).
#[cfg(target_os = "windows")]
pub fn extract_boot_files(merged_path: &str, ssh_pubkey: &str) -> Result<BootFiles> {
    info!("extracting boot files from {}", merged_path);

    // Read kernel, grub_efi, kver in one SSH call
    let all_paths = podman_machine_cmd(&format!(
        "KVER=$(ls {merged}/usr/lib/modules/ | head -1); \
         echo KVER=$KVER; \
         GRUB=$(find {merged}/usr/lib -name 'grubx64.efi' -o -name 'grubaa64.efi' 2>/dev/null | head -1); \
         echo GRUB=$GRUB",
        merged = merged_path,
    ))?;
    let all_paths_str = String::from_utf8_lossy(&all_paths);

    let kver = all_paths_str.lines()
        .find(|l| l.starts_with("KVER="))
        .map(|l| l.trim_start_matches("KVER=").trim().to_string())
        .unwrap_or_default();
    let grub_path = all_paths_str.lines()
        .find(|l| l.starts_with("GRUB="))
        .map(|l| l.trim_start_matches("GRUB=").trim().to_string())
        .unwrap_or_default();

    if kver.is_empty() {
        bail!("kernel version not found in {}", merged_path);
    }
    info!("kernel version: {}", kver);

    let kernel = podman_machine_cmd(&format!(
        "cat {}/usr/lib/modules/{}/vmlinuz", merged_path, kver
    ))?;
    info!("kernel: {} bytes", kernel.len());

    let grub_efi = if grub_path.is_empty() {
        bail!("GRUB EFI not found in {}", merged_path);
    } else {
        podman_machine_cmd(&format!("cat {}", grub_path))?
    };
    info!("GRUB EFI: {} bytes", grub_efi.len());

    // Check initramfs cache
    let cache_path = format!("/tmp/bcvk-initramfs-{}", kver.replace('.', "_"));
    let base_initramfs = if check_cache(&cache_path)? {
        info!("initramfs cache hit: {}", cache_path);
        read_from_podman_machine(&cache_path)?
    } else {
        info!("initramfs cache miss, building...");
        let base = build_base_initramfs(merged_path, &kver)?;
        write_to_podman_machine(&base, &cache_path)?;
        info!("initramfs cached: {} ({} bytes)", cache_path, base.len());
        base
    };
    info!("base initramfs: {} bytes", base_initramfs.len());

    // Append overlay CPIO
    let overlay_cpio = crate::cpio::create_windows_overlay_cpio()?;
    let mut initramfs = base_initramfs;
    append_cpio(&mut initramfs, &overlay_cpio);

    // Append SSH CPIO
    if !ssh_pubkey.is_empty() {
        let ssh_cpio = crate::cpio::create_windows_ssh_cpio(ssh_pubkey.trim(), PASSWORD_HASH)?;
        append_cpio(&mut initramfs, &ssh_cpio);
    }
    info!("final initramfs: {} bytes", initramfs.len());

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

/// Build base initramfs: extract container's existing initramfs + patch nbd.ko.
/// Follows Linux/macOS pattern — no dracut, just CPIO append.
#[cfg(target_os = "windows")]
fn build_base_initramfs(merged_path: &str, kver: &str) -> Result<Vec<u8>> {
    // 1. Extract container's existing initramfs
    let initramfs_path = format!("{}/usr/lib/modules/{}/initramfs.img", merged_path, kver);
    info!("extracting container initramfs...");
    let mut base = podman_machine_cmd(&format!("cat {}", initramfs_path))?;
    if base.is_empty() {
        bail!("container initramfs not found: {}", initramfs_path);
    }
    info!("container initramfs: {} bytes", base.len());

    // 2. Extract extra kernel modules needed but possibly missing from initramfs
    info!("extracting kernel modules...");
    let extra_modules = extract_kernel_modules(merged_path, kver, &[
        "net/vmw_vsock/vsock.ko*",
        "net/vmw_vsock/vmw_vsock_virtio_transport_common.ko*",
        "drivers/hv/hv_sock.ko*",
        "fs/overlayfs/overlay.ko*",
        "fs/erofs/erofs.ko*",
    ])?;

    // 3. Build patched nbd.ko via podman run (only needed for AF_VSOCK support)
    info!("building patched nbd.ko...");
    let nbd_ko = build_patched_nbd_ko(kver)?;
    info!("patched nbd.ko: {} bytes", nbd_ko.len());

    // 4. Append nbd-vsock binary + patched nbd.ko + extra modules + setup-nbd as CPIO
    let nbd_cpio = create_nbd_cpio(kver, &nbd_ko, &extra_modules)?;
    append_cpio(&mut base, &nbd_cpio);

    Ok(base)
}

/// Build patched nbd.ko module with AF_VSOCK support.
#[cfg(target_os = "windows")]
fn build_patched_nbd_ko(kver: &str) -> Result<Vec<u8>> {
    let script = format!(
        "dnf install -y gcc make >/dev/null 2>&1; \
         KVER={kver}; \
         dnf install -y kernel-devel-$KVER >/dev/null 2>&1; \
         KVER_SHORT=$(echo $KVER | sed 's/-.*//'); \
         KVER_MAJOR=${{KVER_SHORT%%.*}}; \
         mkdir -p /tmp/nbd-patch && cd /tmp/nbd-patch && \
         curl -sfL https://cdn.kernel.org/pub/linux/kernel/v${{KVER_MAJOR}}.x/linux-${{KVER_SHORT}}.tar.xz \
           | tar xJ --strip-components=3 linux-${{KVER_SHORT}}/drivers/block/nbd.c && \
         sed -i '/!sk_is_tcp(sock->sk) &&/{{N;s/!sk_is_stream_unix(sock->sk))/!sk_is_stream_unix(sock->sk) \\&\\& sock->sk->sk_family != AF_VSOCK)/}}' nbd.c && \
         echo 'obj-m += nbd.o' > Makefile && \
         make -C /lib/modules/$KVER/build M=$(pwd) modules >/dev/null 2>&1 && \
         cat nbd.ko",
        kver = kver,
    );

    let output = Command::new("podman")
        .args([
            "run", "--rm", "--privileged",
            "quay.io/fedora/fedora-bootc:latest",
            "bash", "-c", &script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if output.stdout.is_empty() {
        bail!("patched nbd.ko build failed");
    }
    Ok(output.stdout)
}

/// Extract kernel modules from container image via podman machine ssh.
#[cfg(target_os = "windows")]
fn extract_kernel_modules(
    merged_path: &str,
    kver: &str,
    patterns: &[&str],
) -> Result<Vec<(String, Vec<u8>)>> {
    let mut modules = Vec::new();
    let base = format!("{}/usr/lib/modules/{}/kernel", merged_path, kver);
    for pattern in patterns {
        let find_cmd = format!(
            "find {} -path '*{}' 2>/dev/null | head -1",
            base, pattern.replace('*', "")
        );
        let path_out = podman_machine_cmd(&find_cmd)?;
        let path = String::from_utf8_lossy(&path_out).trim().to_string();
        if path.is_empty() {
            debug!("kernel module not found: {}", pattern);
            continue;
        }
        let data = podman_machine_cmd(&format!("cat {}", path))?;
        let rel_path = path.trim_start_matches(merged_path).trim_start_matches('/');
        info!("  module: {} ({} bytes)", rel_path, data.len());
        modules.push((rel_path.to_string(), data));
    }
    Ok(modules)
}

/// Create CPIO archive with nbd-vsock, patched nbd.ko, extra modules, and setup-nbd.
#[cfg(target_os = "windows")]
fn create_nbd_cpio(kver: &str, nbd_ko: &[u8], extra_modules: &[(String, Vec<u8>)]) -> Result<Vec<u8>> {
    use cpio::newc::Builder as NewcBuilder;
    use cpio::newc::ModeFileType;
    use std::io::Write;

    let mut buf = Vec::new();

    // Directories
    let dirs = [
        "usr", "usr/bin", "usr/lib", "usr/lib/modules",
        &format!("usr/lib/modules/{}", kver),
        &format!("usr/lib/modules/{}/kernel", kver),
        &format!("usr/lib/modules/{}/kernel/drivers", kver),
        &format!("usr/lib/modules/{}/kernel/drivers/block", kver),
        "var", "var/lib", "var/lib/dracut", "var/lib/dracut/hooks",
        "var/lib/dracut/hooks/pre-udev",
    ];
    for dir in &dirs {
        let builder = NewcBuilder::new(dir)
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Directory);
        builder.write(&mut buf, 0).finish()?;
    }

    // nbd-vsock binary
    let builder = NewcBuilder::new("usr/bin/nbd-vsock")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = builder.write(&mut buf, NBD_VSOCK_BIN.len() as u32);
    w.write_all(NBD_VSOCK_BIN)?;
    w.finish()?;

    // Patched nbd.ko
    let nbd_ko_path = format!("usr/lib/modules/{}/kernel/drivers/block/nbd.ko", kver);
    let builder = NewcBuilder::new(&nbd_ko_path)
        .mode(0o644)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = builder.write(&mut buf, nbd_ko.len() as u32);
    w.write_all(nbd_ko)?;
    w.finish()?;

    // Extra kernel modules (vsock, hv_sock, overlay, erofs)
    for (path, data) in extra_modules {
        let builder = NewcBuilder::new(path)
            .mode(0o644)
            .set_mode_file_type(ModeFileType::Regular);
        let mut w = builder.write(&mut buf, data.len() as u32);
        w.write_all(data)?;
        w.finish()?;
    }

    // setup-nbd.sh script — use insmod as fallback if modprobe fails
    let setup_nbd = format!(
        "#!/bin/bash\n\
modprobe vsock 2>/dev/null || insmod /usr/lib/modules/{kver}/kernel/net/vmw_vsock/vsock.ko.xz 2>/dev/null\n\
modprobe hv_sock 2>/dev/null || insmod /usr/lib/modules/{kver}/kernel/drivers/hv/hv_sock.ko.xz 2>/dev/null\n\
modprobe nbd max_part=16 2>/dev/null || insmod /usr/lib/modules/{kver}/kernel/drivers/block/nbd.ko max_part=16 2>/dev/null\n\
modprobe overlay 2>/dev/null\n\
modprobe erofs 2>/dev/null\n\
sleep 1\n\
/usr/bin/nbd-vsock /dev/nbd0 2 10800 2>/dev/kmsg\n\
sleep 1\n\
blockdev --rereadpt /dev/nbd0 2>/dev/null\n",
        kver = kver
    );
    let setup_nbd = setup_nbd.as_bytes();

    // usr/lib/bcvk directory
    let dirs2 = ["usr/lib/bcvk"];
    for dir in &dirs2 {
        let builder = NewcBuilder::new(dir)
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Directory);
        builder.write(&mut buf, 0).finish()?;
    }

    let builder = NewcBuilder::new("usr/lib/bcvk/setup-nbd.sh")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = builder.write(&mut buf, setup_nbd.len() as u32);
    w.write_all(setup_nbd)?;
    w.finish()?;

    // systemd service unit — runs early in initrd before sysroot.mount
    let service =
        "[Unit]\n\
         Description=Setup NBD vsock connection\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=sysroot.mount initrd-root-device.target\n\
         After=systemd-modules-load.service systemd-udevd.service\n\
         Wants=systemd-modules-load.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         TimeoutStartSec=60\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-nbd.sh\n";

    let unit_dirs = [
        "usr/lib/systemd", "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
    ];
    for dir in &unit_dirs {
        let builder = NewcBuilder::new(dir)
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Directory);
        builder.write(&mut buf, 0).finish()?;
    }

    let builder = NewcBuilder::new("usr/lib/systemd/system/bcvk-setup-nbd.service")
        .mode(0o644)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = builder.write(&mut buf, service.len() as u32);
    w.write_all(service.as_bytes())?;
    w.finish()?;

    // Pull into initrd-root-device.target so it runs before root mount
    let dropin = b"[Unit]\nWants=bcvk-setup-nbd.service\n";
    let builder = NewcBuilder::new("usr/lib/systemd/system/initrd-root-device.target.d/bcvk-setup-nbd.conf")
        .mode(0o644)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = builder.write(&mut buf, dropin.len() as u32);
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

/// Run command on podman machine and return stdout.
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

#[cfg(target_os = "windows")]
fn copy_to_podman_machine(data: &[u8], remote_path: &str, executable: bool) -> Result<()> {
    let chmod = if executable {
        format!(" && chmod +x {}", remote_path)
    } else {
        String::new()
    };
    let cmd = format!("cat > {}{}", remote_path, chmod);
    let mut child = Command::new("podman")
        .args(["machine", "ssh", &cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin.write_all(data)?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("failed to copy to podman machine: {}", remote_path);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn check_cache(path: &str) -> Result<bool> {
    let output = Command::new("podman")
        .args(["machine", "ssh", &format!("test -f {} && echo HIT", path)])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).contains("HIT"))
}

#[cfg(target_os = "windows")]
fn read_from_podman_machine(path: &str) -> Result<Vec<u8>> {
    let output = Command::new("podman")
        .args(["machine", "ssh", &format!("cat {}", path)])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!("failed to read from podman machine: {}", path);
    }
    Ok(output.stdout)
}

#[cfg(target_os = "windows")]
fn write_to_podman_machine(data: &[u8], path: &str) -> Result<()> {
    copy_to_podman_machine(data, path, false)
}
