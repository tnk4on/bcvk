use libc::{
    accept, bind, listen, setsockopt, sockaddr_vm, socket, AF_VSOCK, SOCK_STREAM, SOL_SOCKET,
    SO_RCVBUF, SO_SNDBUF, VMADDR_CID_ANY,
};
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

const MAX_CONNECTIONS: usize = 16;
const RELAY_BUF_SIZE: usize = 256 * 1024;
const SOCKET_BUF_SIZE: i32 = 1024 * 1024;

// Netlink constants
const AF_NETLINK: i32 = 16;
const NETLINK_GENERIC: i32 = 16;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_ACK: u16 = 4;
const NLMSG_ERROR: u16 = 2;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const NLA_F_NESTED: u16 = 1 << 15;

// NBD netlink constants
const NBD_CMD_CONNECT: u8 = 1;
const NBD_GENL_VERSION: u8 = 1;
const NBD_ATTR_INDEX: u16 = 1;
const NBD_ATTR_SIZE_BYTES: u16 = 2;
const NBD_ATTR_BLOCK_SIZE_BYTES: u16 = 3;
const NBD_ATTR_TIMEOUT: u16 = 4;
const NBD_ATTR_SERVER_FLAGS: u16 = 5;
const NBD_ATTR_SOCKETS: u16 = 7;
const NBD_SOCK_ITEM: u16 = 1;
const NBD_SOCK_FD: u16 = 1;

macro_rules! msg {
    ($($arg:tt)*) => {{ eprintln!("nbd-vsock: {}", format!($($arg)*)) }};
}

fn readall(fd: &mut impl Read, buf: &mut [u8]) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = fd.read(&mut buf[done..])?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "EOF"));
        }
        done += n;
    }
    Ok(())
}

fn writeall(fd: RawFd, buf: &[u8]) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = unsafe { libc::write(fd, buf.as_ptr().add(done) as *const _, buf.len() - done) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        done += n as usize;
    }
    Ok(())
}

fn readall_fd(fd: RawFd, buf: &mut [u8]) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().add(done) as *mut _, buf.len() - done) };
        if n <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        done += n as usize;
    }
    Ok(())
}

// --- vsock ---

fn vsock_listen(port: u32) -> std::io::Result<RawFd> {
    unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let opt: libc::c_int = 1;
        setsockopt(fd, SOL_SOCKET, libc::SO_REUSEADDR, &opt as *const _ as *const libc::c_void, 4);
        let mut addr: sockaddr_vm = std::mem::zeroed();
        addr.svm_family = AF_VSOCK as u16;
        addr.svm_cid = VMADDR_CID_ANY;
        addr.svm_port = port;
        if bind(fd, &addr as *const _ as *const libc::sockaddr, std::mem::size_of_val(&addr) as u32) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }
        if listen(fd, MAX_CONNECTIONS as i32) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn vsock_accept(lsock: RawFd) -> std::io::Result<RawFd> {
    unsafe {
        let fd = accept(lsock, std::ptr::null_mut(), std::ptr::null_mut());
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sockbuf: libc::c_int = SOCKET_BUF_SIZE;
        setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &sockbuf as *const _ as *const libc::c_void, 4);
        setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &sockbuf as *const _ as *const libc::c_void, 4);
        Ok(fd)
    }
}

fn nbd_handshake(sock: RawFd) -> std::io::Result<(u64, u16)> {
    let mut stream = unsafe { UnixStream::from_raw_fd(sock) };
    let mut magic = [0u8; 8];
    let mut ihaveopt = [0u8; 8];
    let mut hflags = [0u8; 2];
    readall(&mut stream, &mut magic)?;
    readall(&mut stream, &mut ihaveopt)?;
    readall(&mut stream, &mut hflags)?;
    stream.write_all(&1u32.to_be_bytes())?;
    stream.write_all(&0x49484156454F5054u64.to_be_bytes())?;
    stream.write_all(&1u32.to_be_bytes())?;
    stream.write_all(&0u32.to_be_bytes())?;
    let mut sz = [0u8; 8];
    let mut tf = [0u8; 2];
    let mut pad = [0u8; 124];
    readall(&mut stream, &mut sz)?;
    readall(&mut stream, &mut tf)?;
    readall(&mut stream, &mut pad)?;
    // Prevent drop from closing the fd (we manage it manually)
    std::mem::forget(stream);
    Ok((u64::from_be_bytes(sz), u16::from_be_bytes(tf)))
}

