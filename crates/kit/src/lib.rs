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
pub mod run_ephemeral_macos;

#[cfg(target_os = "macos")]
pub mod vfkit;

// Windows-only modules (Hyper-V PXE + NBD backend)
#[cfg(target_os = "windows")]
#[allow(missing_docs, dead_code)]
pub mod hyperv;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod ssh_forward;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod pxe_server;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod boot_files;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod run_ephemeral_windows;
#[cfg(target_os = "windows")]
#[allow(missing_docs, unsafe_code)]
pub mod hv_sock_proxy;
#[cfg(target_os = "windows")]
#[allow(missing_docs)]
pub mod ephemeral_windows;

// nbdkit module: shared between macOS and Windows
// Contains podman machine ssh calls (OS-independent)
#[cfg(target_os = "macos")]
pub mod nbdkit_macos;
