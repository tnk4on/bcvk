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
const VSOCK_NBD_C: &str = r###"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/ioctl.h>
#include <linux/nbd.h>
#include <linux/vm_sockets.h>
#include <stdint.h>
#include <endian.h>

static int readall(int fd, void *buf, size_t n) {
    size_t done = 0;
    while (done < n) {
        ssize_t r = read(fd, (char*)buf + done, n - done);
        if (r <= 0) return -1;
        done += r;
    }
    return 0;
}

static int writeall(int fd, const void *buf, size_t n) {
    size_t done = 0;
    while (done < n) {
        ssize_t r = write(fd, (const char*)buf + done, n - done);
        if (r <= 0) return -1;
        done += r;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 4) { fprintf(stderr, "Usage: nbd-vsock /dev/nbdX cid port\n"); return 1; }
    const char *dev = argv[1];
    unsigned int cid = atoi(argv[2]);
    unsigned int port = atoi(argv[3]);

    int vs = socket(AF_VSOCK, SOCK_STREAM, 0);
    if (vs < 0) { perror("vsock socket"); return 1; }
    struct sockaddr_vm a = {0};
    a.svm_family = AF_VSOCK; a.svm_cid = cid; a.svm_port = port;
    fprintf(stderr, "nbd-vsock: connecting cid=%u port=%u\n", cid, port);
    if (connect(vs, (struct sockaddr*)&a, sizeof(a)) < 0) { perror("vsock connect"); return 1; }
    fprintf(stderr, "nbd-vsock: connected\n");

    /* NBD newstyle-fixed handshake */
    uint64_t magic, ihaveopt;
    uint16_t hflags;
    if (readall(vs, &magic, 8) < 0 || readall(vs, &ihaveopt, 8) < 0 || readall(vs, &hflags, 2) < 0) {
        fprintf(stderr, "nbd-vsock: handshake read failed\n"); return 1;
    }
    magic = be64toh(magic); ihaveopt = be64toh(ihaveopt); hflags = be16toh(hflags);
    fprintf(stderr, "nbd-vsock: magic=%llx flags=%x\n", (unsigned long long)ihaveopt, hflags);

    /* Send client flags */
    uint32_t cflags = htobe32(1); /* NBD_FLAG_C_FIXED_NEWSTYLE */
    writeall(vs, &cflags, 4);

    /* Send NBD_OPT_EXPORT_NAME (option 1, empty name) */
    uint64_t opt_magic = htobe64(0x49484156454F5054ULL);
    uint32_t opt_id = htobe32(1); /* NBD_OPT_EXPORT_NAME */
    uint32_t opt_len = htobe32(0);
    writeall(vs, &opt_magic, 8);
    writeall(vs, &opt_id, 4);
    writeall(vs, &opt_len, 4);

    /* Receive export info */
    uint64_t export_size;
    uint16_t tflags;
    char pad[124];
    if (readall(vs, &export_size, 8) < 0 || readall(vs, &tflags, 2) < 0 || readall(vs, pad, 124) < 0) {
        fprintf(stderr, "nbd-vsock: export info read failed\n"); return 1;
    }
    export_size = be64toh(export_size);
    tflags = be16toh(tflags);
    fprintf(stderr, "nbd-vsock: export size=%llu bytes, flags=%x\n", (unsigned long long)export_size, tflags);

    /* Configure NBD device */
    int nb = open(dev, O_RDWR);
    if (nb < 0) { perror("open nbd"); return 1; }
    ioctl(nb, NBD_CLEAR_SOCK);
    ioctl(nb, NBD_SET_BLKSIZE, (unsigned long)512);
    ioctl(nb, NBD_SET_SIZE, (unsigned long)export_size);
    ioctl(nb, NBD_SET_FLAGS, (unsigned long)tflags);
    if (ioctl(nb, NBD_SET_SOCK, vs) < 0) { perror("NBD_SET_SOCK"); return 1; }
    fprintf(stderr, "nbd-vsock: device configured, starting kernel I/O\n");

    if (fork() == 0) { ioctl(nb, NBD_DO_IT); ioctl(nb, NBD_CLEAR_SOCK); _exit(0); }
    sleep(2);
    fprintf(stderr, "nbd-vsock: ready\n");
    return 0;
}
"###;

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

    let script = String::from(
        "dnf install -y nbd gcc make >/dev/null 2>&1; \
         KVER=$(ls /usr/lib/modules/ | head -1); \
         KVER_SHORT=$(echo $KVER | sed 's/-.*//'); \
         KVER_MAJOR=${KVER_SHORT%%.*}; \
         dnf install -y kernel-devel-$KVER >/dev/null 2>&1; \
         mkdir -p /tmp/nbd-patch && cd /tmp/nbd-patch && \
         echo NBD_PATCH: downloading kernel $KVER_SHORT source >&2 && \
         curl -sfL https://cdn.kernel.org/pub/linux/kernel/v${KVER_MAJOR}.x/linux-${KVER_SHORT}.tar.xz \
           | tar xJ --strip-components=3 linux-${KVER_SHORT}/drivers/block/nbd.c && \
         echo NBD_PATCH: patching nbd.c >&2 && \
         sed -i 's/!sk_is_stream_unix(sock->sk))/!sk_is_stream_unix(sock->sk) \\&\\& sock->sk->sk_family != AF_VSOCK)/' nbd.c && \
         grep -n AF_VSOCK nbd.c >&2 && \
         echo 'obj-m += nbd.o' > Makefile && \
         echo NBD_PATCH: building module >&2 && \
         make -C /lib/modules/$KVER/build M=$(pwd) modules >/dev/null 2>&1 && \
         echo NBD_PATCH: installing module >&2 && \
         cp nbd.ko /lib/modules/$KVER/kernel/drivers/block/nbd.ko && \
         depmod -a $KVER && \
         echo NBD_PATCH: success >&2; \
         cat > /tmp/nbd-vsock.c << 'CEOF'\n",
    ) + VSOCK_NBD_C.trim()
        + "\nCEOF\n\
         gcc -o /tmp/nbd-vsock /tmp/nbd-vsock.c && \
         mkdir -p /usr/lib/dracut/modules.d/99bcvk-vsock && \
         cp /tmp/nbd-vsock /usr/lib/dracut/modules.d/99bcvk-vsock/ && \
         printf '#!/bin/bash\\ncheck() { return 0; }\\ndepends() { return 0; }\\ninstall() {\\n  inst_simple \"$moddir/nbd-vsock\" /usr/bin/nbd-vsock\\n  inst_hook pre-udev 00 \"$moddir/setup-nbd.sh\"\\n}\\n' > /usr/lib/dracut/modules.d/99bcvk-vsock/module-setup.sh && \
         printf '#!/bin/bash\\nmodprobe hv_sock 2>/dev/null\\nmodprobe nbd max_part=16 2>/dev/null\\nsleep 1\\n/usr/bin/nbd-vsock /dev/nbd0 2 10800\\n' > /usr/lib/dracut/modules.d/99bcvk-vsock/setup-nbd.sh && \
         chmod +x /usr/lib/dracut/modules.d/99bcvk-vsock/*.sh && \
         dracut --force --no-hostonly --add 'nbd network base bcvk-vsock' \
         --add-drivers 'hv_sock hv_utils hv_vmbus vsock nbd' \
         --kver $KVER /tmp/initramfs.img 2>/dev/null && \
         cat /tmp/initramfs.img";
    let initramfs = podman_run_stdout(image, &script)?;
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
