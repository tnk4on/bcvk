//! Initramfs CPIO construction and boot file management for macOS vsock backends.
//!
//! Shared infrastructure for vfkit and krun backends:
//! - CPIO archives with ublk-vsock/nbd-vsock binaries, kernel modules, and systemd services
//! - Boot file extraction (vmlinuz, initramfs, kernel modules) from container images
//! - Initramfs construction with CPIO appends
//! - Unix socket bridge for vsock relay

use color_eyre::{
    eyre::{bail, Context},
    Result,
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use cpio::newc::Builder as NewcBuilder;
use cpio::newc::ModeFileType;
use tracing::{debug, info};

const NBD_VSOCK_AARCH64_BIN: &[u8] = include_bytes!("nbd-vsock-aarch64.bin");

/// Create CPIO with block device binaries, kernel modules, and systemd services.
pub(crate) fn create_krun_block_device_cpio(
    vsock_port: u32,
    cache_dir: &Path,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();

    let dirs = [
        "usr",
        "usr/bin",
        "usr/lib",
        "usr/lib/bcvk",
        "usr/lib/systemd",
        "usr/lib/systemd/system",
        "usr/lib/systemd/system/initrd-root-device.target.d",
        "usr/lib/udev",
        "usr/lib/udev/rules.d",
    ];
    for dir in &dirs {
        let b = NewcBuilder::new(dir)
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    add_binaries_to_cpio(&mut buf, cache_dir)?;
    add_kernel_modules_to_cpio(&mut buf, cache_dir)?;
    add_shell_scripts_to_cpio(&mut buf, vsock_port)?;
    add_systemd_services_to_cpio(&mut buf)?;

    Ok(cpio::newc::trailer(buf)?)
}

fn add_binaries_to_cpio(buf: &mut Vec<u8>, cache_dir: &Path) -> Result<()> {
    let b = NewcBuilder::new("usr/bin/nbd-vsock")
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, NBD_VSOCK_AARCH64_BIN.len() as u32);
    w.write_all(NBD_VSOCK_AARCH64_BIN)?;
    w.finish()?;

    let ublk_vsock_path = cache_dir.join("ublk-vsock");
    if ublk_vsock_path.exists() {
        let ublk_data = std::fs::read(&ublk_vsock_path)?;
        let b = NewcBuilder::new("usr/bin/ublk-vsock")
            .mode(0o755)
            .set_mode_file_type(ModeFileType::Regular);
        let mut w = b.write(&mut *buf, ublk_data.len() as u32);
        w.write_all(&ublk_data)?;
        w.finish()?;
        info!(
            "included ublk-vsock ({} bytes) in initramfs",
            ublk_data.len()
        );
    }
    Ok(())
}

fn add_kernel_modules_to_cpio(buf: &mut Vec<u8>, cache_dir: &Path) -> Result<()> {
    for module_name in &[
        "nbd.ko",
        "vsock.ko",
        "vmw_vsock_virtio_transport_common.ko",
        "vmw_vsock_virtio_transport.ko",
        "ublk_drv.ko",
    ] {
        let module_path = cache_dir.join(module_name);
        if module_path.exists() {
            let module_data = std::fs::read(&module_path)?;
            let cpio_path = format!("usr/lib/bcvk/{}", module_name);
            let b = NewcBuilder::new(&cpio_path)
                .mode(0o644)
                .set_mode_file_type(ModeFileType::Regular);
            let mut w = b.write(&mut *buf, module_data.len() as u32);
            w.write_all(&module_data)?;
            w.finish()?;
            info!(
                "included {} ({} bytes) in initramfs",
                module_name,
                module_data.len()
            );
        }
    }
    Ok(())
}

fn add_shell_scripts_to_cpio(buf: &mut Vec<u8>, vsock_port: u32) -> Result<()> {
    let setup_modules = "\
#!/bin/bash\n\
insmod /usr/lib/bcvk/vsock.ko 2>/dev/kmsg\n\
insmod /usr/lib/bcvk/vmw_vsock_virtio_transport_common.ko 2>/dev/kmsg\n\
insmod /usr/lib/bcvk/vmw_vsock_virtio_transport.ko 2>/dev/kmsg\n\
if insmod /usr/lib/bcvk/ublk_drv.ko 2>/dev/null; then\n\
  if [ ! -e /dev/ublk-control ] && [ -f /sys/class/misc/ublk-control/dev ]; then\n\
    DEVNUM=$(cat /sys/class/misc/ublk-control/dev)\n\
    mknod /dev/ublk-control c ${DEVNUM%%:*} ${DEVNUM##*:}\n\
  fi\n\
fi\n";
    write_script(buf, "usr/lib/bcvk/setup-modules.sh", setup_modules)?;

    let block_device_script = format!(
        "\
#!/bin/bash\n\
VSOCK_PORT={vsock_port}\n\
\n\
# Check io_uring availability (RHEL/CentOS disable it by default)\n\
IO_URING_DISABLED=$(cat /proc/sys/kernel/io_uring_disabled 2>/dev/null)\n\
if [ \"$IO_URING_DISABLED\" = \"2\" ] || [ \"$IO_URING_DISABLED\" = \"1\" ]; then\n\
    echo 'bcvk: io_uring disabled (RHEL/CentOS), using NBD' > /dev/kmsg\n\
else\n\
  if [ -e /sys/module/ublk_drv ] && [ -x /usr/bin/ublk-vsock ] && [ -e /dev/ublk-control ]; then\n\
    if /usr/bin/ublk-vsock --test 2>/dev/null; then\n\
        echo 'bcvk: trying ublk block device' > /dev/kmsg\n\
        /usr/bin/ublk-vsock /dev/ublkb0 \"$VSOCK_PORT\" 1 2>/dev/kmsg &\n\
        UBLK_PID=$!\n\
        i=0\n\
        while [ $i -lt 60 ]; do\n\
            for ublk_dev in /dev/ublkb*; do\n\
                if [ -b \"$ublk_dev\" ]; then\n\
                    echo \"bcvk: ublk device ready ($ublk_dev)\" > /dev/kmsg\n\
                    exit 0\n\
                fi\n\
            done\n\
            if ! kill -0 $UBLK_PID 2>/dev/null; then\n\
                echo 'bcvk: ublk-vsock exited, falling back to NBD' > /dev/kmsg\n\
                break\n\
            fi\n\
            sleep 0.5\n\
            i=$((i + 1))\n\
        done\n\
        echo 'bcvk: ublk failed, falling back to NBD' > /dev/kmsg\n\
        kill $UBLK_PID 2>/dev/null\n\
        wait $UBLK_PID 2>/dev/null\n\
    else\n\
        echo 'bcvk: ublk not available (test failed), using NBD' > /dev/kmsg\n\
    fi\n\
  fi\n\
fi\n\
echo 'bcvk: using NBD block device' > /dev/kmsg\n\
insmod /usr/lib/bcvk/nbd.ko max_part=16 2>/dev/kmsg\n\
/usr/bin/nbd-vsock /dev/nbd0 \"$VSOCK_PORT\" 1 2>/dev/kmsg &\n\
NBD_PID=$!\n\
i=0\n\
while [ $i -lt 60 ]; do\n\
    if [ -b /dev/nbd0 ]; then\n\
        echo 'bcvk: NBD device ready' > /dev/kmsg\n\
        exit 0\n\
    fi\n\
    if ! kill -0 $NBD_PID 2>/dev/null; then\n\
        echo 'bcvk: nbd-vsock exited unexpectedly' > /dev/kmsg\n\
        exit 1\n\
    fi\n\
    sleep 0.5\n\
    i=$((i + 1))\n\
done\n\
echo 'bcvk: NBD device timeout' > /dev/kmsg\n\
exit 1\n",
        vsock_port = vsock_port
    );
    write_script(buf, "usr/lib/bcvk/block-device-setup.sh", &block_device_script)?;

    let post_script = "\
#!/bin/bash\n\
set -x\n\
exec 2>/dev/kmsg\n\
sleep 2\n\
DEV=\n\
for f in /sys/block/ublkb*/dev; do\n\
    if [ -f \"$f\" ]; then\n\
        d=${f%/dev}\n\
        DEV=${d##*/}\n\
        break\n\
    fi\n\
done\n\
if [ -z \"$DEV\" ]; then\n\
    DEV=nbd0\n\
fi\n\
echo \"bcvk: post-setup for /dev/$DEV\" > /dev/kmsg\n\
ls -la /dev/${DEV}* 2>&1 > /dev/kmsg || true\n\
ls -la /sys/block/${DEV}/ 2>&1 > /dev/kmsg || true\n\
# Ensure partition device nodes exist\n\
for part in /sys/block/${DEV}/${DEV}p*/partition; do\n\
    if [ -f \"$part\" ]; then\n\
        PARTDIR=${part%/partition}\n\
        PARTNAME=${PARTDIR##*/}\n\
        if [ ! -b /dev/$PARTNAME ]; then\n\
            MAJOR=$(cat /sys/block/${DEV}/dev | cut -d: -f1)\n\
            MINOR=$(cat ${PARTDIR}/dev | cut -d: -f2)\n\
            mknod /dev/$PARTNAME b $MAJOR $MINOR\n\
            echo \"bcvk: created /dev/$PARTNAME ($MAJOR:$MINOR)\" > /dev/kmsg\n\
        fi\n\
    fi\n\
done\n\
echo 65536 > /sys/block/$DEV/queue/read_ahead_kb 2>/dev/null\n\
echo \"bcvk: attempting mount /dev/${DEV}p2\" > /dev/kmsg\n\
mount -t erofs -o ro /dev/${DEV}p2 /sysroot\n\
if [ $? -eq 0 ]; then\n\
    echo \"bcvk: mounted /dev/${DEV}p2 on /sysroot\" > /dev/kmsg\n\
    systemctl start --no-block sysroot.mount 2>/dev/null || true\n\
else\n\
    echo \"bcvk: FAILED to mount /dev/${DEV}p2\" > /dev/kmsg\n\
fi\n\
echo \"bcvk: post-setup done\" > /dev/kmsg\n";
    write_script(buf, "usr/lib/bcvk/block-device-post.sh", post_script)?;

    Ok(())
}

fn add_systemd_services_to_cpio(buf: &mut Vec<u8>) -> Result<()> {
    let svc_modules = "[Unit]\n\
         Description=Load bcvk kernel modules\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=bcvk-block-device.service\n\
         After=systemd-udevd.service systemd-modules-load.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         TimeoutStartSec=30\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-modules.sh\n";
    write_file(buf, "usr/lib/systemd/system/bcvk-setup-modules.service", svc_modules.as_bytes())?;

    let svc_block = "[Unit]\n\
         Description=bcvk block device setup\n\
         DefaultDependencies=no\n\
         ConditionPathExists=/etc/initrd-release\n\
         Before=sysroot.mount initrd-root-device.target\n\
         After=bcvk-setup-modules.service\n\
         Requires=bcvk-setup-modules.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         KillMode=none\n\
         TimeoutStartSec=120\n\
         ExecStart=/usr/bin/bash /usr/lib/bcvk/block-device-setup.sh\n\
         ExecStartPost=/usr/bin/bash /usr/lib/bcvk/block-device-post.sh\n";
    write_file(buf, "usr/lib/systemd/system/bcvk-block-device.service", svc_block.as_bytes())?;

    let dropin = b"[Unit]\nWants=bcvk-block-device.service\n";
    write_file(
        buf,
        "usr/lib/systemd/system/initrd-root-device.target.d/bcvk-setup-nbd.conf",
        dropin,
    )?;

    // udev rule to notify systemd about nbd partitions
    let udev_rule = b"SUBSYSTEM==\"block\", KERNEL==\"nbd[0-9]*\", OPTIONS+=\"db_persist\"\nSUBSYSTEM==\"block\", KERNEL==\"ublkb[0-9]*\", OPTIONS+=\"db_persist\"\n";
    write_file(buf, "usr/lib/udev/rules.d/99-bcvk-nbd.rules", udev_rule)?;

    Ok(())
}

fn write_script(buf: &mut Vec<u8>, path: &str, content: &str) -> Result<()> {
    let b = NewcBuilder::new(path)
        .mode(0o755)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, content.len() as u32);
    w.write_all(content.as_bytes())?;
    w.finish()?;
    Ok(())
}

