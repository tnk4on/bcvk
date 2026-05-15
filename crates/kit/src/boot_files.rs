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
pub fn extract_boot_files(image: &str, ssh_pubkey: &str) -> Result<BootFiles> {
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

    let ssh_setup = if ssh_pubkey.is_empty() {
        String::new()
    } else {
        format!(
            "cat > /usr/lib/dracut/modules.d/99bcvk-vsock/setup-ssh.sh << 'SSHEOF'\n\
             #!/bin/bash\n\
             mkdir -p /sysroot/var/roothome /sysroot/var/empty /sysroot/var/log /sysroot/var/tmp\n\
             chmod 700 /sysroot/var/roothome\n\
             chmod 711 /sysroot/var/empty\n\
             mkdir -p /sysroot/var/roothome/.ssh\n\
             chmod 700 /sysroot/var/roothome/.ssh\n\
             echo '{}' > /sysroot/var/roothome/.ssh/authorized_keys\n\
             chmod 600 /sysroot/var/roothome/.ssh/authorized_keys\n\
             chown -R 0:0 /sysroot/var/roothome/.ssh\n\
             echo 'root:bcvk' | chroot /sysroot chpasswd\n\
             SSHEOF\n\
             chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/setup-ssh.sh && ",
            ssh_pubkey.trim()
        )
    };

    let script = format!(
        "dnf install -y nbd gcc make >/dev/null 2>&1; \
         KVER=$(ls /usr/lib/modules/ | head -1); \
         echo INITRAMFS: kver=$KVER >&2; \
         dnf install -y kernel-devel-$KVER >/dev/null 2>&1; \
         KVER_SHORT=$(echo $KVER | sed 's/-.*//'); \
         KVER_MAJOR=${{KVER_SHORT%%.*}}; \
         echo INITRAMFS: patching nbd.ko for AF_VSOCK >&2; \
         mkdir -p /tmp/nbd-patch && cd /tmp/nbd-patch && \
         curl -sfL https://cdn.kernel.org/pub/linux/kernel/v${{KVER_MAJOR}}.x/linux-${{KVER_SHORT}}.tar.xz \
           | tar xJ --strip-components=3 linux-${{KVER_SHORT}}/drivers/block/nbd.c && \
         sed -i '/!sk_is_tcp(sock->sk) &&/{{N;s/!sk_is_stream_unix(sock->sk))/!sk_is_stream_unix(sock->sk) \\&\\& sock->sk->sk_family != AF_VSOCK)/}}' nbd.c && \
         grep -n AF_VSOCK nbd.c >&2 && \
         echo 'obj-m += nbd.o' > Makefile && \
         make -C /lib/modules/$KVER/build M=$(pwd) modules >/dev/null 2>&1 && \
         cp nbd.ko /lib/modules/$KVER/kernel/drivers/block/nbd.ko && \
         depmod -a $KVER && \
         echo INITRAMFS: nbd.ko patched >&2; \
         cd /; \
         mkdir -p /usr/lib/dracut/modules.d/99bcvk-vsock && \
         cp /tmp/nbd-vsock-host /usr/lib/dracut/modules.d/99bcvk-vsock/nbd-vsock && \
         chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/nbd-vsock && \
         printf '#!/bin/bash\\ncheck() {{ return 0; }}\\ndepends() {{ return 0; }}\\ninstall() {{\\n\
           inst_multiple nbd-client blockdev mount cp mkdir chmod chown\\n\
           inst_simple \"$moddir/nbd-vsock\" /usr/bin/nbd-vsock\\n\
           inst_hook pre-udev 00 \"$moddir/setup-nbd.sh\"\\n\
           inst_simple \"$moddir/bcvk-etc-overlay.service\" /usr/lib/systemd/system/bcvk-etc-overlay.service\\n\
           inst_simple \"$moddir/bcvk-var-ephemeral.service\" /usr/lib/systemd/system/bcvk-var-ephemeral.service\\n\
           mkdir -p \"$initdir/usr/lib/systemd/system/initrd-fs.target.d\"\\n\
           echo \"[Unit]\" > \"$initdir/usr/lib/systemd/system/initrd-fs.target.d/bcvk-overlay.conf\"\\n\
           echo \"Wants=bcvk-etc-overlay.service bcvk-var-ephemeral.service\" >> \"$initdir/usr/lib/systemd/system/initrd-fs.target.d/bcvk-overlay.conf\"\\n\
           {ssh_install}\
         }}\\n' > /usr/lib/dracut/modules.d/99bcvk-vsock/module-setup.sh && \
         printf '#!/bin/bash\\necho \"<6>setup-nbd: start\" > /dev/kmsg\\nmodprobe vsock 2>/dev/null\\nmodprobe hv_sock 2>/dev/null\\nmodprobe nbd max_part=16 2>/dev/null\\nsleep 1\\necho \"<6>setup-nbd: running nbd-vsock\" > /dev/kmsg\\n/usr/bin/nbd-vsock /dev/nbd0 2 10800 2>/dev/kmsg\\necho \"<6>setup-nbd: nbd-vsock exit=$?\" > /dev/kmsg\\nsleep 1\\nblockdev --rereadpt /dev/nbd0 2>/dev/null\\nls -la /dev/nbd0* > /dev/kmsg 2>&1\\n' > /usr/lib/dracut/modules.d/99bcvk-vsock/setup-nbd.sh && \
         cat > /usr/lib/dracut/modules.d/99bcvk-vsock/bcvk-etc-overlay.service << 'ETCEOF'\n\
[Unit]\n\
Description=Setup ephemeral /etc overlay\n\
DefaultDependencies=no\n\
ConditionPathExists=/etc/initrd-release\n\
Before=initrd-fs.target\n\
After=sysroot.mount initrd-parse-etc.service\n\
Requires=sysroot.mount\n\
[Service]\n\
Type=oneshot\n\
RemainAfterExit=yes\n\
TimeoutStartSec=30\n\
ExecStart=/usr/bin/mkdir -p /run/etc-lower /run/etc-upper /run/etc-work\n\
ExecStart=/usr/bin/mount --bind /sysroot/etc /run/etc-lower\n\
ExecStart=/usr/bin/mount -t overlay overlay -o lowerdir=/run/etc-lower,upperdir=/run/etc-upper,workdir=/run/etc-work,index=off,metacopy=off /sysroot/etc\n\
ETCEOF\n\
         cat > /usr/lib/dracut/modules.d/99bcvk-vsock/bcvk-var-ephemeral.service << 'VAREOF'\n\
[Unit]\n\
Description=Setup ephemeral /var from image content\n\
DefaultDependencies=no\n\
ConditionPathExists=/etc/initrd-release\n\
Before=initrd-fs.target\n\
After=sysroot.mount initrd-parse-etc.service\n\
Requires=sysroot.mount\n\
[Service]\n\
Type=oneshot\n\
RemainAfterExit=yes\n\
TimeoutStartSec=60\n\
ExecStart=/usr/bin/mkdir -p /run/var-ephemeral\n\
ExecStart=/usr/bin/cp -a /sysroot/var/. /run/var-ephemeral/\n\
ExecStart=/usr/bin/mount --bind /run/var-ephemeral /sysroot/var\n\
VAREOF\n\
         {ssh_setup}\
         chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/*.sh && \
         echo INITRAMFS: dracut start >&2 && \
         mkdir -p /var/roothome 2>/dev/null; \
         dracut --force --no-hostonly --add 'nbd network bcvk-vsock' \
         --add-drivers 'hv_sock hv_utils hv_vmbus vsock nbd overlay' \
         --kver $KVER /tmp/initramfs.img; \
         echo INITRAMFS: dracut exit=$? >&2; \
         test -f /tmp/initramfs.img && cat /tmp/initramfs.img",
        ssh_install = if ssh_pubkey.is_empty() {
            String::new()
        } else {
            "inst_simple \\\"$moddir/setup-ssh.sh\\\" /usr/lib/bcvk/setup-ssh.sh\\n\
             inst_simple \\\"$moddir/bcvk-ssh-setup.service\\\" /usr/lib/systemd/system/bcvk-ssh-setup.service\\n\
             echo \\\"[Unit]\\\" >> \\\"$initdir/usr/lib/systemd/system/initrd-fs.target.d/bcvk-overlay.conf\\\"\\n\
             echo \\\"Wants=bcvk-ssh-setup.service\\\" >> \\\"$initdir/usr/lib/systemd/system/initrd-fs.target.d/bcvk-overlay.conf\\\"\\n".to_string()
        },
        ssh_setup = if ssh_pubkey.is_empty() {
            String::new()
        } else {
            format!(
                "cat > /usr/lib/dracut/modules.d/99bcvk-vsock/bcvk-ssh-setup.service << 'SSHSVCEOF'\n\
[Unit]\n\
Description=Setup SSH authorized_keys for root\n\
DefaultDependencies=no\n\
ConditionPathExists=/etc/initrd-release\n\
Before=initrd-fs.target\n\
After=bcvk-var-ephemeral.service\n\
Requires=bcvk-var-ephemeral.service\n\
[Service]\n\
Type=oneshot\n\
RemainAfterExit=yes\n\
ExecStart=/usr/bin/bash /usr/lib/bcvk/setup-ssh.sh\n\
SSHSVCEOF\n\
                 cat > /usr/lib/dracut/modules.d/99bcvk-vsock/setup-ssh.sh << 'SSHEOF'\n\
#!/bin/bash\n\
mkdir -p /sysroot/var/roothome /sysroot/var/empty /sysroot/var/log /sysroot/var/tmp\n\
chmod 700 /sysroot/var/roothome\n\
chmod 711 /sysroot/var/empty\n\
mkdir -p /sysroot/var/roothome/.ssh\n\
chmod 700 /sysroot/var/roothome/.ssh\n\
echo '{}' > /sysroot/var/roothome/.ssh/authorized_keys\n\
chmod 600 /sysroot/var/roothome/.ssh/authorized_keys\n\
chown -R 0:0 /sysroot/var/roothome/.ssh\n\
echo 'root:bcvk' | chroot /sysroot chpasswd\n\
SSHEOF\n\
                 chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/setup-ssh.sh && ",
                ssh_pubkey.trim()
            )
        },
    );

    let output = Command::new("podman")
        .args([
            "run", "--rm", "--privileged",
            "-v", "/tmp/bcvk-nbd-vsock:/tmp/nbd-vsock-host:ro,z",
            image, "bash", "-c", &script,
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
