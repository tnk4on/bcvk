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
#[derive(Debug)]
pub struct BootFiles {
    pub grub_efi: Vec<u8>,
    pub kernel: Vec<u8>,
    pub initramfs: Vec<u8>,
    pub grub_cfg: String,
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
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
        let grub_cfg_bytes = boot_files.grub_cfg.into_bytes();
        files.insert("EFI\\BOOT\\grub.cfg".to_string(), grub_cfg_bytes.clone());
        files.insert("EFI/BOOT/grub.cfg".to_string(), grub_cfg_bytes.clone());
        // GRUB searches multiple paths for its config
        files.insert("grub.cfg".to_string(), grub_cfg_bytes.clone());
        files.insert("/grub.cfg".to_string(), grub_cfg_bytes.clone());
        files.insert("EFI/fedora/grub.cfg".to_string(), grub_cfg_bytes.clone());
        files.insert("EFI\\fedora\\grub.cfg".to_string(), grub_cfg_bytes.clone());
        files.insert("/EFI/fedora/grub.cfg".to_string(), grub_cfg_bytes.clone());

        Ok(Self {
            server_ip: sip,
            client_ip: cip,
            boot_file: "EFI\\BOOT\\BOOTX64.EFI".to_string(),
            files: Arc::new(files),
            stop: Arc::new(Notify::new()),
        })
    }

    pub fn start_background(&self) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
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

        (dhcp_handle, tftp_handle)
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

// === DHCP ===

#[cfg(target_os = "windows")]
async fn run_dhcp(server_ip: [u8; 4], client_ip: [u8; 4], boot_file: &str, stop: Arc<Notify>) -> Result<()> {
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
                    let resp = build_dhcp_response(xid, chaddr, &server_ip, &client_ip, boot_file, resp_type);
                    sock.send_to(&resp, "255.255.255.255:68").await?;
                    let mac = format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                        chaddr[0], chaddr[1], chaddr[2], chaddr[3], chaddr[4], chaddr[5]);
                    let type_name = if msg_type == 1 { "OFFER" } else { "ACK" };
                    info!("DHCP {} → {} ({})", type_name, format_ip(&client_ip), mac);
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn build_dhcp_response(xid: &[u8], chaddr: &[u8], server_ip: &[u8; 4], client_ip: &[u8; 4], boot_file: &str, msg_type: u8) -> Vec<u8> {
    let mut resp = vec![0u8; 512];
    resp[0] = 2;
    resp[1] = 1;
    resp[2] = 6;
    resp[4..8].copy_from_slice(xid);
    resp[16..20].copy_from_slice(client_ip);
    resp[20..24].copy_from_slice(server_ip);
    resp[28..44].copy_from_slice(chaddr);
    let sname = format_ip(server_ip);
    resp[44..44 + sname.len()].copy_from_slice(sname.as_bytes());
    let bf = boot_file.as_bytes();
    resp[108..108 + bf.len()].copy_from_slice(bf);
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
                let raw_req = data.to_vec();
                let files = files.clone();
                let stop = stop.clone();
                let sip = server_ip;
                tokio::spawn(async move {
                    if let Err(e) = handle_tftp_read(from, &raw_req, &filename, &files, stop, sip).await {
                        warn!("TFTP error for {}: {}", filename, e);
                    }
                });
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
async fn handle_tftp_read(client: SocketAddr, raw_request: &[u8], filename: &str, files: &HashMap<String, Vec<u8>>, _stop: Arc<Notify>, server_ip: [u8; 4]) -> Result<()> {
    let stripped = filename.trim_start_matches('/').trim_start_matches('\\');
    let with_fwd = stripped.replace('\\', "/");
    let with_back = stripped.replace('/', "\\");
    let data = files.get(filename)
        .or_else(|| files.get(stripped))
        .or_else(|| files.get(&with_fwd))
        .or_else(|| files.get(&with_back))
        .or_else(|| files.get(&format!("/{}", with_fwd)));

    let bind_addr = format!("{}.{}.{}.{}:0", server_ip[0], server_ip[1], server_ip[2], server_ip[3]);
    let data = match data {
        Some(d) => d,
        None => {
            warn!("TFTP: file not found: {}", filename);
            let xfer = UdpSocket::bind(&bind_addr).await?;
            let err = [0u8, 5, 0, 1, b'N', b'o', b't', b' ', b'f', b'o', b'u', b'n', b'd', 0];
            xfer.send_to(&err, client).await?;
            return Ok(());
        }
    };

    // Parse blksize option from RRQ (RFC 2348)
    let mut block_size: usize = 512;
    let mut tsize_requested = false;
    {
        let mut i = 2;
        // skip filename
        while i < raw_request.len() && raw_request[i] != 0 { i += 1; }
        i += 1;
        // skip mode
        while i < raw_request.len() && raw_request[i] != 0 { i += 1; }
        i += 1;
        // parse options
        while i < raw_request.len() {
            let opt_start = i;
            while i < raw_request.len() && raw_request[i] != 0 { i += 1; }
            let opt_name = String::from_utf8_lossy(&raw_request[opt_start..i]).to_lowercase();
            i += 1;
            let val_start = i;
            while i < raw_request.len() && raw_request[i] != 0 { i += 1; }
            let opt_val = String::from_utf8_lossy(&raw_request[val_start..i]);
            i += 1;
            match opt_name.as_str() {
                "blksize" => {
                    if let Ok(bs) = opt_val.parse::<usize>() {
                        block_size = bs.min(16384).max(8);
                    }
                }
                "tsize" => { tsize_requested = true; }
                _ => {}
            }
        }
    }

    info!("TFTP: serving {} ({} bytes, blksize={}) to {}", filename, data.len(), block_size, client.ip());

    let xfer = UdpSocket::bind(&bind_addr).await?;
    let mut ack_buf = [0u8; 600];

    // Send OACK if options were requested
    if block_size != 512 || tsize_requested {
        let mut oack = vec![0u8, 6]; // OACK opcode
        if block_size != 512 {
            oack.extend_from_slice(b"blksize\0");
            oack.extend_from_slice(block_size.to_string().as_bytes());
            oack.push(0);
        }
        if tsize_requested {
            oack.extend_from_slice(b"tsize\0");
            oack.extend_from_slice(data.len().to_string().as_bytes());
            oack.push(0);
        }
        for retry in 0..5 {
            xfer.send_to(&oack, client).await?;
            match tokio::time::timeout(Duration::from_secs(3), xfer.recv_from(&mut ack_buf)).await {
                Ok(Ok((_, _))) => break,
                _ => { if retry == 4 { bail!("TFTP: OACK timeout"); } }
            }
        }
    }

    let mut block_num: u16 = 1;
    let mut offset: usize = 0;

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
        if block_num % 2000 == 0 {
            debug!("TFTP: {} block {}...", filename, block_num);
        }
    }

    info!("TFTP: {} complete ({} blocks, {} bytes)", filename, block_num, data.len());
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
