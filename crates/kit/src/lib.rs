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

// Windows-only modules (Hyper-V PXE + NBD backend)
#[cfg(target_os = "windows")]
pub mod nbdkit_macos; // reused: OS-independent despite the name
#[cfg(target_os = "windows")]
pub mod run_ephemeral_macos; // reused: detect_machine_name, wait_for_ssh, etc.
#[cfg(target_os = "windows")]
pub mod hyperv;
#[cfg(target_os = "windows")]
pub mod ssh_forward;
#[cfg(target_os = "windows")]
pub mod pxe_server;
#[cfg(target_os = "windows")]
pub mod boot_files;
#[cfg(target_os = "windows")]
pub mod run_ephemeral_windows;
#[cfg(target_os = "windows")]
pub mod ephemeral_windows;
