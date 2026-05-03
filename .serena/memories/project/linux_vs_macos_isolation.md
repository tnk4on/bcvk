# Linux vs macOS VM Resource Isolation

## Core Difference
Linux: Each VM is a podman container → full namespace isolation (PID, mount, net)
macOS: Each VM is a direct vfkit process → resources must be manually per-VM isolated

## Linux Resource Isolation (automatic via container)
- SSH key: /var/lib/bcvk/ssh (container-internal, independent per container)
- SSH port: 2222 hardcoded but inside container network namespace (no conflict)
- SSH connection: `podman exec CONTAINER ssh ...` (container-scoped)
- VirtioFS sockets: /run/inner-shared/ (container tmpfs)
- Kernel/initramfs: /run/qemu/ (container tmpfs)
- Logs: /run/ (container tmpfs)

## macOS Resource Isolation (manual, per-VM naming required)
- SSH key: /private/tmp/bcvk/{vm_name}-key
- SSH port: find_available_ssh_port() (2222-3000 random)
- SSH connection: direct ssh -p PORT root@localhost
- gvproxy socket: /private/tmp/bcvk/{vm_name}-gvproxy.sock
- initramfs: /private/tmp/bcvk/{vm_name}-initramfs.img
- SquashFS: shared read-only (rootfs-{digest}.squashfs)
- Kernel/Image: shared read-only (boot-{digest}/)

## Remaining Issues (macOS)
1. vfkit.log is shared — needs per-VM naming
2. Same SquashFS attached to multiple vfkit instances — needs verification
