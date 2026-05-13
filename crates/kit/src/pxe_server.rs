//! Minimal DHCP + TFTP server for PXE boot on Windows.
//!
//! Single-client, memory-backed. Runs as async tasks within bcvk.exe.

#[cfg(target_os = "windows")]
use color_eyre::{eyre::bail, Result};
#[cfg(target_os = "windows")]
use std::collections::HashMap;
#[cfg(target_os = "windows")]
use std::net::SocketAddr;
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use std::time::Duration;
#[cfg(target_os = "windows")]
use tokio::net::UdpSocket;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
#[cfg(target_os = "windows")]
use tracing::{debug, info, warn};

#[cfg(target_os = "windows")]
pub struct BootFiles {
    pub grub_efi: Vec<u8>,
    pub kernel: Vec<u8>,
    pub initramfs: Vec<u8>,
    pub grub_cfg: String,
}

#[cfg(target_os = "windows")]
pub struct PxeServer {
    server_ip: [u8; 4],
    client_ip: [u8; 4],
    boot_file: String,
    files: Arc<HashMap<String, Vec<u8>>>,
    stop: Arc<Notify>,
}

#[cfg(target_os = "windows")]
impl PxeServer {
    pub fn new(server_ip: &str, client_ip: &str, boot_files: BootFiles) -> Result<Self> {
        let sip = parse_ip(server_ip)?;
        let cip = parse_ip(client_ip)?;

        let mut files: HashMap<String, Vec<u8>> = HashMap::new();
        files.insert("EFI\\BOOT\\BOOTX64.EFI".to_string(), boot_files.grub_efi);
        files.insert("EFI/BOOT/BOOTX64.EFI".to_string(), files.get("EFI\\BOOT\\BOOTX64.EFI").unwrap().clone());
        files.insert("boot\\vmlinuz".to_string(), boot_files.kernel);
        files.insert("boot/vmlinuz".to_string(), files.get("boot\\vmlinuz").unwrap().clone());
        files.insert("boot\\initramfs.img".to_string(), boot_files.initramfs);
        files.insert("boot/initramfs.img".to_string(), files.get("boot\\initramfs.img").unwrap().clone());
        files.insert("EFI\\BOOT\\grub.cfg".to_string(), boot_files.grub_cfg.into_bytes());
        files.insert("EFI/BOOT/grub.cfg".to_string(), files.get("EFI\\BOOT\\grub.cfg").unwrap().clone());

        Ok(Self {
            server_ip: sip,
            client_ip: cip,
            boot_file: "EFI\\BOOT\\BOOTX64.EFI".to_string(),
            files: Arc::new(files),
            stop: Arc::new(Notify::new()),
        })
    }

    pub async fn serve(&self) -> Result<()> {
        let dhcp_handle = tokio::spawn({
            let sip = self.server_ip;
            let cip = self.client_ip;
            let boot_file = self.boot_file.clone();
            let stop = self.stop.clone();
            async move {
                if let Err(e) = run_dhcp(sip, cip, &boot_file, stop).await {
                    warn!("DHCP server error: {}", e);
                }
            }
        });

        let tftp_handle = tokio::spawn({
            let sip = self.server_ip;
            let files = self.files.clone();
            let stop = self.stop.clone();
            async move {
                if let Err(e) = run_tftp(sip, files, stop).await {
                    warn!("TFTP server error: {}", e);
                }
            }
        });

        tokio::select! {
            _ = dhcp_handle => {},
            _ = tftp_handle => {},
        }
        Ok(())
    }

    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

#[cfg(target_os = "windows")]
fn parse_ip(s: &str) -> Result<[u8; 4]> {
    let parts: Vec<u8> = s.split('.').map(|p| p.parse()).collect::<std::result::Result<Vec<_>, _>>()?;
    if parts.len() != 4 {
        bail!("invalid IP: {}", s);
    }
    Ok([parts[0], parts[1], parts[2], parts[3]])
}

// === DHCP ===

#[cfg(target_os = "windows")]
async fn run_dhcp(server_ip: [u8; 4], client_ip: [u8; 4], boot_file: &str, stop: Arc<Notify>) -> Result<()> {
    let addr = format!("{}.{}.{}.{}:67", server_ip[0], server_ip[1], server_ip[2], server_ip[3]);
    let sock = UdpSocket::bind(&addr).await?;
    sock.set_broadcast(true)?;
    info!("DHCP listening on {}", addr);

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

                // Find DHCP message type in options
                let msg_type = find_dhcp_option(data, 53).and_then(|v| v.first().copied()).unwrap_or(0);

                if msg_type == 1 || msg_type == 3 {
                    let resp_type = if msg_type == 1 { 2u8 } else { 5u8 };
                    let resp = build_dhcp_response(xid, chaddr, &server_ip, &client_ip, boot_file, resp_type);
                    sock.send_to(&resp, "255.255.255.255:68").await?;
                    let type_name = if msg_type == 1 { "OFFER" } else { "ACK" };
                    let mac = format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                        chaddr[0], chaddr[1], chaddr[2], chaddr[3], chaddr[4], chaddr[5]);
                    info!("DHCP {} → {} ({})", type_name, format_ip(&client_ip), mac);
                }
            }
        }
    }
    Ok(())
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