// --- relay ---

fn relay_thread(from: RawFd, to: RawFd) {
    let mut buf = vec![0u8; RELAY_BUF_SIZE];
    loop {
        let n = unsafe { libc::read(from, buf.as_mut_ptr() as *mut _, RELAY_BUF_SIZE) };
        if n <= 0 {
            break;
        }
        if writeall(to, &buf[..n as usize]).is_err() {
            break;
        }
    }
    unsafe {
        libc::shutdown(from, libc::SHUT_RD);
        libc::shutdown(to, libc::SHUT_WR);
    }
}

fn start_relay(fd_a: RawFd, fd_b: RawFd) -> (std::thread::JoinHandle<()>, std::thread::JoinHandle<()>) {
    let t1 = std::thread::spawn(move || relay_thread(fd_a, fd_b));
    let t2 = std::thread::spawn(move || relay_thread(fd_b, fd_a));
    (t1, t2)
}

// --- netlink ---

fn nla_put_u32(buf: &mut Vec<u8>, attr_type: u16, val: u32) {
    buf.extend_from_slice(&8u16.to_ne_bytes());
    buf.extend_from_slice(&attr_type.to_ne_bytes());
    buf.extend_from_slice(&val.to_ne_bytes());
}

fn nla_put_u64(buf: &mut Vec<u8>, attr_type: u16, val: u64) {
    buf.extend_from_slice(&12u16.to_ne_bytes());
    buf.extend_from_slice(&attr_type.to_ne_bytes());
    buf.extend_from_slice(&val.to_ne_bytes());
    // NLA_ALIGN(12) = 12, already aligned
}

fn nla_nest_start(buf: &mut Vec<u8>, attr_type: u16) -> usize {
    let offset = buf.len();
    buf.extend_from_slice(&0u16.to_ne_bytes()); // placeholder len
    buf.extend_from_slice(&(attr_type | NLA_F_NESTED).to_ne_bytes());
    offset
}

fn nla_nest_end(buf: &mut Vec<u8>, offset: usize) {
    let len = (buf.len() - offset) as u16;
    buf[offset..offset + 2].copy_from_slice(&len.to_ne_bytes());
}

