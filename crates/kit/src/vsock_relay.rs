//! Hyper-V vsock relay: bridge hv_sock connections between two VMs.
//!
//! Accepts AF_VSOCK from ephemeral VM, dials AF_VSOCK to podman machine VM,
//! then relays data bidirectionally using blocking I/O threads.

#[cfg(target_os = "windows")]
use color_eyre::Result;
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
#[cfg(target_os = "windows")]
use tokio::task::JoinHandle;
#[cfg(target_os = "windows")]
use tracing::{debug, info, warn};

#[cfg(target_os = "windows")]
use std::io;
#[cfg(target_os = "windows")]
use std::mem;
#[cfg(target_os = "windows")]
use std::os::windows::io::RawSocket;

#[cfg(target_os = "windows")]
const AF_HYPERV: i32 = 34;
#[cfg(target_os = "windows")]
const HV_PROTOCOL_RAW: i32 = 1;
#[cfg(target_os = "windows")]
const RELAY_BUF_SIZE: usize = 256 * 1024;

#[cfg(target_os = "windows")]
#[repr(C)]
struct HvSockGuid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[cfg(target_os = "windows")]
#[repr(C)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: HvSockGuid,
    service_id: HvSockGuid,
}

#[cfg(target_os = "windows")]
fn vsock_service_id(port: u32) -> HvSockGuid {
    HvSockGuid {
        data1: port,
        data2: 0xFACB,
        data3: 0x11E6,
        data4: [0xBD, 0x58, 0x64, 0x00, 0x6A, 0x79, 0x86, 0xD3],
    }
}

#[cfg(target_os = "windows")]
fn wildcard_vm_id() -> HvSockGuid {
    HvSockGuid { data1: 0, data2: 0, data3: 0, data4: [0; 8] }
}

