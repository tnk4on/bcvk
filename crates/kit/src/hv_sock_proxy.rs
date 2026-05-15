//! Hyper-V Socket (hv_sock) proxy: AF_HYPERV listener → TCP forward.
//!
//! Bypasses vmswitch.sys TCP blocking on Internal Switch by using VMBus
//! (hv_sock) instead of virtual network for NBD traffic.
//!
//! Host: AF_HYPERV listen → accept → forward to 127.0.0.1:NBD_PORT
//! Guest: socat TCP-LISTEN:10809 VSOCK-CONNECT:2:PORT → nbd-client localhost

#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
#[cfg(target_os = "windows")]
use tracing::{debug, info, warn};

#[cfg(target_os = "windows")]
const AF_HYPERV: i32 = 34;
#[cfg(target_os = "windows")]
const SHV_PROTO_RAW: i32 = 1;

/// GUID template for vsock port mapping: {XXXXXXXX-FACB-11E6-BD58-64006A7986D3}
/// where XXXXXXXX is the vsock port in little-endian hex.
#[cfg(target_os = "windows")]
pub fn vsock_port_to_service_guid(port: u32) -> String {
    format!(
        "{{{:08X}-FACB-11E6-BD58-64006A7986D3}}",
        port
    )
}

/// Register a vsock service GUID in the Windows registry so that
/// Hyper-V allows guest connections on this port.
#[cfg(target_os = "windows")]
pub fn register_vsock_service(port: u32) -> Result<()> {
    let guid = vsock_port_to_service_guid(port);
    let reg_path = format!(
        "HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Virtualization\\GuestCommunicationServices\\{}",
        guid
    );
    let output = std::process::Command::new("reg")
        .args(["add", &reg_path, "/v", "ElementName", "/t", "REG_SZ", "/d", "bcvk-nbd", "/f"])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to register vsock service {}: {}", guid, stderr.trim());
    }
    info!("registered vsock service: {} (port {})", guid, port);
    Ok(())
}

/// Start an AF_HYPERV listener that forwards connections to a local TCP port.
/// Returns a JoinHandle for the proxy task.
#[cfg(target_os = "windows")]
pub fn start_hvsock_proxy(
    vsock_port: u32,
    tcp_target: String,
    stop: Arc<Notify>,
) -> Result<tokio::task::JoinHandle<()>> {
    use std::os::windows::io::FromRawSocket;

    let guid = vsock_port_to_service_guid(vsock_port);
    info!("starting hv_sock proxy: vsock port {} ({}) → {}", vsock_port, guid, tcp_target);

    // Create AF_HYPERV socket using raw Windows API
    let raw_fd = unsafe {
        let mut wsa_data: winapi_WSAData = std::mem::zeroed();
        WSAStartup(0x0202, &mut wsa_data);
        socket(AF_HYPERV, SOCK_STREAM, SHV_PROTO_RAW)
    };
    if raw_fd == INVALID_SOCKET {
        bail!("failed to create AF_HYPERV socket");
    }

    // Build sockaddr_hyperv
    let service_guid = parse_guid(&guid)?;
    let addr = SockaddrHyperv {
        family: AF_HYPERV as u16,
        reserved: 0,
        vm_id: GUID_WILDCARD, // accept from any VM
        service_id: service_guid,
    };

    // Bind
    let ret = unsafe {
        bind(
            raw_fd,
            &addr as *const SockaddrHyperv as *const _,
            std::mem::size_of::<SockaddrHyperv>() as i32,
        )
    };
    if ret != 0 {
        bail!("failed to bind AF_HYPERV socket: error {}", unsafe { WSAGetLastError() });
    }

    // Listen
    let ret = unsafe { listen(raw_fd, 5) };
    if ret != 0 {
        bail!("failed to listen on AF_HYPERV socket: error {}", unsafe { WSAGetLastError() });
    }

    info!("hv_sock listening on vsock port {}", vsock_port);

    // Convert to tokio-compatible async using a background thread
    let handle = tokio::spawn(async move {
        loop {
            // accept in blocking mode on a dedicated thread
            let target = tcp_target.clone();
            let accepted = tokio::task::spawn_blocking(move || {
                let mut client_addr: SockaddrHyperv = unsafe { std::mem::zeroed() };
                let mut addr_len = std::mem::size_of::<SockaddrHyperv>() as i32;
                let client_fd = unsafe {
                    accept(
                        raw_fd,
                        &mut client_addr as *mut SockaddrHyperv as *mut _,
                        &mut addr_len,
                    )
                };
                if client_fd == INVALID_SOCKET {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(client_fd)
            })
            .await;

            match accepted {
                Ok(Ok(client_fd)) => {
                    info!("hv_sock connection accepted");
                    let target = tcp_target.clone();
                    tokio::spawn(async move {
                        let std_stream = unsafe {
                            std::net::TcpStream::from_raw_socket(client_fd as u64)
                        };
                        // hv_sock fd is NOT a TCP socket, can't use TcpStream directly.
                        // Use raw read/write instead.
                        handle_hvsock_connection(client_fd, &target).await;
                    });
                }
                Ok(Err(e)) => {
                    warn!("hv_sock accept error: {}", e);
                    break;
                }
                Err(e) => {
                    warn!("hv_sock accept task error: {}", e);
                    break;
                }
            }
        }
    });

    Ok(handle)
}

#[cfg(target_os = "windows")]
async fn handle_hvsock_connection(hvsock_fd: usize, tcp_target: &str) {
    use std::io::{Read, Write};

    // Connect to TCP target
    let tcp_stream = match std::net::TcpStream::connect(tcp_target) {
        Ok(s) => s,
        Err(e) => {
            warn!("hv_sock proxy: TCP connect to {} failed: {}", tcp_target, e);
            unsafe { closesocket(hvsock_fd); }
            return;
        }
    };

    let mut tcp_read = tcp_stream.try_clone().unwrap();
    let mut tcp_write = tcp_stream;

    // Bidirectional copy using blocking threads
    let fd1 = hvsock_fd;
    let fd2 = hvsock_fd;

    let h1 = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = unsafe {
                recv(fd1, buf.as_mut_ptr() as *mut _, buf.len() as i32, 0)
            };
            if n <= 0 { break; }
            if tcp_write.write_all(&buf[..n as usize]).is_err() { break; }
        }
    });

    let h2 = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            let n = match tcp_read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let sent = unsafe {
                send(fd2, buf.as_ptr() as *const _, n as i32, 0)
            };
            if sent <= 0 { break; }
        }
    });

    let _ = h1.join();
    let _ = h2.join();
    unsafe { closesocket(hvsock_fd); }
    debug!("hv_sock proxy connection closed");
}

