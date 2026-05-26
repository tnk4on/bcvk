//! Hyper-V vsock relay with NBD caching: bridge hv_sock connections between two VMs.
//!
//! Parses NBD protocol to cache read responses on the Host side.
//! Cache hits are served directly (2 VMBus hops instead of 4),
//! improving throughput from ~400 MB/s to ~900 MB/s for cached data.
//!
//! ## Architecture: Host-initiated connections
//!
//! The relay initiates connections FROM the Host TO both VMs (podman + ephemeral).
//! This is critical for performance: Hyper-V hv_sock has a ~10x throughput asymmetry
//! based on connection direction (Guest-initiated ~120 MB/s vs Host-initiated ~1 GB/s)
//! due to VMBus ring buffer handling differences.
//!
//! ## Data path
//!
//! ```text
//! Cache miss (4 VMBus hops):
//!   VM → Host relay → podman/nbdkit → Host relay → VM
//!
//! Cache hit (2 VMBus hops):
//!   VM → Host relay (HashMap lookup) → VM
//! ```
//!
//! The prefetch thread pre-populates the cache with the first 256 MB (boot-critical
//! regions) via a dedicated podman connection, running in parallel with relay setup.

use color_eyre::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info};

use std::io;
use std::mem;
use std::os::windows::io::RawSocket;
use std::sync::{Mutex, RwLock};

const AF_HYPERV: i32 = 34;
const HV_PROTOCOL_RAW: i32 = 1;
const SOCKET_BUF_SIZE: i32 = 4 * 1024 * 1024;

const NBD_REQUEST_MAGIC: u32 = 0x25609513;
const NBD_REPLY_MAGIC: u32 = 0x67446698;
const NBD_CMD_READ: u16 = 0;

#[repr(C)]
struct WsaBuf {
    len: u32,
    buf: *mut u8,
}

#[repr(C)]
#[derive(Clone)]
struct HvSockGuid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

#[repr(C)]
struct SockaddrHv {
    family: u16,
    reserved: u16,
    vm_id: HvSockGuid,
    service_id: HvSockGuid,
}

fn vsock_service_id(port: u32) -> HvSockGuid {
    HvSockGuid {
        data1: port,
        data2: 0xFACB,
        data3: 0x11E6,
        data4: [0xBD, 0x58, 0x64, 0x00, 0x6A, 0x79, 0x86, 0xD3],
    }
}

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
    Ok(HvSockGuid {
        data1,
        data2,
        data3,
        data4,
    })
}

// --- NBD cache ---

type NbdCache = Arc<RwLock<HashMap<u64, Vec<u8>>>>;
type VmSockLock = Arc<Mutex<RawSocket>>;

fn cache_new() -> NbdCache {
    Arc::new(RwLock::new(HashMap::new()))
}

// --- Socket helpers ---