fn write_file(buf: &mut Vec<u8>, path: &str, content: &[u8]) -> Result<()> {
    let b = NewcBuilder::new(path)
        .mode(0o644)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut *buf, content.len() as u32);
    w.write_all(content)?;
    w.finish()?;
    Ok(())
}

/// Append a CPIO archive to an initramfs with 4-byte alignment padding.
pub(crate) fn append_cpio(initramfs: &mut Vec<u8>, cpio: &[u8]) {
    let aligned = (initramfs.len() + 3) & !3;
    initramfs.resize(aligned, 0);
    initramfs.extend_from_slice(cpio);
}

#[allow(dead_code)]
pub(crate) fn create_macos_ssh_cpio(pubkey: &str) -> Result<Vec<u8>> {
    if pubkey.is_empty() {
        bail!("SSH public key is empty");
    }

    let mut buf = Vec::new();

    // Directories
    for dir in &["root", "root/.ssh"] {
        let b = NewcBuilder::new(dir)
            .mode(0o700)
            .set_mode_file_type(ModeFileType::Directory);
        b.write(&mut buf, 0).finish()?;
    }

    // authorized_keys
    let ak = format!("{}\n", pubkey);
    let b = NewcBuilder::new("root/.ssh/authorized_keys")
        .mode(0o600)
        .set_mode_file_type(ModeFileType::Regular);
    let mut w = b.write(&mut buf, ak.len() as u32);
    w.write_all(ak.as_bytes())?;
    w.finish()?;

    Ok(cpio::newc::trailer(buf)?)
}

