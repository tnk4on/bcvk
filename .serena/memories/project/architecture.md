# bcvk Architecture

## Linux Ephemeral VM Process Model
```
bcvk run → podman run --privileged → entrypoint.sh → bwrap → container-entrypoint → virtiofsd + QEMU
```
- Each VM is a separate podman container (full namespace isolation)
- SSH keys stored in /var/lib/bcvk/ssh (container-internal, no conflicts)
- SSH port 2222 hardcoded but isolated per container
- VirtioFS root via virtiofsd daemon

## macOS Ephemeral VM Process Model (wip/macos-vfkit)
```
bcvk run → gvproxy + vfkit (direct host process, no container isolation)
```
- No container isolation — resources must be manually per-VM isolated
- SSH keys: /private/tmp/bcvk/{vm_name}-key
- SSH port: dynamic allocation (find_available_ssh_port, 2222-3000)
- SquashFS rootfs via virtio-blk (read-only)
- Kernel: vmlinuz PE+zstd → ARM64 Image decompression

## Key Modules
- run_ephemeral.rs: Linux ephemeral VM (1500+ lines, podman+QEMU)
- run_ephemeral_macos.rs: macOS ephemeral VM (700+ lines, vfkit)
- ssh.rs: SSH key generation + connection (Linux, podman exec based)
- ssh_options.rs: Cross-platform SSH options (CommonSshOptions)
- ephemeral.rs / ephemeral_macos.rs: Subcommand dispatchers
- vfkit/: macOS persistent VM module (libvirt/ equivalent)
- bcvk-qemu/: QEMU config builder (Linux only)
