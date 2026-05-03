# bcvk - Bootc Virtualization Kit

## Purpose
CLI toolkit for building, testing, and managing bootc containers using ephemeral VMs.
Runs bootc images as temporary VMs, installs to disk, or manages existing installations.

## Tech Stack
- Language: Rust (edition 2021)
- CLI: clap 4.4 (derive)
- Error handling: color-eyre
- Logging: tracing + tracing-subscriber
- Serialization: serde + serde_json
- Async: tokio (Linux only, for QEMU management)

## Platform Support
- Linux: QEMU/KVM + libvirt (full functionality)
- macOS: vfkit (Apple Virtualization.framework) — in development on wip/macos-vfkit branch

## Key Crates
- `crates/kit/` — Main binary, all commands (15k+ lines)
- `crates/bcvk-qemu/` — QEMU configuration builder (Linux, 1585 lines)
- `crates/bcvk-krun/` — krunkit/libkrun wrapper (experimental, 461 lines)
- `crates/bcvk-fsd/` — ProxyFs file server (experimental, 248 lines)
- `crates/integration-tests/` — E2E tests