#[cfg(target_os = "windows")]
fn build_dhcp_response(xid: &[u8], chaddr: &[u8], server_ip: &[u8; 4], client_ip: &[u8; 4], boot_file: &str, msg_type: u8) -> Vec<u8> {
    let mut resp = vec![0u8; 300];
    resp[0] = 2; // BOOTREPLY
    resp[1] = 1; // Ethernet
    resp[2] = 6; // HW addr len
    resp[4..8].copy_from_slice(xid);
    resp[16..20].copy_from_slice(client_ip); // yiaddr
    resp[20..24].copy_from_slice(server_ip); // siaddr
    resp[28..44].copy_from_slice(chaddr);

    // sname (TFTP server)
    let sname = format!("{}", format_ip(server_ip));
    resp[44..44 + sname.len()].copy_from_slice(sname.as_bytes());

    // file (boot file)
    let bf = boot_file.as_bytes();
    resp[108..108 + bf.len()].copy_from_slice(bf);

    // Magic cookie
    resp[236..240].copy_from_slice(&[99, 130, 83, 99]);

    let mut i = 240;
    // Option 53: DHCP Message Type
    resp[i] = 53; resp[i + 1] = 1; resp[i + 2] = msg_type; i += 3;
    // Option 1: Subnet Mask
    resp[i] = 1; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(&[255, 255, 255, 0]); i += 6;
    // Option 3: Router
    resp[i] = 3; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(server_ip); i += 6;
    // Option 54: Server Identifier
    resp[i] = 54; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(server_ip); i += 6;
    // Option 51: Lease Time (3600s)
    resp[i] = 51; resp[i + 1] = 4; resp[i + 2..i + 6].copy_from_slice(&[0, 0, 14, 16]); i += 6;
    // End
    resp[i] = 255; i += 1;

    resp.truncate(i);
    resp
}

// === TFTP ===

#[cfg(target_os = "windows")]
async fn run_tftp(server_ip: [u8; 4], files: Arc<HashMap<String, Vec<u8>>>, stop: Arc<Notify>) -> Result<()> {
    let addr = format!("{}.{}.{}.{}:69", server_ip[0], server_ip[1], server_ip[2], server_ip[3]);
    let sock = UdpSocket::bind(&addr).await?;
    info!("TFTP listening on {}", addr);

    let mut buf = vec![0u8; 1500];
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            result = sock.recv_from(&mut buf) => {
                let (len, from) = result?;
                let data = &buf[..len];
                let opcode = ((data[0] as u16) << 8) | data[1] as u16;
                if opcode != 1 { continue; } // RRQ only

                let filename = extract_string(&data[2..]);
                let files = files.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_tftp_read(from, &filename, &files, stop).await {
                        warn!("TFTP error for {}: {}", filename, e);
                    }
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn handle_tftp_read(client: SocketAddr, filename: &str, files: &HashMap<String, Vec<u8>>, _stop: Arc<Notify>) -> Result<()> {
    let normalized = filename.replace('/', "\\");
    let data = files.get(filename)
        .or_else(|| files.get(&normalized))
        .or_else(|| files.get(&filename.replace('\\', "/")));

    let data = match data {
        Some(d) => d,
        None => {
            warn!("TFTP: file not found: {}", filename);
            let xfer = UdpSocket::bind("0.0.0.0:0").await?;
            let err = [0u8, 5, 0, 1, b'N', b'o', b't', b' ', b'f', b'o', b'u', b'n', b'd', 0];
            xfer.send_to(&err, client).await?;
            return Ok(());
        }
    };

    info!("TFTP: serving {} ({} bytes) to {}", filename, data.len(), client.ip());

    let xfer = UdpSocket::bind("0.0.0.0:0").await?;
    let block_size: usize = 512;
    let mut block_num: u16 = 1;
    let mut offset: usize = 0;
    let mut ack_buf = [0u8; 4];

    while offset < data.len() || offset == 0 {
        let end = std::cmp::min(offset + block_size, data.len());
        let chunk = &data[offset..end];

        let mut pkt = Vec::with_capacity(4 + chunk.len());
        pkt.extend_from_slice(&[0, 3, (block_num >> 8) as u8, (block_num & 0xFF) as u8]);
        pkt.extend_from_slice(chunk);

        for retry in 0..5 {
            xfer.send_to(&pkt, client).await?;
            match tokio::time::timeout(Duration::from_secs(3), xfer.recv_from(&mut ack_buf)).await {
                Ok(Ok((_, _))) if ack_buf[1] == 4 => break,
                _ => {
                    if retry == 4 {
                        bail!("TFTP: timeout waiting for ACK block {}", block_num);
                    }
                }
            }
        }

        offset += block_size;
        block_num = block_num.wrapping_add(1);
        if block_num % 5000 == 0 {
            debug!("TFTP: {} block {}...", filename, block_num);
        }
    }

    info!("TFTP: {} complete ({} blocks)", filename, block_num);
    Ok(())
}

#[cfg(target_os = "windows")]
fn extract_string(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).to_string()
}

#[cfg(target_os = "windows")]
fn format_ip(ip: &[u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}
