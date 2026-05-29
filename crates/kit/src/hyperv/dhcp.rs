//! Minimal DHCP server for ephemeral VM IP assignment on Windows.
//!
//! Single-client. Runs as an async task within bcvk.exe.
//! Uses IP_UNICAST_IF to bind outgoing broadcast to the correct vEthernet adapter.

use color_eyre::{eyre::bail, Result};
use std::os::windows::io::AsRawSocket;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tracing::{info, warn};
use windows::Win32::Networking::WinSock as ws;

const IP_UNICAST_IF: i32 = 31;

#[derive(Debug)]
pub struct DhcpServer {
    server_ip: [u8; 4],
    client_ip: [u8; 4],
    adapter_name: String,
    stop: Arc<Notify>,
}

impl DhcpServer {
    pub fn new(server_ip: &str, client_ip: &str, switch_name: &str) -> Result<Self> {
        Ok(Self {
            server_ip: parse_ip(server_ip)?,
            client_ip: parse_ip(client_ip)?,
            adapter_name: format!("vEthernet ({})", switch_name),
            stop: Arc::new(Notify::new()),
        })
    }

    pub fn start_background(&self) -> tokio::task::JoinHandle<()> {
        let sip = self.server_ip;
        let cip = self.client_ip;
        let adapter = self.adapter_name.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            if let Err(e) = run_dhcp(sip, cip, adapter, stop).await {
                warn!("DHCP server error: {}", e);
            }
        })
    }

    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

fn parse_ip(s: &str) -> Result<[u8; 4]> {
    let parts: Vec<u8> = s
        .split('.')
        .map(|p| p.parse::<u8>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        bail!("invalid IP: {}", s);
    }
    Ok([parts[0], parts[1], parts[2], parts[3]])
}

#[allow(unsafe_code)]
fn get_interface_index(adapter_name: &str) -> Result<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::NetworkManagement::IpHelper::{
        ConvertInterfaceAliasToLuid, ConvertInterfaceLuidToIndex,
    };
    use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;

    let wide: Vec<u16> = adapter_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut luid = NET_LUID_LH::default();
    unsafe {
        let ret = ConvertInterfaceAliasToLuid(PCWSTR(wide.as_ptr()), &mut luid);
        if ret != NO_ERROR {
            bail!("ConvertInterfaceAliasToLuid failed: {:?}", ret);
        }
        let mut index = 0u32;
        let ret = ConvertInterfaceLuidToIndex(&luid, &mut index);
        if ret != NO_ERROR {
            bail!("ConvertInterfaceLuidToIndex failed: {:?}", ret);
        }
        Ok(index)
    }
}

