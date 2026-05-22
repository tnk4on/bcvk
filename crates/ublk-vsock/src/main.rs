use io_uring::IoUring;
use libc::{
    sockaddr_vm, socket, bind, listen, accept, setsockopt, AF_VSOCK, SOCK_STREAM, SOL_SOCKET,
    SO_SNDBUF, SO_RCVBUF, VMADDR_CID_ANY,
};
use libublk::ctrl::UblkCtrlBuilder;
use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use libublk::uring_async::{run_uring_tasks, ublk_reap_events_with_handler, ublk_wake_task};
use libublk::{sys, BufDesc, UblkError, UblkFlags};
use std::cell::RefCell;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::io::RawFd;
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

fn readall_sync(fd: &mut UnixStream, buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    let mut done = 0;
    while done < buf.len() {
        let n = fd.read(&mut buf[done..])?;
        if n == 0 { return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "EOF")); }
        done += n;
    }
    Ok(())
}

fn vsock_listen(port: u32) -> std::io::Result<RawFd> {
    unsafe {
        let fd = socket(AF_VSOCK, SOCK_STREAM, 0);
        if fd < 0 { return Err(std::io::Error::last_os_error()); }
        let opt: libc::c_int = 1;
        setsockopt(fd, SOL_SOCKET, libc::SO_REUSEADDR,
            &opt as *const _ as *const libc::c_void, std::mem::size_of_val(&opt) as u32);
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

fn vsock_accept(lsock: RawFd) -> std::io::Result<RawFd> {
    unsafe {
        let fd = accept(lsock, std::ptr::null_mut(), std::ptr::null_mut());
        if fd < 0 { return Err(std::io::Error::last_os_error()); }
        let sockbuf: libc::c_int = SOCKET_BUF_SIZE;
        setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &sockbuf as *const _ as *const libc::c_void, 4);
        setsockopt(fd, SOL_SOCKET, SO_RCVBUF, &sockbuf as *const _ as *const libc::c_void, 4);
        Ok(fd)
    }
}

fn nbd_handshake_sync(fd: RawFd) -> std::io::Result<u64> {
    use std::io::Write;
    let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
    let mut magic = [0u8; 8]; let mut ihaveopt = [0u8; 8]; let mut hflags = [0u8; 2];
    readall_sync(&mut stream, &mut magic)?;
    readall_sync(&mut stream, &mut ihaveopt)?;
    readall_sync(&mut stream, &mut hflags)?;
    stream.write_all(&1u32.to_be().to_ne_bytes())?;
    stream.write_all(&0x49484156454F5054u64.to_be().to_ne_bytes())?;
    stream.write_all(&1u32.to_be().to_ne_bytes())?;
    stream.write_all(&0u32.to_be().to_ne_bytes())?;
    let mut sz = [0u8; 8]; let mut tf = [0u8; 2]; let mut pad = [0u8; 124];
    readall_sync(&mut stream, &mut sz)?;
    readall_sync(&mut stream, &mut tf)?;
    readall_sync(&mut stream, &mut pad)?;
    let export_size = u64::from_be_bytes(sz);
    let _ = stream.into_raw_fd(); // don't close fd
    Ok(export_size)
}

// Async io_uring helpers
async fn uring_read_exact(q: &UblkQueue<'_>, fd: RawFd, buf: &mut [u8]) -> Result<(), UblkError> {
    let mut done = 0;
    while done < buf.len() {
        let sqe = io_uring::opcode::Read::new(
            io_uring::types::Fd(fd),
            buf[done..].as_mut_ptr(),
            (buf.len() - done) as u32,
        ).build();
        let n = q.ublk_submit_sqe(sqe).await;
        if n <= 0 { return Err(UblkError::OtherError(-(libc::EIO as i32))); }
        done += n as usize;
    }
    Ok(())
}

async fn uring_write_all(q: &UblkQueue<'_>, fd: RawFd, buf: &[u8]) -> Result<(), UblkError> {
    let mut done = 0;
    while done < buf.len() {
        let sqe = io_uring::opcode::Write::new(
            io_uring::types::Fd(fd),
            buf[done..].as_ptr(),
            (buf.len() - done) as u32,
        ).build();
        let n = q.ublk_submit_sqe(sqe).await;
        if n <= 0 { return Err(UblkError::OtherError(-(libc::EIO as i32))); }
        done += n as usize;
    }
    Ok(())
}

// Per-tag async IO task (stage 2: no pipelining, direct vsock IO per tag)
async fn io_task(
    q: &UblkQueue<'_>,
    tag: u16,
    vsock_fd: RawFd,
) -> Result<(), UblkError> {
    let buf_size = q.dev.dev_info.max_io_buf_bytes as usize;
    let buffer = IoBuf::<u8>::new(buf_size);

    q.submit_io_prep_cmd(tag, BufDesc::Slice(buffer.as_slice()), 0, Some(&buffer))
        .await?;

    loop {
        let iod = q.get_iod(tag);
        let op = iod.op_flags & 0xff;
        let offset = iod.start_sector * 512;
        let bytes = (iod.nr_sectors as usize) * 512;

        let result = match op {
            sys::UBLK_IO_OP_READ => {
                let mut req = [0u8; 28];
                req[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
                req[6..8].copy_from_slice(&NBD_CMD_READ.to_be_bytes());
                req[16..24].copy_from_slice(&offset.to_be_bytes());
                req[24..28].copy_from_slice(&(bytes as u32).to_be_bytes());

                match uring_write_all(q, vsock_fd, &req).await {
                    Ok(()) => {},
                    Err(_) => { -libc::EIO; continue; }
                }

                let mut reply = [0u8; 16];
                match uring_read_exact(q, vsock_fd, &mut reply).await {
                    Ok(()) => {},
                    Err(_) => { -libc::EIO; continue; }
                }

                let error = u32::from_be_bytes(reply[4..8].try_into().unwrap());
                if error != 0 {
                    -(libc::EIO as i32)
                } else {
                    let buf_slice = unsafe {
                        std::slice::from_raw_parts_mut(buffer.as_mut_ptr(), bytes)
                    };
                    match uring_read_exact(q, vsock_fd, buf_slice).await {
                        Ok(()) => bytes as i32,
                        Err(_) => -(libc::EIO as i32),
                    }
                }
            }
            sys::UBLK_IO_OP_WRITE => {
                let mut req = [0u8; 28];
                req[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
                req[6..8].copy_from_slice(&NBD_CMD_WRITE.to_be_bytes());
                req[16..24].copy_from_slice(&offset.to_be_bytes());
                req[24..28].copy_from_slice(&(bytes as u32).to_be_bytes());

                let buf_slice = unsafe {
                    std::slice::from_raw_parts(buffer.as_mut_ptr(), bytes)
                };

                let write_ok = uring_write_all(q, vsock_fd, &req).await.is_ok()
                    && uring_write_all(q, vsock_fd, buf_slice).await.is_ok();

                if !write_ok {
                    -(libc::EIO as i32)
                } else {
                    let mut reply = [0u8; 16];
                    match uring_read_exact(q, vsock_fd, &mut reply).await {
                        Ok(()) => {
                            let error = u32::from_be_bytes(reply[4..8].try_into().unwrap());
                            if error != 0 { -(libc::EIO as i32) } else { bytes as i32 }
                        }
                        Err(_) => -(libc::EIO as i32),
                    }
                }
            }
            _ => bytes as i32,
        };

        q.submit_io_commit_cmd(tag, BufDesc::Slice(buffer.as_slice()), result)
            .await?;
    }
}

async fn poll_and_handle_rings<R, I>(
    q: &UblkQueue<'_>,
    run_ops: R,
    is_done: I,
) -> Result<(), UblkError>
where
    R: Fn(),
    I: Fn() -> bool,
{
    let queue_fd = libublk::io::with_queue_ring(q, |ring| ring.as_raw_fd());
    let async_queue = smol::Async::new(unsafe { std::fs::File::from_raw_fd(queue_fd) })
        .map_err(|_| UblkError::OtherError(-libc::EINVAL))?;

    let poll_uring = || async {
        libublk::io::with_queue_ring_mut(q, |ring| ring.submit_and_wait(0))?;
        async_queue.readable().await.map_err(UblkError::IOError)?;
        Ok(false)
    };

    let reap_events = |_poll_timeout| {
        libublk::io::with_queue_ring_mut(q, |ring| {
            ublk_reap_events_with_handler(ring, |cqe| {
                ublk_wake_task(cqe.user_data(), cqe);
            })
        })
    };

    run_uring_tasks(poll_uring, reap_events, run_ops, is_done).await?;

    let _ = async_queue.into_inner().map(|f| f.into_raw_fd());
    Ok(())
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
        libc::sendto(fd, b"READY=1".as_ptr() as *const libc::c_void, 7, 0,
            &addr as *const _ as *const libc::sockaddr, len as u32);
        libc::close(fd);
    }
}

fn move_to_root_cgroup() {
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", format!("{}\n", std::process::id()));
}

fn run_async_queue(qid: u16, dev: &UblkDev, vsock_fd: RawFd) {
    let _ = libublk::io::ublk_init_task_ring(|cell| {
        if cell.get().is_none() {
            let ring = IoUring::<io_uring::squeue::Entry, io_uring::cqueue::Entry>::builder()
                .setup_cqsize(256)
                .setup_coop_taskrun()
                .build(256)
                .map_err(UblkError::IOError)?;
            cell.set(RefCell::new(ring))
                .map_err(|_| UblkError::OtherError(-libc::EEXIST))?;
        }
        Ok(())
    });

    let q = Rc::new(UblkQueue::new(qid, dev).unwrap());
    let depth = dev.dev_info.queue_depth as u16;
    let exec = Rc::new(smol::LocalExecutor::new());

    let mut tasks = Vec::new();
    for tag in 0..depth {
        let q_clone = q.clone();
        tasks.push(exec.spawn(async move {
            match io_task(&q_clone, tag, vsock_fd).await {
                Err(UblkError::QueueIsDown) | Ok(_) => {}
                Err(e) => msg!("io_task[{}] error: {:?}", tag, e),
            }
        }));
    }

    let exec2 = exec.clone();
    let q_ref = q.clone();
    smol::block_on(exec.run(async {
        let run_ops = || { while exec2.try_tick() {} };
        let is_done = || tasks.iter().all(|t| t.is_finished());
        if let Err(e) = poll_and_handle_rings(&q_ref, run_ops, is_done).await {
            msg!("event loop error: {:?}", e);
        }
    }));
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

    msg!("listening on vsock port {} (queues={}, async)", vsock_port, num_queues);
    let lsock = vsock_listen(vsock_port).expect("vsock listen failed");

    msg!("waiting for {} connections", num_queues);
    let connections: Arc<Mutex<Vec<(RawFd, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    for i in 0..num_queues {
        let fd = vsock_accept(lsock).expect("accept failed");
        let export_size = nbd_handshake_sync(fd).expect("handshake failed");
        msg!("connection {}: export_size={} MB", i, export_size / (1024 * 1024));
        connections.lock().unwrap().push((fd, export_size));
    }
    unsafe { libc::close(lsock); }

    let export_size = connections.lock().unwrap()[0].1;
    msg!("all {} connections ready", num_queues);

    let ctrl = UblkCtrlBuilder::default()
        .name("bcvk")
        .nr_queues(num_queues)
        .depth(1u16)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .expect("ublk ctrl build failed");

    let sz = export_size;
    let tgt_init = move |dev: &mut UblkDev| {
        dev.set_default_params(sz);
        Ok(())
    };

    let conns = connections.clone();
    let q_fn = move |qid: u16, dev: &UblkDev| {
        let (fd, _) = conns.lock().unwrap().remove(0);
        run_async_queue(qid, dev, fd);
    };

    let nq = num_queues;
    let res = ctrl.run_target(tgt_init, q_fn, move |ctrl| {
        msg!("/dev/ublkb{} ready ({} queues, async)", ctrl.dev_info().dev_id, nq);
        sd_notify_ready();
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN); }
        move_to_root_cgroup();
    });

    if let Err(e) = res {
        msg!("FAILED: {:?}", e);
        std::process::exit(1);
    }
}