// --- Boot file extraction and initramfs construction ---

pub(crate) struct BootFiles {
    pub kernel_path: PathBuf,
    pub initramfs_path: PathBuf,
    pub cache_dir: PathBuf,
}

/// Decompress vmlinuz (PE+zstd) to ARM64 Image.
/// ARM64 vmlinuz is a PE binary containing a zstd-compressed kernel payload.
/// The zboot header at offset 4 contains payload offset and size.
pub(crate) fn decompress_vmlinuz(vmlinuz_path: &Path, output_path: &Path) -> Result<()> {
    let data = fs::read(vmlinuz_path)?;

    if data.len() < 0x14 || &data[4..8] != b"zimg" {
        bail!("vmlinuz does not have zboot header (not PE+zstd?)");
    }

    let payload_offset = u32::from_le_bytes(data[0x08..0x0c].try_into().unwrap()) as usize;
    let payload_size = u32::from_le_bytes(data[0x0c..0x10].try_into().unwrap()) as usize;

    if payload_offset + payload_size > data.len() {
        bail!(
            "zboot payload exceeds file size (offset={}, size={}, file={})",
            payload_offset,
            payload_size,
            data.len()
        );
    }

    let compressed = &data[payload_offset..payload_offset + payload_size];
    let decompressed = zstd::decode_all(compressed)
        .context("zstd decompression of vmlinuz payload failed")?;

    if decompressed.len() > 0x40 && &decompressed[0x38..0x3c] == b"ARMd" {
        info!(
            "ARM64 Image decompressed: {} → {} bytes",
            data.len(),
            decompressed.len()
        );
    } else {
        tracing::warn!("decompressed kernel does not have ARM64 magic at 0x38");
    }

    fs::write(output_path, &decompressed)?;
    Ok(())
}