// --- Raw Windows API bindings ---

#[cfg(target_os = "windows")]
const SOCK_STREAM: i32 = 1;
#[cfg(target_os = "windows")]
const INVALID_SOCKET: usize = usize::MAX;

#[cfg(target_os = "windows")]
#[repr(C)]
struct winapi_WSAData {
    version: u16,
    high_version: u16,
    max_sockets: u16,
    max_udp_dg: u16,
    vendor_info: *mut u8,
    description: [u8; 257],
    system_status: [u8; 129],
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy)]
struct GUID {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[cfg(target_os = "windows")]
const GUID_WILDCARD: GUID = GUID {
    data1: 0x00000000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
};

#[cfg(target_os = "windows")]
#[repr(C)]
struct SockaddrHyperv {
    family: u16,
    reserved: u16,
    vm_id: GUID,
    service_id: GUID,
}

#[cfg(target_os = "windows")]
fn parse_guid(s: &str) -> Result<GUID> {
    let s = s.trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        bail!("invalid GUID: {}", s);
    }
    let data1 = u32::from_str_radix(parts[0], 16)?;
    let data2 = u16::from_str_radix(parts[1], 16)?;
    let data3 = u16::from_str_radix(parts[2], 16)?;
    let mut data4 = [0u8; 8];
    let hex4 = format!("{}{}", parts[3], parts[4]);
    for i in 0..8 {
        data4[i] = u8::from_str_radix(&hex4[i * 2..i * 2 + 2], 16)?;
    }
    Ok(GUID { data1, data2, data3, data4 })
}

#[cfg(target_os = "windows")]
extern "system" {
    fn WSAStartup(version: u32, data: *mut winapi_WSAData) -> i32;
    fn WSAGetLastError() -> i32;
    fn socket(af: i32, socket_type: i32, protocol: i32) -> usize;
    fn bind(s: usize, addr: *const std::ffi::c_void, namelen: i32) -> i32;
    fn listen(s: usize, backlog: i32) -> i32;
    fn accept(s: usize, addr: *mut std::ffi::c_void, addrlen: *mut i32) -> usize;
    fn recv(s: usize, buf: *mut std::ffi::c_void, len: i32, flags: i32) -> i32;
    fn send(s: usize, buf: *const std::ffi::c_void, len: i32, flags: i32) -> i32;
    fn closesocket(s: usize) -> i32;
}
