//! Minimal DHCP server for ephemeral VM IP assignment on Windows.
//!
//! Single-client. Runs as an async task within bcvk.exe.

#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use tokio::net::UdpSocket;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
#[cfg(target_os = "windows")]
use tracing::{info, warn};

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct DhcpServer {
    server_ip: [u8; 4],
    client_ip: [u8; 4],
    stop: Arc<Notify>,
}

#[cfg(target_os = "windows")]
impl DhcpServer {
    pub fn new(server_ip: &str, client_ip: &str) -> Result<Self> {
        Ok(Self {
            server_ip: parse_ip(server_ip)?,
            client_ip: parse_ip(client_ip)?,
            stop: Arc::new(Notify::new()),
        })
    }

    pub fn start_background(&self) -> tokio::task::JoinHandle<()> {
        let sip = self.server_ip;
        let cip = self.client_ip;
        let stop = self.stop.clone();
        tokio::spawn(async move {
            if let Err(e) = run_dhcp(sip, cip, stop).await {
                warn!("DHCP server error: {}", e);
            }
        })
    }

    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

#[cfg(target_os = "windows")]
fn parse_ip(s: &str) -> Result<[u8; 4]> {
    let parts: Vec<u8> = s.split('.').map(|p| p.parse::<u8>()).collect::<std::result::Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        bail!("invalid IP: {}", s);
    }
    Ok([parts[0], parts[1], parts[2], parts[3]])
}

#[cfg(target_os = "windows")]
async fn run_dhcp(server_ip: [u8; 4], client_ip: [u8; 4], stop: Arc<Notify>) -> Result<()> {
    let bind_addr = format!("{}.{}.{}.{}:67", server_ip[0], server_ip[1], server_ip[2], server_ip[3]);
    let sock = UdpSocket::bind(&bind_addr).await?;
    sock.set_broadcast(true)?;
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
                    sock.send_to(&resp, "255.255.255.255:68").await?;
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

#[cfg(target_os = "windows")]
fn build_dhcp_response(xid: &[u8], chaddr: &[u8], server_ip: &[u8; 4], client_ip: &[u8; 4], msg_type: u8) -> Vec<u8> {
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
    resp[i] = 53; resp[i + 1] = 1; resp[i + 2] = msg_type; i += 3;
    resp[i] = 1; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(&[255, 255, 255, 0]); i += 6;
    resp[i] = 3; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(server_ip); i += 6;
    resp[i] = 54; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(server_ip); i += 6;
    resp[i] = 51; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(&[0, 0, 14, 16]); i += 6;
    resp[i] = 255; i += 1;
    resp.truncate(i);
    resp
}

#[cfg(target_os = "windows")]
fn find_dhcp_option(data: &[u8], option: u8) -> Option<Vec<u8>> {
    let mut i = 240;
    while i < data.len() && data[i] != 255 {
        let opt = data[i];
        if i + 1 >= data.len() { break; }
        let len = data[i + 1] as usize;
        if opt == option && i + 2 + len <= data.len() {
            return Some(data[i + 2..i + 2 + len].to_vec());
        }
        i += 2 + len;
    }
    None
}