fn wsa_recv_exact(sock: RawSocket, buf: &mut [u8]) -> bool {
    let mut done: usize = 0;
    while done < buf.len() {
        let mut bytes_recv: u32 = 0;
        let mut flags: u32 = 0;
        let mut wsa_buf = WsaBuf {
            len: (buf.len() - done) as u32,
            buf: buf.as_mut_ptr().wrapping_add(done),
        };
        let rc = unsafe {
            WSARecv(
                sock,
                &mut wsa_buf,
                1,
                &mut bytes_recv,
                &mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 || bytes_recv == 0 {
            return false;
        }
        done += bytes_recv as usize;
    }
    true
}

fn wsa_send_all(sock: RawSocket, buf: &[u8]) -> bool {
    let mut done: usize = 0;
    while done < buf.len() {
        let mut bytes_sent: u32 = 0;
        let mut wsa_buf = WsaBuf {
            len: (buf.len() - done) as u32,
            buf: buf.as_ptr().wrapping_add(done) as *mut u8,
        };
        let rc = unsafe {
            WSASend(
                sock,
                &mut wsa_buf,
                1,
                &mut bytes_sent,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if rc != 0 || bytes_sent == 0 {
            return false;
        }
        done += bytes_sent as usize;
    }
    true
}

// --- Prefetch: read all sectors from podman into cache ---

/// Prefetch boot-critical regions: GPT + ESP + EROFS metadata (~256 MB)
/// Remaining sectors are cached on-demand as VM reads them.
fn prefetch_boot_regions(podman_sock: RawSocket, cache: NbdCache) {
    // Handshake
    let mut hs = [0u8; 18];
    if !wsa_recv_exact(podman_sock, &mut hs) {
        return;
    }
    let mut client_hs = [0u8; 20];
    client_hs[0..4].copy_from_slice(&1u32.to_be().to_ne_bytes());
    client_hs[4..12].copy_from_slice(&0x49484156454F5054u64.to_be().to_ne_bytes());
    client_hs[12..16].copy_from_slice(&1u32.to_be().to_ne_bytes());
    client_hs[16..20].copy_from_slice(&0u32.to_be().to_ne_bytes());
    if !wsa_send_all(podman_sock, &client_hs) {
        return;
    }
    let mut reply = [0u8; 134];
    if !wsa_recv_exact(podman_sock, &mut reply) {
        return;
    }
    let export_size = u64::from_be_bytes(reply[0..8].try_into().unwrap());

    // Prefetch first 256 MB (GPT + ESP + EROFS metadata + early rootfs files)
    const PREFETCH_LIMIT: u64 = 256 * 1024 * 1024;
    let prefetch_end = std::cmp::min(PREFETCH_LIMIT, export_size);
    let chunk_size: u32 = 512 * 1024;
    let mut offset: u64 = 0;
    let mut cached_bytes: u64 = 0;
    let start = std::time::Instant::now();

    while offset < prefetch_end {
        let len = std::cmp::min(chunk_size as u64, prefetch_end - offset) as u32;
        let mut req = [0u8; 28];
        req[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        req[8..16].copy_from_slice(&offset.to_be_bytes());
        req[16..24].copy_from_slice(&offset.to_be_bytes());
        req[24..28].copy_from_slice(&len.to_be_bytes());
        if !wsa_send_all(podman_sock, &req) {
            break;
        }

        let mut reply_hdr = [0u8; 16];
        if !wsa_recv_exact(podman_sock, &mut reply_hdr) {
            break;
        }
        if u32::from_be_bytes(reply_hdr[4..8].try_into().unwrap()) != 0 {
            break;
        }

        let mut data = vec![0u8; len as usize];
        if !wsa_recv_exact(podman_sock, &mut data) {
            break;
        }

        cache.write().unwrap().insert(offset, data);
        cached_bytes += len as u64;
        offset += len as u64;
    }

    let elapsed = start.elapsed();
    let speed = if elapsed.as_secs_f64() > 0.0 {
        (cached_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64()
    } else {
        0.0
    };

    info!(
        "prefetch: boot regions {} MB in {:.1}s ({:.0} MB/s), {} entries",
        cached_bytes / (1024 * 1024),
        elapsed.as_secs_f64(),
        speed,
        cache.read().unwrap().len()
    );

    unsafe {
        closesocket(podman_sock);
    }
}

// --- NBD-aware caching relay with request tracking ---

struct PendingRead {
    offset: u64,
    length: usize,
}

type PendingMap = Arc<RwLock<HashMap<u64, PendingRead>>>;

fn relay_vm_to_podman_tracked(
    vm_sock: RawSocket,
    podman_sock: RawSocket,
    cache: NbdCache,
    pending: PendingMap,
    vm_write: VmSockLock,
) {
    let mut handshake = [0u8; 20];
    if !wsa_recv_exact(vm_sock, &mut handshake) {
        return;
    }
    if !wsa_send_all(podman_sock, &handshake) {
        return;
    }

    loop {
        let mut hdr = [0u8; 28];
        if !wsa_recv_exact(vm_sock, &mut hdr) {
            break;
        }

        let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
        if magic != NBD_REQUEST_MAGIC {
            if !wsa_send_all(podman_sock, &hdr) {
                break;
            }
            continue;
        }

        let cmd = u16::from_be_bytes(hdr[6..8].try_into().unwrap());
        let handle = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
        let offset = u64::from_be_bytes(hdr[16..24].try_into().unwrap());
        let length = u32::from_be_bytes(hdr[24..28].try_into().unwrap()) as usize;

        if cmd == NBD_CMD_READ {
            let cache_hit = cache.read().unwrap().get(&offset).and_then(|d| {
                if d.len() == length {
                    Some(d.clone())
                } else {
                    None
                }
            });
            if let Some(cached_data) = cache_hit {
                let mut reply_buf = vec![0u8; 16 + length];
                reply_buf[0..4].copy_from_slice(&NBD_REPLY_MAGIC.to_be_bytes());
                reply_buf[8..16].copy_from_slice(&handle.to_be_bytes());
                reply_buf[16..].copy_from_slice(&cached_data);
                let sock = vm_write.lock().unwrap();
                if !wsa_send_all(*sock, &reply_buf) {
                    break;
                }
                drop(sock);
                continue;
            }
            pending
                .write()
                .unwrap()
                .insert(handle, PendingRead { offset, length });
        }

        if !wsa_send_all(podman_sock, &hdr) {
            break;
        }

        if cmd == 1 {
            let mut data = vec![0u8; length];
            if !wsa_recv_exact(vm_sock, &mut data) {
                break;
            }
            if !wsa_send_all(podman_sock, &data) {
                break;
            }
        }
    }
}

fn relay_podman_to_vm_tracked(
    podman_sock: RawSocket,
    cache: NbdCache,
    pending: PendingMap,
    vm_write: VmSockLock,
) {
    let mut hs1 = [0u8; 18];
    if !wsa_recv_exact(podman_sock, &mut hs1) {
        return;
    }
    {
        let sock = vm_write.lock().unwrap();
        if !wsa_send_all(*sock, &hs1) {
            return;
        }
    }
    let mut hs2 = [0u8; 134];
    if !wsa_recv_exact(podman_sock, &mut hs2) {
        return;
    }
    {
        let sock = vm_write.lock().unwrap();
        if !wsa_send_all(*sock, &hs2) {
            return;
        }
    }

    loop {
        let mut reply_hdr = [0u8; 16];
        if !wsa_recv_exact(podman_sock, &mut reply_hdr) {
            break;
        }

        let magic = u32::from_be_bytes(reply_hdr[0..4].try_into().unwrap());
        if magic != NBD_REPLY_MAGIC {
            let sock = vm_write.lock().unwrap();
            if !wsa_send_all(*sock, &reply_hdr) {
                break;
            }
            continue;
        }

        let error = u32::from_be_bytes(reply_hdr[4..8].try_into().unwrap());
        let handle = u64::from_be_bytes(reply_hdr[8..16].try_into().unwrap());

        let pending_read = pending.write().unwrap().remove(&handle);

        if let Some(pr) = pending_read {
            if error == 0 {
                let mut data = vec![0u8; pr.length];
                if !wsa_recv_exact(podman_sock, &mut data) {
                    break;
                }

                cache.write().unwrap().insert(pr.offset, data.clone());

                let mut reply_buf = vec![0u8; 16 + pr.length];
                reply_buf[0..16].copy_from_slice(&reply_hdr);
                reply_buf[16..].copy_from_slice(&data);
                let sock = vm_write.lock().unwrap();
                if !wsa_send_all(*sock, &reply_buf) {
                    break;
                }
            } else {
                let sock = vm_write.lock().unwrap();
                if !wsa_send_all(*sock, &reply_hdr) {
                    break;
                }
            }
        } else {
            let sock = vm_write.lock().unwrap();
            if !wsa_send_all(*sock, &reply_hdr) {
                break;
            }
        }
    }
}

// --- Public API ---

#[derive(Debug)]
pub struct VsockRelay {
    stop: Arc<Notify>,
    handles: Vec<JoinHandle<()>>,
}

impl VsockRelay {
    pub async fn start(
        vsock_port: u32,
        num_connections: u32,
        podman_vm_guid: &str,
        ephemeral_vm_guid: &str,
    ) -> Result<Self> {
        let podman_guid = parse_vm_guid(podman_vm_guid)?;
        let ephemeral_guid = parse_vm_guid(ephemeral_vm_guid)?;
        let stop = Arc::new(Notify::new());
        let mut handles = Vec::new();
        let cache = cache_new();

        // Prefetch boot regions in background (with retry, parallel with relay)
        {
            let pod_prefetch = podman_guid.clone();
            let cache_prefetch = cache.clone();
            tokio::task::spawn_blocking(move || {
                info!("vsock relay: prefetching boot regions from podman");
                let podman_sock =
                    match unsafe { hvsock_connect_retry(&pod_prefetch, vsock_port, 75, 200) } {
                        Ok(s) => s,
                        Err(e) => {
                            info!("vsock relay: prefetch connect failed: {}", e);
                            return;
                        }
                    };
                prefetch_boot_regions(podman_sock, cache_prefetch);
            });
            // NOT awaited — runs in parallel with relay connections
        }

        let mut connect_handles = Vec::new();
        for i in 0..num_connections {
            let pod_g = podman_guid.clone();
            let eph_g = ephemeral_guid.clone();
            let stop_clone = stop.clone();
            let cache_clone = cache.clone();
            connect_handles.push(tokio::task::spawn_blocking(
                move || -> Option<JoinHandle<()>> {
                    info!(
                        "vsock relay[{}]: connecting to podman machine (port {})",
                        i, vsock_port
                    );
                    let podman_sock =
                        match unsafe { hvsock_connect_retry(&pod_g, vsock_port, 75, 200) } {
                            Ok(s) => s,
                            Err(e) => {
                                info!("vsock relay[{}]: podman connect failed: {}", i, e);
                                return None;
                            }
                        };
                    info!(
                        "vsock relay[{}]: connecting to ephemeral VM (port {}, with retry)",
                        i, vsock_port
                    );
                    let vm_sock =
                        match unsafe { hvsock_connect_retry(&eph_g, vsock_port, 300, 200) } {
                            Ok(s) => s,
                            Err(e) => {
                                info!("vsock relay[{}]: VM connect failed: {}", i, e);
                                unsafe {
                                    closesocket(podman_sock);
                                }
                                return None;
                            }
                        };
                    info!("vsock relay[{}]: connected both sides", i);
                    let idx = i;
                    let sc = stop_clone;
                    let cc = cache_clone;
                    let pod_g2 = pod_g.clone();
                    let eph_g2 = eph_g.clone();
                    Some(tokio::spawn(async move {
                        let relay_task = tokio::task::spawn_blocking(move || {
                            relay_one_connection_cached(vm_sock, podman_sock, cc.clone());
                            // Connection dropped — attempt one reconnect (ublk→NBD failover)
                            info!(
                                "vsock relay[{}]: connection closed, attempting reconnect",
                                idx
                            );
                            let new_podman =
                                match unsafe { hvsock_connect_retry(&pod_g2, vsock_port, 30, 200) }
                                {
                                    Ok(s) => s,
                                    Err(e) => {
                                        info!(
                                            "vsock relay[{}]: podman reconnect failed: {}",
                                            idx, e
                                        );
                                        return;
                                    }
                                };
                            let new_vm =
                                match unsafe { hvsock_connect_retry(&eph_g2, vsock_port, 50, 200) }
                                {
                                    Ok(s) => s,
                                    Err(e) => {
                                        info!("vsock relay[{}]: VM reconnect failed: {}", idx, e);
                                        unsafe {
                                            closesocket(new_podman);
                                        }
                                        return;
                                    }
                                };
                            info!("vsock relay[{}]: reconnected (failover)", idx);
                            relay_one_connection_cached(new_vm, new_podman, cc);
                        });
                        tokio::select! {
                            _ = sc.notified() => { debug!("vsock relay[{}]: stop requested", idx); }
                            _ = relay_task => { debug!("vsock relay[{}]: relay finished", idx); }
                        }
                    }))
                },
            ));
        }

        for ch in connect_handles {
            if let Ok(Some(h)) = ch.await {
                handles.push(h);
            }
        }

        info!(
            "vsock relay: {} connections established (NBD caching enabled)",
            handles.len()
        );

        Ok(Self { stop, handles })
    }

    #[allow(dead_code)]
    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

impl Drop for VsockRelay {
    fn drop(&mut self) {
        self.stop.notify_waiters();
        for h in &self.handles {
            h.abort();
        }
    }
}

fn relay_one_connection_cached(vm_sock: RawSocket, podman_sock: RawSocket, cache: NbdCache) {
    let pending: PendingMap = Arc::new(RwLock::new(HashMap::new()));
    let vm_write: VmSockLock = Arc::new(Mutex::new(vm_sock));

    let cache_req = cache.clone();
    let pending_req = pending.clone();
    let vm_w1 = vm_write.clone();
    let t1 = std::thread::spawn(move || {
        relay_vm_to_podman_tracked(vm_sock, podman_sock, cache_req, pending_req, vm_w1);
    });

    let cache_reply = cache.clone();
    let pending_reply = pending.clone();
    let vm_w2 = vm_write.clone();
    let t2 = std::thread::spawn(move || {
        relay_podman_to_vm_tracked(podman_sock, cache_reply, pending_reply, vm_w2);
    });

    let _ = t1.join();
    let _ = t2.join();

    unsafe {
        closesocket(vm_sock);
        closesocket(podman_sock);
    }
}

// --- Raw Windows socket operations ---

extern "system" {
    fn socket(af: i32, sock_type: i32, protocol: i32) -> RawSocket;
    fn connect(s: RawSocket, name: *const u8, namelen: i32) -> i32;
    fn setsockopt(s: RawSocket, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
    fn closesocket(s: RawSocket) -> i32;
    fn WSARecv(
        s: RawSocket,
        bufs: *mut WsaBuf,
        buf_count: u32,
        bytes_recv: *mut u32,
        flags: *mut u32,
        overlapped: *mut u8,
        completion: *mut u8,
    ) -> i32;
    fn WSASend(
        s: RawSocket,
        bufs: *mut WsaBuf,
        buf_count: u32,
        bytes_sent: *mut u32,
        flags: u32,
        overlapped: *mut u8,
        completion: *mut u8,
    ) -> i32;
    fn WSAGetLastError() -> i32;
    fn ioctlsocket(s: RawSocket, cmd: i32, argp: *mut u32) -> i32;
    fn select(
        nfds: i32,
        readfds: *mut u8,
        writefds: *mut u8,
        exceptfds: *mut u8,
        timeout: *const [i64; 2],
    ) -> i32;
    fn WSAStartup(version: u16, data: *mut [u8; 408]) -> i32;
}

static WSA_INIT: std::sync::Once = std::sync::Once::new();

fn ensure_wsa() {
    WSA_INIT.call_once(|| {
        let mut data = [0u8; 408];
        unsafe {
            WSAStartup(0x0202, &mut data);
        }
    });
}

unsafe fn hvsock_connect(vm_guid: &HvSockGuid, port: u32) -> Result<RawSocket> {
    ensure_wsa();
    let sock = socket(AF_HYPERV, 1, HV_PROTOCOL_RAW);
    if sock == u64::MAX as RawSocket {
        return Err(io::Error::from_raw_os_error(WSAGetLastError()).into());
    }
    // Set non-blocking for connect with timeout
    let mut nonblock: u32 = 1;
    ioctlsocket(sock, 0x8004667E_u32 as i32, &mut nonblock); // FIONBIO

    let addr = SockaddrHv {
        family: AF_HYPERV as u16,
        reserved: 0,
        vm_id: vm_guid.clone(),
        service_id: vsock_service_id(port),
    };
    let rc = connect(
        sock,
        &addr as *const SockaddrHv as *const u8,
        mem::size_of::<SockaddrHv>() as i32,
    );
    if rc != 0 {
        let err = WSAGetLastError();
        const WSAEWOULDBLOCK: i32 = 10035;
        if err != WSAEWOULDBLOCK {
            closesocket(sock);
            return Err(io::Error::from_raw_os_error(err).into());
        }
        // Wait for connect with 1s timeout using select(writefds)
        // fd_set: first u32 = count, then socket handles (up to 64)
        let mut wfds = [0u8; 264]; // fd_set = u32 count + 64 * SOCKET
        let count_ptr = wfds.as_mut_ptr() as *mut u32;
        *count_ptr = 1;
        let sock_ptr = wfds.as_mut_ptr().add(mem::size_of::<usize>()) as *mut RawSocket;
        *sock_ptr = sock;
        let timeout: [i64; 2] = [1, 0]; // 1 second, 0 microseconds
        let n = select(
            0,
            std::ptr::null_mut(),
            wfds.as_mut_ptr(),
            std::ptr::null_mut(),
            &timeout,
        );
        if n <= 0 {
            closesocket(sock);
            return Err(color_eyre::eyre::eyre!("connect timed out (1s)"));
        }
    }

    // Restore blocking mode
    let mut blocking: u32 = 0;
    ioctlsocket(sock, 0x8004667E_u32 as i32, &mut blocking);

    let sockbuf: i32 = SOCKET_BUF_SIZE;
    setsockopt(sock, 0xFFFF, 0x1001, &sockbuf as *const i32 as *const u8, 4);
    setsockopt(sock, 0xFFFF, 0x1002, &sockbuf as *const i32 as *const u8, 4);
    Ok(sock)
}

unsafe fn hvsock_connect_retry(
    vm_guid: &HvSockGuid,
    port: u32,
    max_attempts: u32,
    interval_ms: u64,
) -> Result<RawSocket> {
    for attempt in 1..=max_attempts {
        match hvsock_connect(vm_guid, port) {
            Ok(sock) => return Ok(sock),
            Err(e) => {
                if attempt == max_attempts {
                    return Err(color_eyre::eyre::eyre!(
                        "vsock relay: failed to connect after {} attempts: {}",
                        max_attempts,
                        e
                    ));
                }
                debug!(
                    "vsock relay: attempt {}/{} failed ({}), retrying in {}ms",
                    attempt, max_attempts, e, interval_ms
                );
                std::thread::sleep(std::time::Duration::from_millis(interval_ms));
            }
        }
    }
    unreachable!()
}
