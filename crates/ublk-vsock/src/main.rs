use libc::{
    sockaddr_vm, socket, bind, listen, accept, setsockopt, AF_VSOCK, SOCK_STREAM, SOL_SOCKET,
    SO_SNDBUF, SO_RCVBUF, VMADDR_CID_ANY,
};
use libublk::ctrl::UblkCtrlBuilder;
use libublk::io::{BufDesc, BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{sys, UblkFlags, UblkIORes};
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const NBD_REQUEST_MAGIC: u32 = 0x25609513;
const NBD_CMD_READ: u16 = 0;
const NBD_CMD_WRITE: u16 = 1;
const SOCKET_BUF_SIZE: i32 = 4 * 1024 * 1024;

macro_rules! msg {
    ($($arg:tt)*) => {{
        eprintln!("ublk-vsock: {}", format!($($arg)*))
    }};
}

fn readall(fd: &mut UnixStream, buf: &mut [u8]) -> std::io::Result<()> {
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

fn vsock_listen(port: u32) -> std::io::Result<RawFd> {
    unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let opt: libc::c_int = 1;
        setsockopt(
            fd, SOL_SOCKET, libc::SO_REUSEADDR,
            &opt as *const _ as *const libc::c_void,
            std::mem::size_of_val(&opt) as u32,
        );
        let mut addr: sockaddr_vm = std::mem::zeroed();
        addr.svm_family = AF_VSOCK as u16;
        addr.svm_cid = VMADDR_CID_ANY;
        addr.svm_port = port;
        if bind(fd, &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&addr) as u32) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }
        if listen(fd, 16) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn vsock_accept(lsock: RawFd) -> std::io::Result<UnixStream> {
    unsafe {
        let fd = accept(lsock, std::ptr::null_mut(), std::ptr::null_mut());
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sockbuf: libc::c_int = SOCKET_BUF_SIZE;
        setsockopt(fd, SOL_SOCKET, SO_SNDBUF,
                   &sockbuf as *const _ as *const libc::c_void, 4);
        setsockopt(fd, SOL_SOCKET, SO_RCVBUF,
                   &sockbuf as *const _ as *const libc::c_void, 4);
        Ok(UnixStream::from_raw_fd(fd))
    }
}

fn nbd_handshake(stream: &mut UnixStream) -> std::io::Result<(u64, u16)> {
    let mut magic = [0u8; 8];
    let mut ihaveopt = [0u8; 8];
    let mut hflags = [0u8; 2];
    readall(stream, &mut magic)?;
    readall(stream, &mut ihaveopt)?;
    readall(stream, &mut hflags)?;

    let cflags: u32 = 1u32.to_be();
    stream.write_all(&cflags.to_ne_bytes())?;

    let opt_magic: u64 = 0x49484156454F5054u64.to_be();
    let opt_id: u32 = 1u32.to_be();
    let opt_len: u32 = 0u32.to_be();
    stream.write_all(&opt_magic.to_ne_bytes())?;
    stream.write_all(&opt_id.to_ne_bytes())?;
    stream.write_all(&opt_len.to_ne_bytes())?;

    let mut export_size_buf = [0u8; 8];
    let mut tflags_buf = [0u8; 2];
    let mut pad = [0u8; 124];
    readall(stream, &mut export_size_buf)?;
    readall(stream, &mut tflags_buf)?;
    readall(stream, &mut pad)?;

    let export_size = u64::from_be_bytes(export_size_buf);
    let tflags = u16::from_be_bytes(tflags_buf);
    Ok((export_size, tflags))
}

fn nbd_read(stream: &mut UnixStream, offset: u64, buf: &mut [u8]) -> i32 {
    let len = buf.len();
    let mut req = [0u8; 28];
    req[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
    req[4..6].copy_from_slice(&0u16.to_be_bytes());
    req[6..8].copy_from_slice(&NBD_CMD_READ.to_be_bytes());
    req[8..16].copy_from_slice(&0u64.to_be_bytes());
    req[16..24].copy_from_slice(&offset.to_be_bytes());
    req[24..28].copy_from_slice(&(len as u32).to_be_bytes());

    if stream.write_all(&req).is_err() {
        return -(libc::EIO as i32);
    }
    let mut reply = [0u8; 16];
    if readall(stream, &mut reply).is_err() {
        return -(libc::EIO as i32);
    }
    let error = u32::from_be_bytes(reply[4..8].try_into().unwrap());
    if error != 0 {
        return -(libc::EIO as i32);
    }
    if readall(stream, buf).is_err() {
        return -(libc::EIO as i32);
    }
    len as i32
}

fn nbd_write(stream: &mut UnixStream, offset: u64, buf: &[u8]) -> i32 {
    let len = buf.len();
    let mut req = [0u8; 28];
    req[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
    req[4..6].copy_from_slice(&0u16.to_be_bytes());
    req[6..8].copy_from_slice(&NBD_CMD_WRITE.to_be_bytes());
    req[8..16].copy_from_slice(&0u64.to_be_bytes());
    req[16..24].copy_from_slice(&offset.to_be_bytes());
    req[24..28].copy_from_slice(&(len as u32).to_be_bytes());

    if stream.write_all(&req).is_err() || stream.write_all(buf).is_err() {
        return -(libc::EIO as i32);
    }
    let mut reply = [0u8; 16];
    if readall(stream, &mut reply).is_err() {
        return -(libc::EIO as i32);
    }
    let error = u32::from_be_bytes(reply[4..8].try_into().unwrap());
    if error != 0 {
        return -(libc::EIO as i32);
    }
    len as i32
}

fn sd_notify_ready() {
    let sock_path = match std::env::var("NOTIFY_SOCKET") {
        Ok(p) => p,
        Err(_) => return,
    };
    unsafe {
        let fd = socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 { return; }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        let path_bytes = sock_path.as_bytes();
        if path_bytes[0] == b'@' {
            addr.sun_path[0] = 0;
            for (i, &b) in path_bytes[1..].iter().enumerate() {
                if i + 1 >= addr.sun_path.len() { break; }
                addr.sun_path[i + 1] = b as i8;
            }
        } else {
            for (i, &b) in path_bytes.iter().enumerate() {
                if i >= addr.sun_path.len() - 1 { break; }
                addr.sun_path[i] = b as i8;
            }
        }
        let len = std::mem::size_of::<libc::sa_family_t>() + path_bytes.len();
        let m = b"READY=1";
        libc::sendto(
            fd, m.as_ptr() as *const libc::c_void, m.len(), 0,
            &addr as *const _ as *const libc::sockaddr, len as u32,
        );
        libc::close(fd);
    }
}

fn move_to_root_cgroup() {
    let pid = std::process::id();
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", format!("{}\n", pid));
}

fn q_handler(qid: u16, dev: &UblkDev, mut stream: UnixStream) {
    let bufs = Rc::new(dev.alloc_queue_io_bufs());
    let bufs_ref = bufs.clone();

    let io_handler = move |q: &UblkQueue, tag: u16, _io: &UblkIOCtx| {
        let iod = q.get_iod(tag);
        let op = iod.op_flags & 0xff;
        let offset = iod.start_sector * 512;
        let bytes = (iod.nr_sectors as usize) * 512;

        let res = match op {
            sys::UBLK_IO_OP_READ => {
                let buf_ptr = bufs_ref[tag as usize].as_mut_ptr();
                let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf_ptr, bytes) };
                nbd_read(&mut stream, offset, buf_slice)
            }
            sys::UBLK_IO_OP_WRITE => {
                let buf_ptr = bufs_ref[tag as usize].as_mut_ptr();
                let buf_slice = unsafe { std::slice::from_raw_parts(buf_ptr, bytes) };
                nbd_write(&mut stream, offset, buf_slice)
            }
            _ => bytes as i32,
        };

        let _ = q.complete_io_cmd_unified(
            tag,
            BufDesc::Slice(bufs_ref[tag as usize].as_slice()),
            Ok(UblkIORes::Result(res)),
        );
    };

    let queue = UblkQueue::new(qid, dev)
        .unwrap()
        .submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs)))
        .unwrap();

    queue.wait_and_handle_io(io_handler);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("Usage: ublk-vsock <device> <vsock_port> [num_queues]");
        std::process::exit(1);
    }
    let vsock_port: u32 = args[2].parse().expect("invalid port");
    let num_queues: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);

    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    msg!("listening on vsock port {} (queues={})", vsock_port, num_queues);
    let lsock = vsock_listen(vsock_port).expect("vsock listen failed");

    msg!("waiting for {} Host relay connections", num_queues);
    let connections: Arc<Mutex<Vec<(UnixStream, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    for i in 0..num_queues {
        let mut stream = vsock_accept(lsock).expect("vsock accept failed");
        let (export_size, _tflags) = nbd_handshake(&mut stream).expect("NBD handshake failed");
        msg!("connection {}: handshake OK, export_size={} MB", i, export_size / (1024 * 1024));
        connections.lock().unwrap().push((stream, export_size));
    }
    unsafe { libc::close(lsock); }

    let export_size = connections.lock().unwrap()[0].1;
    msg!("all {} connections ready, export_size={} MB", num_queues, export_size / (1024 * 1024));

    msg!("creating ublk device (nr_queues={}, depth=64)", num_queues);
    let ctrl = match UblkCtrlBuilder::default()
        .name("bcvk")
        .nr_queues(num_queues)
        .depth(64u16)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            msg!("ublk ctrl build FAILED: {}", e);
            std::process::exit(1);
        }
    };
    msg!("ublk ctrl created, dev_id={}", ctrl.dev_info().dev_id);

    let sz = export_size;
    let tgt_init = move |dev: &mut UblkDev| {
        dev.set_default_params(sz);
        Ok(())
    };

    let conns = connections.clone();
    let q_fn = move |qid: u16, dev: &UblkDev| {
        let (stream, _) = conns.lock().unwrap().remove(0);
        q_handler(qid, dev, stream);
    };

    msg!("starting ublk target");
    let res = ctrl.run_target(tgt_init, q_fn, move |ctrl| {
        let dev_id = ctrl.dev_info().dev_id;
        msg!("/dev/ublkb{} ready ({} queues)", dev_id, num_queues);
        sd_notify_ready();
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN); }
        move_to_root_cgroup();
    });

    match res {
        Ok(_) => msg!("exited normally"),
        Err(e) => {
            msg!("FAILED: {}", e);
            std::process::exit(1);
        }
    }
}