pub(crate) fn podman_machine_cat(
    machine: &str,
    ssh_prefix: &str,
    remote_path: &str,
    local_path: &Path,
) -> Result<()> {
    let cmd = format!("{}cat {}", ssh_prefix, remote_path);
    let output = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &cmd])
        .output()
        .context(format!("failed to cat {}", remote_path))?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!(
            "failed to extract {}: {}",
            remote_path,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::write(local_path, &output.stdout)?;
    Ok(())
}

pub(crate) fn ensure_boot_files(
    machine: &str,
    rootful: bool,
    merged_path: &str,
    digest_short: &str,
) -> Result<BootFiles> {
    let cache_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/bcvk/cache")
        .join(format!("boot-{}", digest_short));

    let vmlinuz_path = cache_dir.join("vmlinuz");
    let kernel_path = cache_dir.join("Image");
    let initramfs_path = cache_dir.join("initramfs.img");

    if kernel_path.exists() && initramfs_path.exists() {
        info!("boot files cache hit: {}", cache_dir.display());
        return Ok(BootFiles {
            kernel_path,
            initramfs_path,
            cache_dir,
        });
    }

    fs::create_dir_all(&cache_dir)?;
    info!("extracting boot files via podman machine ssh...");

    let ssh_prefix = if rootful { "" } else { "sudo " };

    let kver_cmd = format!(
        "{}ls {}/usr/lib/modules/ | head -1",
        ssh_prefix, merged_path
    );
    let kver_output = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &kver_cmd])
        .output()
        .context("failed to get kernel version")?;
    let kver = String::from_utf8_lossy(&kver_output.stdout)
        .trim()
        .to_string();
    if kver.is_empty() {
        bail!("kernel version not found in {}/usr/lib/modules/", merged_path);
    }
    info!("kernel version: {}", kver);

    let vmlinuz_remote = format!("{}/usr/lib/modules/{}/vmlinuz", merged_path, kver);
    podman_machine_cat(machine, ssh_prefix, &vmlinuz_remote, &vmlinuz_path)?;
    info!("vmlinuz extracted ({} bytes)", fs::metadata(&vmlinuz_path)?.len());

    let initramfs_remote = format!("{}/usr/lib/modules/{}/initramfs.img", merged_path, kver);
    podman_machine_cat(machine, ssh_prefix, &initramfs_remote, &initramfs_path)?;
    info!(
        "initramfs.img extracted ({} bytes)",
        fs::metadata(&initramfs_path)?.len()
    );

    let modules_cmd = format!(
        "{p}bash -c '\
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vsock.ko.xz > /tmp/vsock.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vmw_vsock_virtio_transport_common.ko.xz > /tmp/vmw_vsock_virtio_transport_common.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/net/vmw_vsock/vmw_vsock_virtio_transport.ko.xz > /tmp/vmw_vsock_virtio_transport.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/drivers/block/nbd.ko.xz > /tmp/nbd.ko 2>/dev/null; \
         xz -dk -c {m}/usr/lib/modules/{k}/kernel/drivers/block/ublk_drv.ko.xz > /tmp/ublk_drv.ko 2>/dev/null; \
         echo OK'",
        p = ssh_prefix,
        m = merged_path,
        k = kver,
    );
    let _ = Command::new("podman")
        .args(["machine", "ssh", machine, "--", &modules_cmd])
        .output();

    for ko in &[
        "vsock.ko",
        "vmw_vsock_virtio_transport_common.ko",
        "vmw_vsock_virtio_transport.ko",
        "nbd.ko",
        "ublk_drv.ko",
    ] {
        let _ = podman_machine_cat(
            machine,
            ssh_prefix,
            &format!("/tmp/{}", ko),
            &cache_dir.join(ko),
        );
    }
    info!("kernel modules extracted");

    let ublk_vsock_src = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/bcvk/ublk-vsock");
    if ublk_vsock_src.exists() {
        fs::copy(&ublk_vsock_src, cache_dir.join("ublk-vsock"))?;
        info!("copied ublk-vsock from {}", ublk_vsock_src.display());
    }

    info!("decompressing vmlinuz to ARM64 Image...");
    decompress_vmlinuz(&vmlinuz_path, &kernel_path)?;

    Ok(BootFiles {
        kernel_path,
        initramfs_path,
        cache_dir,
    })
}