async fn run_dhcp(
    server_ip: [u8; 4],
    client_ip: [u8; 4],
    adapter_name: String,
    stop: Arc<Notify>,
) -> Result<()> {
    let bind_addr = format!(
        "{}.{}.{}.{}:67",
        server_ip[0], server_ip[1], server_ip[2], server_ip[3]
    );

    // Bind with retry (vEthernet adapter may not be ready yet)
    let mut std_sock = None;
    for _ in 0..120 {
        if let Ok(s) = std::net::UdpSocket::bind(&bind_addr) {
            std_sock = Some(s);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let std_sock = match std_sock {
        Some(s) => s,
        None => {
            warn!("DHCP: bind to {} failed after retries", bind_addr);
            std::net::UdpSocket::bind(&bind_addr)?
        }
    };
    std_sock.set_broadcast(true)?;

    // Bind outgoing broadcast to the correct vEthernet adapter via IP_UNICAST_IF
    #[allow(unsafe_code)]
    match get_interface_index(&adapter_name) {
        Ok(if_index) => {
            let optval = if_index.to_be_bytes();
            unsafe {
                ws::setsockopt(
                    ws::SOCKET(std_sock.as_raw_socket() as usize),
                    0, // IPPROTO_IP
                    IP_UNICAST_IF,
                    Some(&optval),
                );
            }
            info!(
                "DHCP: bound to interface '{}' (index {})",
                adapter_name, if_index
            );
        }
        Err(e) => {
            warn!(
                "DHCP: failed to get interface index for '{}': {}, broadcast may go to wrong interface",
                adapter_name, e
            );
        }
    }

    // Convert to async tokio socket
    std_sock.set_nonblocking(true)?;
    let sock = UdpSocket::from_std(std_sock)?;
    info!("DHCP listening on {}", bind_addr);

    let mut buf = vec![0u8; 1500];
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            result = sock.recv_from(&mut buf) => {
                let (len, _from) = result?;
                let data = &buf[..len];
                if data[0] != 1 || len < 240 { continue; }

                let xid = &data[4..8];
                let chaddr = &data[28..44];
                let msg_type = find_dhcp_option(data, 53).and_then(|v| v.first().copied()).unwrap_or(0);

                if msg_type == 1 || msg_type == 3 {
                    let resp_type = if msg_type == 1 { 2u8 } else { 5u8 };
                    let resp = build_dhcp_response(xid, chaddr, &server_ip, &client_ip, resp_type);
                    let bcast = format!("{}.{}.{}.255:68", server_ip[0], server_ip[1], server_ip[2]);
                    sock.send_to(&resp, &bcast).await?;
                    let mac = format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                        chaddr[0], chaddr[1], chaddr[2], chaddr[3], chaddr[4], chaddr[5]);
                    let type_name = if msg_type == 1 { "OFFER" } else { "ACK" };
                    info!("DHCP {} → {}.{}.{}.{} ({})", type_name, client_ip[0], client_ip[1], client_ip[2], client_ip[3], mac);
                }
            }
        }
    }
    Ok(())
}

fn build_dhcp_response(
    xid: &[u8],
    chaddr: &[u8],
    server_ip: &[u8; 4],
    client_ip: &[u8; 4],
    msg_type: u8,
) -> Vec<u8> {
    let mut resp = vec![0u8; 512];
    resp[0] = 2;
    resp[1] = 1;
    resp[2] = 6;
    resp[4..8].copy_from_slice(xid);
    resp[16..20].copy_from_slice(client_ip);
    resp[20..24].copy_from_slice(server_ip);
    resp[28..44].copy_from_slice(chaddr);
    resp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    let mut i = 240;
    resp[i] = 53;
    resp[i + 1] = 1;
    resp[i + 2] = msg_type;
    i += 3;
    resp[i] = 1;
    resp[i + 1] = 4;
    resp[i + 2..i + 6].copy_from_slice(&[255, 255, 255, 0]);
    i += 6;
    resp[i] = 3;
    resp[i + 1] = 4;
    resp[i + 2..i + 6].copy_from_slice(server_ip);
    i += 6;
    // Option 6: DNS (Google Public DNS)
    resp[i] = 6;
    resp[i + 1] = 8;
    resp[i + 2..i + 6].copy_from_slice(&[8, 8, 8, 8]);
    resp[i + 6..i + 10].copy_from_slice(&[8, 8, 4, 4]);
    i += 10;
    resp[i] = 54;
    resp[i + 1] = 4;
    resp[i + 2..i + 6].copy_from_slice(server_ip);
    i += 6;
    resp[i] = 51;
    resp[i + 1] = 4;
    resp[i + 2..i + 6].copy_from_slice(&[0, 0, 14, 16]);
    i += 6;
    resp[i] = 255;
    i += 1;
    // Pad to BOOTP minimum (300 bytes)
    resp.truncate(i.max(300));
    resp
}

fn find_dhcp_option(data: &[u8], option: u8) -> Option<Vec<u8>> {
    let mut i = 240;
    while i < data.len() && data[i] != 255 {
        let opt = data[i];
        if i + 1 >= data.len() {
            break;
        }
        let len = data[i + 1] as usize;
        if opt == option && i + 2 + len <= data.len() {
            return Some(data[i + 2..i + 2 + len].to_vec());
        }
        i += 2 + len;
    }
    None
}
