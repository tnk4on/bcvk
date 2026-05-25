//! bcvk library - exposes internal modules for testing

pub mod cpio;
pub mod qemu_img;
pub mod ssh_options;
pub mod xml_utils;

// Cross-platform modules
pub mod ssh_options;

// Linux-only modules
#[cfg(target_os = "linux")]
pub mod kernel;

// macOS-only modules (vfkit backend)
#[cfg(target_os = "macos")]
pub mod nbdkit_macos;
#[cfg(target_os = "macos")]
pub mod run_ephemeral_macos;

#[cfg(target_os = "macos")]
pub mod vfkit;

// macOS krun backend (requires --features krun and libkrun-efi >= 1.18.0)
#[cfg(all(target_os = "macos", feature = "krun"))]
pub mod boot_files_macos;
#[cfg(all(target_os = "macos", feature = "krun"))]
pub mod run_ephemeral_macos_krun;
