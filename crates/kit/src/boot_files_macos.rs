//! Initramfs CPIO construction for macOS krun + ublk/NBD over vsock.
//!
//! Builds CPIO archives containing ublk-vsock/nbd-vsock binaries, kernel modules,
//! shell scripts, and systemd services for injection into the initramfs.
//! The guest VM uses these to create a block device (ublk or NBD) connected
//! to nbdkit via AF_VSOCK.

use color_eyre::{eyre::bail, Result};
use std::io::Write;
use std::path::Path;

use cpio::newc::Builder as NewcBuilder;
use cpio::newc::ModeFileType;
use tracing::info;

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
        /usr/bin/ublk-vsock --connect /dev/ublkb0 \"$VSOCK_PORT\" 1 2>/dev/kmsg &\n\
        UBLK_PID=$!\n\
        i=0\n\
        while [ $i -lt 60 ]; do\n\
            if [ -b /dev/ublkb0 ]; then\n\
                echo 'bcvk: ublk device ready' > /dev/kmsg\n\
                exit 0\n\
            fi\n\
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
exec /usr/bin/nbd-vsock --connect /dev/nbd0 \"$VSOCK_PORT\" 1 2>/dev/kmsg\n",
        vsock_port = vsock_port
    );
    write_script(buf, "usr/lib/bcvk/block-device-setup.sh", &block_device_script)?;

    let post_script = "\
#!/bin/bash\n\
sleep 2\n\
if [ -b /dev/ublkb0 ]; then\n\
    DEV=ublkb0\n\
else\n\
    DEV=nbd0\n\
fi\n\
echo \"bcvk: post-setup for /dev/$DEV\" > /dev/kmsg\n\
blockdev --rereadpt /dev/$DEV 2>/dev/kmsg\n\
# Ensure partition device nodes exist (initramfs devtmpfs may not auto-create)\n\
if [ ! -b /dev/${DEV}p2 ] && [ -f /sys/block/${DEV}/${DEV}p2/partition ]; then\n\
    MAJOR=$(cat /sys/block/${DEV}/dev | cut -d: -f1)\n\
    MINOR2=$(cat /sys/block/${DEV}/${DEV}p2/dev | cut -d: -f2)\n\
    mknod /dev/${DEV}p2 b $MAJOR $MINOR2\n\
    echo \"bcvk: created /dev/${DEV}p2 ($MAJOR:$MINOR2)\" > /dev/kmsg\n\
fi\n\
if [ ! -b /dev/${DEV}p1 ] && [ -f /sys/block/${DEV}/${DEV}p1/partition ]; then\n\
    MAJOR=$(cat /sys/block/${DEV}/dev | cut -d: -f1)\n\
    MINOR1=$(cat /sys/block/${DEV}/${DEV}p1/dev | cut -d: -f2)\n\
    mknod /dev/${DEV}p1 b $MAJOR $MINOR1\n\
fi\n\
echo 65536 > /sys/block/$DEV/queue/read_ahead_kb 2>/dev/null\n\
echo \"bcvk: post-setup for /dev/$DEV\" > /dev/kmsg\n\
echo 65536 > /sys/block/$DEV/queue/read_ahead_kb 2>/dev/null\n\
# Mount root directly — systemd device unit doesn't activate for nbd partitions\n\
mount -t erofs -o ro /dev/${DEV}p2 /sysroot 2>/dev/kmsg\n\
if [ $? -eq 0 ]; then\n\
    echo \"bcvk: mounted /dev/${DEV}p2 on /sysroot\" > /dev/kmsg\n\
    # Signal to systemd that root is ready\n\
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
         Type=notify\n\
         NotifyAccess=all\n\
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

/// Create SSH key injection CPIO for macOS krun backend.
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