/// Build initramfs with CPIO appends for vsock boot.
///
/// SSH key injection is handled by nbdkit EROFS plugin (`ssh_pubkey=` parameter),
/// not by initramfs CPIO — the initramfs tmpfs is unmounted after switch_root.
pub(crate) fn build_vsock_initramfs(
    boot_files: &BootFiles,
    vsock_port: u32,
    output_name: &str,
) -> Result<PathBuf> {
    let mut initramfs = fs::read(&boot_files.initramfs_path)?;

    let block_cpio =
        create_krun_block_device_cpio(vsock_port, &boot_files.cache_dir)?;
    append_cpio(&mut initramfs, &block_cpio);

    let overlay_cpio = crate::cpio::create_initramfs_units_cpio()
        .map_err(|e| color_eyre::eyre::eyre!("failed to create overlay CPIO: {e}"))?;
    append_cpio(&mut initramfs, &overlay_cpio);

    let final_path = boot_files.cache_dir.join(output_name);
    fs::write(&final_path, &initramfs)?;
    info!("vsock initramfs: {} bytes ({})", initramfs.len(), output_name);
    Ok(final_path)
}

// --- Unix socket bridge ---

/// Bridge two Unix sockets bidirectionally.
/// Connects to both sockets and relays data between them in two threads.
pub(crate) fn start_unix_bridge(sock_a: &str, sock_b: &str) -> Result<()> {
    use std::os::unix::net::UnixStream;

    let b = UnixStream::connect(sock_b)
        .context(format!("bridge: connect to nbdkit ({}) failed", sock_b))?;
    debug!("bridge: connected to nbdkit side");

    // Wait for VM vsock socket to appear and connect
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let a = loop {
        if std::time::Instant::now() > deadline {
            bail!("bridge: VM side connection timed out");
        }
        match UnixStream::connect(sock_a) {
            Ok(s) => {
                debug!("bridge: connected to VM side");
                break s;
            }
            Err(e) => {
                debug!("bridge: VM connect retry: {}", e);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    };

    // Give guest vsock transport time to fully initialize before relay starts.
    // Without this delay, data sent through the relay may be lost because the
    // guest's vsock transport hasn't bound to the virtio-vsock PCI device yet.
    std::thread::sleep(Duration::from_secs(3));

    info!("bridge: {} ↔ {}", sock_a, sock_b);

    let mut a_read = a.try_clone()?;
    let mut b_write = b.try_clone()?;
    let mut b_read = b;
    let mut a_write = a;

    std::thread::spawn(move || {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match std::io::Read::read(&mut a_read, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if std::io::Write::write_all(&mut b_write, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    std::thread::spawn(move || {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match std::io::Read::read(&mut b_read, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if std::io::Write::write_all(&mut a_write, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    Ok(())
}