fn nla_put_string(buf: &mut Vec<u8>, attr_type: u16, s: &str) {
    let slen = s.len() + 1; // include null terminator
    let nla_len = (4 + slen) as u16;
    buf.extend_from_slice(&nla_len.to_ne_bytes());
    buf.extend_from_slice(&attr_type.to_ne_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.push(0); // null terminator
    // NLA_ALIGN padding
    let padded = (slen + 3) & !3;
    for _ in slen..padded {
        buf.push(0);
    }
}

#[repr(C)]
struct NlMsgHdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

#[repr(C)]
struct GenlMsgHdr {
    cmd: u8,
    version: u8,
    reserved: u16,
}

#[repr(C)]
struct SockaddrNl {
    nl_family: u16,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

fn nl_open() -> std::io::Result<RawFd> {
    unsafe {
        let fd = libc::socket(AF_NETLINK, libc::SOCK_DGRAM, NETLINK_GENERIC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sa = SockaddrNl {
            nl_family: AF_NETLINK as u16,
            nl_pad: 0,
            nl_pid: libc::getpid() as u32,
            nl_groups: 0,
        };
        if libc::bind(fd, &sa as *const _ as *const libc::sockaddr, std::mem::size_of::<SockaddrNl>() as u32) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn genl_resolve_family(nl: RawFd, name: &str) -> std::io::Result<u16> {
    let nlh_size = std::mem::size_of::<NlMsgHdr>();
    let gh_size = std::mem::size_of::<GenlMsgHdr>();

    let mut buf = Vec::with_capacity(256);
    buf.resize(nlh_size + gh_size, 0);

    // genlmsghdr
    let gh = GenlMsgHdr { cmd: CTRL_CMD_GETFAMILY, version: 1, reserved: 0 };
    unsafe {
        std::ptr::copy_nonoverlapping(&gh as *const _ as *const u8, buf.as_mut_ptr().add(nlh_size), gh_size);
    }

    nla_put_string(&mut buf, CTRL_ATTR_FAMILY_NAME, name);

    // nlmsghdr
    let nh = NlMsgHdr {
        nlmsg_len: buf.len() as u32,
        nlmsg_type: GENL_ID_CTRL,
        nlmsg_flags: NLM_F_REQUEST,
        nlmsg_seq: 1,
        nlmsg_pid: unsafe { libc::getpid() } as u32,
    };
    unsafe {
        std::ptr::copy_nonoverlapping(&nh as *const _ as *const u8, buf.as_mut_ptr(), nlh_size);
    }

    let sent = unsafe { libc::send(nl, buf.as_ptr() as *const _, buf.len(), 0) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut resp = [0u8; 4096];
    let n = unsafe { libc::recv(nl, resp.as_mut_ptr() as *mut _, resp.len(), 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let n = n as usize;

    // Check for error
    let resp_type = u16::from_ne_bytes(resp[4..6].try_into().unwrap());
    if resp_type == NLMSG_ERROR {
        let err = i32::from_ne_bytes(resp[nlh_size..nlh_size + 4].try_into().unwrap());
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(-err));
        }
    }

    // Parse attributes to find CTRL_ATTR_FAMILY_ID
    let mut off = nlh_size + gh_size;
    while off + 4 <= n {
        let alen = u16::from_ne_bytes(resp[off..off + 2].try_into().unwrap());
        let atype = u16::from_ne_bytes(resp[off + 2..off + 4].try_into().unwrap());
        if alen < 4 {
            break;
        }
        if atype == CTRL_ATTR_FAMILY_ID && alen >= 6 {
            let fid = u16::from_ne_bytes(resp[off + 4..off + 6].try_into().unwrap());
            return Ok(fid);
        }
        off += ((alen as usize) + 3) & !3; // NLA_ALIGN
    }

    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nbd family not found"))
}

fn nbd_connect(nl: RawFd, family_id: u16, dev_index: u32, fds: &[RawFd], size: u64, block_size: u32, server_flags: u16) -> std::io::Result<()> {
    let nlh_size = std::mem::size_of::<NlMsgHdr>();
    let gh_size = std::mem::size_of::<GenlMsgHdr>();

    let mut buf = Vec::with_capacity(512);
    buf.resize(nlh_size + gh_size, 0);

    let gh = GenlMsgHdr { cmd: NBD_CMD_CONNECT, version: NBD_GENL_VERSION, reserved: 0 };
    unsafe {
        std::ptr::copy_nonoverlapping(&gh as *const _ as *const u8, buf.as_mut_ptr().add(nlh_size), gh_size);
    }

    nla_put_u32(&mut buf, NBD_ATTR_INDEX, dev_index);
    nla_put_u64(&mut buf, NBD_ATTR_SIZE_BYTES, size);
    nla_put_u64(&mut buf, NBD_ATTR_BLOCK_SIZE_BYTES, block_size as u64);
    nla_put_u64(&mut buf, NBD_ATTR_TIMEOUT, 0);
    nla_put_u64(&mut buf, NBD_ATTR_SERVER_FLAGS, server_flags as u64);

    let socks_off = nla_nest_start(&mut buf, NBD_ATTR_SOCKETS);
    for &fd in fds {
        let item_off = nla_nest_start(&mut buf, NBD_SOCK_ITEM);
        nla_put_u32(&mut buf, NBD_SOCK_FD, fd as u32);
        nla_nest_end(&mut buf, item_off);
    }
    nla_nest_end(&mut buf, socks_off);

    let nh = NlMsgHdr {
        nlmsg_len: buf.len() as u32,
        nlmsg_type: family_id,
        nlmsg_flags: NLM_F_REQUEST | NLM_F_ACK,
        nlmsg_seq: 2,
        nlmsg_pid: unsafe { libc::getpid() } as u32,
    };
    unsafe {
        std::ptr::copy_nonoverlapping(&nh as *const _ as *const u8, buf.as_mut_ptr(), nlh_size);
    }

    let sent = unsafe { libc::send(nl, buf.as_ptr() as *const _, buf.len(), 0) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut resp = [0u8; 4096];
    let n = unsafe { libc::recv(nl, resp.as_mut_ptr() as *mut _, resp.len(), 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let resp_type = u16::from_ne_bytes(resp[4..6].try_into().unwrap());
    if resp_type == NLMSG_ERROR {
        let err = i32::from_ne_bytes(resp[nlh_size..nlh_size + 4].try_into().unwrap());
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(-err));
        }
    }

    Ok(())
}

// --- sd_notify ---

fn sd_notify_ready() {
    let sock_path = match std::env::var("NOTIFY_SOCKET") {
        Ok(p) => p,
        Err(_) => return,
    };
    unsafe {
        let fd = socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return;
        }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        let pb = sock_path.as_bytes();
        if pb[0] == b'@' {
            addr.sun_path[0] = 0;
            for (i, &b) in pb[1..].iter().enumerate() {
                if i + 1 >= addr.sun_path.len() {
                    break;
                }
                addr.sun_path[i + 1] = b as i8;
            }
        } else {
            for (i, &b) in pb.iter().enumerate() {
                if i >= addr.sun_path.len() - 1 {
                    break;
                }
                addr.sun_path[i] = b as i8;
            }
        }
        let len = std::mem::size_of::<libc::sa_family_t>() + pb.len();
        libc::sendto(fd, b"READY=1".as_ptr() as *const _, 7, 0, &addr as *const _ as *const _, len as u32);
        libc::close(fd);
    }
}

fn move_to_root_cgroup() {
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", format!("{}\n", std::process::id()));
}

// --- main ---

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("Usage: nbd-vsock /dev/nbdX vsock_port [num_connections]");
        std::process::exit(1);
    }
    let dev = &args[1];
    let port: u32 = args[2].parse().expect("invalid port");
    let num_conns: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1).min(MAX_CONNECTIONS).max(1);
    let dev_index: u32 = dev.trim_start_matches("/dev/nbd").parse().expect("invalid device index");

    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    let lsock = vsock_listen(port).expect("vsock listen failed");
    msg!("listening on vsock port {}", port);
    msg!("waiting for {} connection(s) from Host relay", num_conns);

    let mut vsock_fds = Vec::with_capacity(num_conns);
    let mut unix_fds = Vec::with_capacity(num_conns);
    let mut relay_handles = Vec::with_capacity(num_conns * 2);
    let mut export_size: u64 = 0;
    let mut tflags: u16 = 0;

    for i in 0..num_conns {
        let sock = vsock_accept(lsock).unwrap_or_else(|e| {
            msg!("connection {} accept failed: {}", i, e);
            std::process::exit(1);
        });
        let (sz, fl) = nbd_handshake(sock).unwrap_or_else(|e| {
            msg!("connection {} handshake failed: {}", i, e);
            std::process::exit(1);
        });
        export_size = sz;
        tflags = fl;

        let mut pair = [0i32; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } < 0 {
            msg!("socketpair failed");
            std::process::exit(1);
        }

        let sndbuf: libc::c_int = RELAY_BUF_SIZE as i32;
        unsafe {
            setsockopt(pair[0], SOL_SOCKET, SO_SNDBUF, &sndbuf as *const _ as *const libc::c_void, 4);
            setsockopt(pair[0], SOL_SOCKET, SO_RCVBUF, &sndbuf as *const _ as *const libc::c_void, 4);
            setsockopt(pair[1], SOL_SOCKET, SO_SNDBUF, &sndbuf as *const _ as *const libc::c_void, 4);
            setsockopt(pair[1], SOL_SOCKET, SO_RCVBUF, &sndbuf as *const _ as *const libc::c_void, 4);
        }

        vsock_fds.push(sock);
        unix_fds.push(pair[0]);
        let (t1, t2) = start_relay(sock, pair[1]);
        relay_handles.push(t1);
        relay_handles.push(t2);
    }

    unsafe { libc::close(lsock); }
    msg!("{} connection(s), export size={} bytes", num_conns, export_size);

    let nl = nl_open().expect("netlink socket failed");
    let family_id = genl_resolve_family(nl, "nbd").unwrap_or_else(|e| {
        msg!("nbd genl family: {}", e);
        std::process::exit(1);
    });

    nbd_connect(nl, family_id, dev_index, &unix_fds, export_size, 512, tflags).unwrap_or_else(|e| {
        msg!("NBD_CMD_CONNECT failed: {}", e);
        std::process::exit(1);
    });

    msg!("kernel I/O started, relay running");
    unsafe { libc::close(nl); }
    sd_notify_ready();

    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN); }
    move_to_root_cgroup();

    for h in relay_handles {
        let _ = h.join();
    }
}
