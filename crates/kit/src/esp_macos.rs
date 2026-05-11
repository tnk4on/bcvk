//! ESP image construction for macOS EFI boot.

use std::fs;
use std::process::Command;
use color_eyre::{eyre::{bail, Context}, Result};

use crate::run_ephemeral_macos::ephemeral_base_dir;

/// Get the merged overlay path from podman image mount.
pub(crate) fn get_merged_path(machine: &str, rootful: bool, image: &str) -> Result<String> {
    let output = if rootful {
        Command::new("podman")
            .args(["machine", "ssh", machine, "--", "podman", "image", "mount", image])
            .output()
            .context("podman image mount")?
    } else {
        Command::new("podman")
            .args(["machine", "ssh", machine, "--", "podman", "unshare", "podman", "image", "mount", image])
            .output()
            .context("podman image mount")?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("podman image mount failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Build an ESP image with GRUB, kernel, initramfs, and SSH key.
pub(crate) fn build_esp_image(
    machine: &str,
    merged_path: &str,
    cmdline: &str,
    ssh_pubkey: &str,
    esp_output: &str,
) -> Result<()> {
    let ssh_setup = if ssh_pubkey.is_empty() {
        String::new()
    } else {
        format!(
            r#"
SSHDIR=$(mktemp -d)
mkdir -p "$SSHDIR/usr/lib/bcvk" "$SSHDIR/usr/lib/systemd/system/initrd-fs.target.d"
cat > "$SSHDIR/usr/lib/bcvk/setup-ssh.sh" << 'SSHSCRIPT'
#!/bin/bash
mkdir -p /sysroot/var/roothome /sysroot/var/empty /sysroot/var/log /sysroot/var/tmp
chmod 700 /sysroot/var/roothome
chmod 711 /sysroot/var/empty
mkdir -p /sysroot/var/roothome/.ssh
chmod 700 /sysroot/var/roothome/.ssh
echo '{pubkey}' > /sysroot/var/roothome/.ssh/authorized_keys
chmod 600 /sysroot/var/roothome/.ssh/authorized_keys
chown -R 0:0 /sysroot/var/roothome/.ssh
SSHSCRIPT
chmod 755 "$SSHDIR/usr/lib/bcvk/setup-ssh.sh"
cat > "$SSHDIR/usr/lib/systemd/system/bcvk-ssh-setup.service" << 'SVCEOF'
[Unit]
Description=Setup SSH authorized_keys for root
DefaultDependencies=no
ConditionPathExists=/etc/initrd-release
Before=initrd-fs.target
After=bcvk-var-ephemeral.service
Requires=bcvk-var-ephemeral.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-ssh.sh
SVCEOF
cat > "$SSHDIR/usr/lib/systemd/system/initrd-fs.target.d/bcvk-ssh-setup.conf" << 'DROPEOF'
[Unit]
Wants=bcvk-ssh-setup.service
DROPEOF
ISIZE=$(stat -c%s "$BUILDDIR/initramfs.img")
PAD=$(( (4 - ISIZE % 4) % 4 ))
[ $PAD -gt 0 ] && dd if=/dev/zero bs=1 count=$PAD >> "$BUILDDIR/initramfs.img" 2>/dev/null
(cd "$SSHDIR" && find . -mindepth 1 | cpio -o -H newc --quiet) >> "$BUILDDIR/initramfs.img"
rm -rf "$SSHDIR"
"#,
            pubkey = ssh_pubkey,
        )
    };

    let script = format!(
        r#"
set -e
MERGED="{merged}"
ESPOUT="{esp_output}"
mkdir -p /var/tmp/bcvk
BUILDDIR=$(mktemp -d /var/tmp/bcvk/esp-build.XXXXXX)
mkdir -p "$(dirname "$ESPOUT")"

KVER=$(ls "$MERGED/usr/lib/modules/" | head -1)
cp "$MERGED/usr/lib/modules/$KVER/vmlinuz" "$BUILDDIR/vmlinuz"
cp "$MERGED/usr/lib/modules/$KVER/initramfs.img" "$BUILDDIR/initramfs.img"

# Append bcvk systemd units CPIO
UNITSDIR=$(mktemp -d)
UDIR="$UNITSDIR/usr/lib/systemd/system"
DDIR="$UDIR/initrd-fs.target.d"
mkdir -p "$UDIR" "$DDIR"

cat > "$UDIR/bcvk-var-ephemeral.service" << 'UNITEOF'
[Unit]
Description=Setup ephemeral /var from image content
DefaultDependencies=no
ConditionPathExists=/etc/initrd-release
Before=initrd-fs.target
After=sysroot.mount initrd-parse-etc.service
Requires=sysroot.mount

[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=60
ExecStart=/usr/bin/mkdir -p /run/var-ephemeral
ExecStart=/usr/bin/cp -a /sysroot/var/. /run/var-ephemeral/
ExecStart=/usr/bin/mount --bind /run/var-ephemeral /sysroot/var
UNITEOF

cat > "$UDIR/bcvk-etc-overlay.service" << 'UNITEOF'
[Unit]
Description=Setup ephemeral /etc overlay
DefaultDependencies=no
ConditionPathExists=/etc/initrd-release
Before=initrd-fs.target
After=sysroot.mount initrd-parse-etc.service
Requires=sysroot.mount

[Service]
Type=oneshot
RemainAfterExit=yes
TimeoutStartSec=30
ExecStart=/usr/bin/mkdir -p /run/etc-lower /run/etc-upper /run/etc-work
ExecStart=/usr/bin/mount --bind /sysroot/etc /run/etc-lower
ExecStart=/usr/bin/mount -t overlay overlay -o lowerdir=/run/etc-lower,upperdir=/run/etc-upper,workdir=/run/etc-work,index=off,metacopy=off /sysroot/etc
UNITEOF

cat > "$UDIR/bcvk-copy-units.service" << 'UNITEOF'
[Unit]
Description=Copy bcvk units for post-switch-root on systemd <256
DefaultDependencies=no
ConditionPathExists=/etc/initrd-release
Before=initrd-fs.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'mkdir -p /run/systemd/system/sysinit.target.wants && cp /usr/lib/systemd/system/bcvk-journal-stream.service /run/systemd/system/ && ln -s ../bcvk-journal-stream.service /run/systemd/system/sysinit.target.wants/'
UNITEOF

cat > "$UDIR/bcvk-journal-stream.service" << 'UNITEOF'
[Unit]
Description=Stream journal to virtio-serial
DefaultDependencies=no

[Service]
Type=simple
ExecStart=/bin/sh -c 'journalctl -f --no-hostname -o short-monotonic > /dev/hvc1 2>&1 || true'
UNITEOF

printf '[Unit]\nWants=bcvk-var-ephemeral.service\n' > "$DDIR/bcvk-var-ephemeral.conf"
printf '[Unit]\nWants=bcvk-etc-overlay.service\n' > "$DDIR/bcvk-etc-overlay.conf"
printf '[Unit]\nWants=bcvk-copy-units.service\n' > "$DDIR/bcvk-copy-units.conf"

ISIZE=$(stat -c%s "$BUILDDIR/initramfs.img")
PAD=$(( (4 - ISIZE % 4) % 4 ))
[ $PAD -gt 0 ] && dd if=/dev/zero bs=1 count=$PAD >> "$BUILDDIR/initramfs.img" 2>/dev/null
(cd "$UNITSDIR" && find . -mindepth 1 | cpio -o -H newc --quiet) >> "$BUILDDIR/initramfs.img"
rm -rf "$UNITSDIR"

{ssh_setup}

# Build ESP FAT32 image
mkdir -p "$BUILDDIR/esp/EFI/BOOT" "$BUILDDIR/esp/boot"
GRUB_EFI=$(find "$MERGED/usr/lib" -name "grubaa64.efi" 2>/dev/null | head -1)
if [ -z "$GRUB_EFI" ]; then
  echo "ERROR: grubaa64.efi not found" >&2
  exit 1
fi
cp "$GRUB_EFI" "$BUILDDIR/esp/EFI/BOOT/BOOTAA64.EFI"
cp "$BUILDDIR/vmlinuz" "$BUILDDIR/esp/boot/vmlinuz"
cp "$BUILDDIR/initramfs.img" "$BUILDDIR/esp/boot/initramfs.img"

cat > "$BUILDDIR/esp/EFI/BOOT/grub.cfg" << GRUBEOF
set timeout=0
set default=0
menuentry "bcvk" {{
  linux /boot/vmlinuz {cmdline}
  initrd /boot/initramfs.img
}}
GRUBEOF

ESP_SIZE=$(( $(du -sb "$BUILDDIR/esp" | cut -f1) + 10*1024*1024 ))
ESP_SIZE_MB=$(( (ESP_SIZE + 1048575) / 1048576 ))
dd if=/dev/zero of="$ESPOUT" bs=1M count=$ESP_SIZE_MB status=none
mkfs.vfat -F 32 "$ESPOUT" > /dev/null 2>&1
ESPMNT="$BUILDDIR/esp-mnt"
mkdir -p "$ESPMNT"
mount -o loop "$ESPOUT" "$ESPMNT"
cp -r "$BUILDDIR/esp/"* "$ESPMNT/"
sync
umount "$ESPMNT"
rm -rf "$BUILDDIR"
"#,
        merged = merged_path,
        esp_output = esp_output,
        ssh_setup = ssh_setup,
        cmdline = cmdline,
    );

    let script_path = format!("{}/build-esp-{}.sh", ephemeral_base_dir().display(), std::process::id());
    fs::write(&script_path, &script).context("writing ESP build script")?;
    let output = Command::new("podman")
        .args(["machine", "ssh", machine, "--", "bash", &script_path])
        .output()
        .context("building ESP image")?;
    let _ = fs::remove_file(&script_path);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ESP build failed: {}", stderr.trim());
    }
    Ok(())
}