#[cfg(target_os = "windows")]
fn parse_vm_guid(s: &str) -> Result<HvSockGuid> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        color_eyre::eyre::bail!("invalid VM GUID: {}", s);
    }
    let data1 = u32::from_str_radix(parts[0], 16)?;
    let data2 = u16::from_str_radix(parts[1], 16)?;
    let data3 = u16::from_str_radix(parts[2], 16)?;
    let mut data4 = [0u8; 8];
    let hi = u16::from_str_radix(parts[3], 16)?;
    data4[0] = (hi >> 8) as u8;
    data4[1] = (hi & 0xFF) as u8;
    let lo_hex = parts[4];
    for i in 0..6 {
        data4[2 + i] = u8::from_str_radix(&lo_hex[i * 2..i * 2 + 2], 16)?;
    }
    Ok(HvSockGuid { data1, data2, data3, data4 })
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct VsockRelay {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

#[cfg(target_os = "windows")]
impl VsockRelay {
    pub async fn start(vsock_port: u32, podman_vm_guid: &str) -> Result<Self> {
        let target_guid = parse_vm_guid(podman_vm_guid)?;
        let stop = Arc::new(Notify::new());

        let listen_sock = unsafe { hvsock_listen(vsock_port)? };
        info!("vsock relay listening on port {}", vsock_port);

        let stop_clone = stop.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_clone.notified() => break,
                    result = tokio::task::spawn_blocking({
                        let ls = listen_sock;
                        move || unsafe { hvsock_accept(ls) }
                    }) => {
                        match result {
                            Ok(Ok(client_sock)) => {
                                info!("vsock relay: accepted connection from ephemeral VM");
                                let tg = HvSockGuid {
                                    data1: target_guid.data1,
                                    data2: target_guid.data2,
                                    data3: target_guid.data3,
                                    data4: target_guid.data4,
                                };
                                let port = vsock_port;
                                std::thread::spawn(move || {
                                    if let Err(e) = relay_connection(client_sock, &tg, port) {
                                        warn!("vsock relay error: {}", e);
                                    }
                                });
                            }
                            Ok(Err(e)) => {
                                warn!("vsock relay accept error: {}", e);
                                break;
                            }
                            Err(e) => {
                                warn!("vsock relay task error: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
            unsafe { closesocket(listen_sock); }
        });

        Ok(Self { stop, handle })
    }

    #[allow(dead_code)]
    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

#[cfg(target_os = "windows")]
impl Drop for VsockRelay {
    fn drop(&mut self) {
        self.stop.notify_waiters();
        self.handle.abort();
    }
}

#[cfg(target_os = "windows")]
fn relay_connection(client_sock: RawSocket, target_guid: &HvSockGuid, port: u32) -> Result<()> {
    let dial_sock = unsafe { hvsock_connect(target_guid, port)? };
    info!("vsock relay: connected to podman machine");

    let cs = client_sock;
    let ds = dial_sock;
    let t1 = std::thread::spawn(move || relay_data(cs, ds));

    let cs2 = client_sock;
    let ds2 = dial_sock;
    let t2 = std::thread::spawn(move || relay_data(ds2, cs2));

    let _ = t1.join();
    let _ = t2.join();

    unsafe {
        closesocket(client_sock);
        closesocket(dial_sock);
    }
    debug!("vsock relay: connection closed");
    Ok(())
}

#[cfg(target_os = "windows")]
fn relay_data(from: RawSocket, to: RawSocket) {
    let mut buf = vec![0u8; RELAY_BUF_SIZE];
    loop {
        let n = unsafe {
            recv(from, buf.as_mut_ptr(), buf.len() as i32, 0)
        };
        if n <= 0 { break; }
        let mut sent = 0i32;
        while sent < n {
            let w = unsafe {
                send(to, buf.as_ptr().add(sent as usize), n - sent, 0)
            };
            if w <= 0 { return; }
            sent += w;
        }
    }
}

// --- Raw Windows socket operations ---

#[cfg(target_os = "windows")]
extern "system" {
    fn socket(af: i32, sock_type: i32, protocol: i32) -> RawSocket;
    fn bind(s: RawSocket, name: *const u8, namelen: i32) -> i32;
    fn listen(s: RawSocket, backlog: i32) -> i32;
    fn accept(s: RawSocket, addr: *mut u8, addrlen: *mut i32) -> RawSocket;
    fn connect(s: RawSocket, name: *const u8, namelen: i32) -> i32;
    fn closesocket(s: RawSocket) -> i32;
    fn recv(s: RawSocket, buf: *mut u8, len: i32, flags: i32) -> i32;
    fn send(s: RawSocket, buf: *const u8, len: i32, flags: i32) -> i32;
    fn WSAGetLastError() -> i32;
    fn WSAStartup(version: u16, data: *mut [u8; 408]) -> i32;
}

#[cfg(target_os = "windows")]
static WSA_INIT: std::sync::Once = std::sync::Once::new();

#[cfg(target_os = "windows")]
fn ensure_wsa() {
    WSA_INIT.call_once(|| {
        let mut data = [0u8; 408];
        unsafe { WSAStartup(0x0202, &mut data); }
    });
}

#[cfg(target_os = "windows")]
unsafe fn hvsock_listen(port: u32) -> Result<RawSocket> {
    ensure_wsa();
    let sock = socket(AF_HYPERV, 1, HV_PROTOCOL_RAW);
    if sock == u64::MAX as RawSocket {
        return Err(io::Error::from_raw_os_error(WSAGetLastError()).into());
    }

    let addr = SockaddrHv {
        family: AF_HYPERV as u16,
        reserved: 0,
        vm_id: wildcard_vm_id(),
        service_id: vsock_service_id(port),
    };

    if bind(sock, &addr as *const SockaddrHv as *const u8,
            mem::size_of::<SockaddrHv>() as i32) != 0 {
        let err = WSAGetLastError();
        closesocket(sock);
        return Err(io::Error::from_raw_os_error(err).into());
    }

    if listen(sock, 16) != 0 {
        let err = WSAGetLastError();
        closesocket(sock);
        return Err(io::Error::from_raw_os_error(err).into());
    }

    Ok(sock)
}

#[cfg(target_os = "windows")]
unsafe fn hvsock_accept(listen_sock: RawSocket) -> Result<RawSocket> {
    let mut addrlen = mem::size_of::<SockaddrHv>() as i32;
    let mut addr = mem::zeroed::<SockaddrHv>();
    let sock = accept(listen_sock, &mut addr as *mut SockaddrHv as *mut u8, &mut addrlen);
    if sock == u64::MAX as RawSocket {
        return Err(io::Error::from_raw_os_error(WSAGetLastError()).into());
    }
    Ok(sock)
}

#[cfg(target_os = "windows")]
unsafe fn hvsock_connect(vm_guid: &HvSockGuid, port: u32) -> Result<RawSocket> {
    ensure_wsa();
    let sock = socket(AF_HYPERV, 1, HV_PROTOCOL_RAW);
    if sock == u64::MAX as RawSocket {
        return Err(io::Error::from_raw_os_error(WSAGetLastError()).into());
    }

    let addr = SockaddrHv {
        family: AF_HYPERV as u16,
        reserved: 0,
        vm_id: HvSockGuid {
            data1: vm_guid.data1,
            data2: vm_guid.data2,
            data3: vm_guid.data3,
            data4: vm_guid.data4,
        },
        service_id: vsock_service_id(port),
    };

    if connect(sock, &addr as *const SockaddrHv as *const u8,
               mem::size_of::<SockaddrHv>() as i32) != 0 {
        let err = WSAGetLastError();
        closesocket(sock);
        return Err(io::Error::from_raw_os_error(err).into());
    }

    Ok(sock)
}
