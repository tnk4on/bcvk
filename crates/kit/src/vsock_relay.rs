//! Hyper-V vsock relay: bridge hv_sock connections between two VMs.
//!
//! Connects to both ephemeral VM and podman machine VM using Host-initiated
//! AF_HYPERV connections (fast path: ~1 GB/s), then relays data bidirectionally.

#[cfg(target_os = "windows")]
use color_eyre::Result;
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
#[cfg(target_os = "windows")]
use tokio::task::JoinHandle;
#[cfg(target_os = "windows")]
use tracing::{debug, info};

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
struct WsaBuf {
    len: u32,
    buf: *mut u8,
}

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
impl HvSockGuid {
    fn clone(&self) -> Self {
        HvSockGuid {
            data1: self.data1,
            data2: self.data2,
            data3: self.data3,
            data4: self.data4,
        }
    }
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct VsockRelay {
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

#[cfg(target_os = "windows")]
impl VsockRelay {
    pub async fn start(
        vsock_port: u32,
        podman_vm_guid: &str,
        ephemeral_vm_guid: &str,
    ) -> Result<Self> {
        let podman_guid = parse_vm_guid(podman_vm_guid)?;
        let ephemeral_guid = parse_vm_guid(ephemeral_vm_guid)?;
        let stop = Arc::new(Notify::new());

        info!("vsock relay: connecting to podman machine (port {})", vsock_port);
        let podman_sock = unsafe { hvsock_connect(&podman_guid, vsock_port)? };
        info!("vsock relay: connected to podman machine");

        info!("vsock relay: connecting to ephemeral VM (port {}, with retry)", vsock_port);
        let vm_sock = unsafe { hvsock_connect_retry(&ephemeral_guid, vsock_port, 30, 2)? };
        info!("vsock relay: connected to ephemeral VM");

        let stop_clone = stop.clone();
        let handle = tokio::spawn(async move {
            let ps = podman_sock;
            let vs = vm_sock;

            let relay_task = tokio::task::spawn_blocking(move || {
                relay_connection(vs, ps);
            });

            tokio::select! {
                _ = stop_clone.notified() => {
                    unsafe {
                        closesocket(podman_sock);
                        closesocket(vm_sock);
                    }
                }
                _ = relay_task => {
                    debug!("vsock relay: connection finished");
                }
            }
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
fn relay_connection(sock_a: RawSocket, sock_b: RawSocket) {
    let sa = sock_a;
    let sb = sock_b;
    let t1 = std::thread::spawn(move || relay_data(sa, sb));

    let sa2 = sock_a;
    let sb2 = sock_b;
    let t2 = std::thread::spawn(move || relay_data(sb2, sa2));

    let _ = t1.join();
    let _ = t2.join();

    unsafe {
        closesocket(sock_a);
        closesocket(sock_b);
    }
    debug!("vsock relay: connection closed");
}

#[cfg(target_os = "windows")]
fn relay_data(from: RawSocket, to: RawSocket) {
    let mut buf = vec![0u8; RELAY_BUF_SIZE];
    loop {
        let mut bytes_recv: u32 = 0;
        let mut flags: u32 = 0;
        let mut wsa_buf = WsaBuf { len: buf.len() as u32, buf: buf.as_mut_ptr() };
        let rc = unsafe {
            WSARecv(from, &mut wsa_buf, 1, &mut bytes_recv, &mut flags, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if rc != 0 || bytes_recv == 0 { break; }
        let n = bytes_recv as usize;
        let mut sent: usize = 0;
        while sent < n {
            let mut bytes_sent: u32 = 0;
            let mut send_buf = WsaBuf { len: (n - sent) as u32, buf: buf.as_mut_ptr().wrapping_add(sent) };
            let rc = unsafe {
                WSASend(to, &mut send_buf, 1, &mut bytes_sent, 0, std::ptr::null_mut(), std::ptr::null_mut())
            };
            if rc != 0 || bytes_sent == 0 { return; }
            sent += bytes_sent as usize;
        }
    }
}

// --- Raw Windows socket operations ---

#[cfg(target_os = "windows")]
extern "system" {
    fn socket(af: i32, sock_type: i32, protocol: i32) -> RawSocket;
    fn connect(s: RawSocket, name: *const u8, namelen: i32) -> i32;
    fn setsockopt(s: RawSocket, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
    fn closesocket(s: RawSocket) -> i32;
    fn WSARecv(s: RawSocket, bufs: *mut WsaBuf, buf_count: u32, bytes_recv: *mut u32, flags: *mut u32, overlapped: *mut u8, completion: *mut u8) -> i32;
    fn WSASend(s: RawSocket, bufs: *mut WsaBuf, buf_count: u32, bytes_sent: *mut u32, flags: u32, overlapped: *mut u8, completion: *mut u8) -> i32;
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
unsafe fn hvsock_connect(vm_guid: &HvSockGuid, port: u32) -> Result<RawSocket> {
    ensure_wsa();
    let sock = socket(AF_HYPERV, 1, HV_PROTOCOL_RAW);
    if sock == u64::MAX as RawSocket {
        return Err(io::Error::from_raw_os_error(WSAGetLastError()).into());
    }

    let addr = SockaddrHv {
        family: AF_HYPERV as u16,
        reserved: 0,
        vm_id: vm_guid.clone(),
        service_id: vsock_service_id(port),
    };

    if connect(sock, &addr as *const SockaddrHv as *const u8,
               mem::size_of::<SockaddrHv>() as i32) != 0 {
        let err = WSAGetLastError();
        closesocket(sock);
        return Err(io::Error::from_raw_os_error(err).into());
    }

    let sockbuf: i32 = 1024 * 1024;
    setsockopt(sock, 0xFFFF, 0x1001, &sockbuf as *const i32 as *const u8, 4); // SO_SNDBUF
    setsockopt(sock, 0xFFFF, 0x1002, &sockbuf as *const i32 as *const u8, 4); // SO_RCVBUF

    Ok(sock)
}

#[cfg(target_os = "windows")]
unsafe fn hvsock_connect_retry(
    vm_guid: &HvSockGuid,
    port: u32,
    max_attempts: u32,
    interval_secs: u64,
) -> Result<RawSocket> {
    for attempt in 1..=max_attempts {
        match hvsock_connect(vm_guid, port) {
            Ok(sock) => return Ok(sock),
            Err(e) => {
                if attempt == max_attempts {
                    return Err(color_eyre::eyre::eyre!(
                        "vsock relay: failed to connect to ephemeral VM after {} attempts: {}",
                        max_attempts, e
                    ));
                }
                debug!("vsock relay: connect attempt {}/{} failed ({}), retrying in {}s",
                    attempt, max_attempts, e, interval_secs);
                std::thread::sleep(std::time::Duration::from_secs(interval_secs));
            }
        }
    }
    unreachable!()
}
