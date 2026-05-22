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
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

const NBD_REQUEST_MAGIC: u32 = 0x25609513;
const NBD_CMD_READ: u16 = 0;
const NBD_CMD_WRITE: u16 = 1;
const SOCKET_BUF_SIZE: i32 = 4 * 1024 * 1024;

macro_rules! msg {
    ($($arg:tt)*) => {{ eprintln!("ublk-vsock: {}", format!($($arg)*)) }};
}

// --- Completion slot: per-tag wake mechanism ---

struct CompletionSlot {
    result: Cell<Option<i32>>,
    waker: Cell<Option<Waker>>,
}

impl CompletionSlot {
    fn new() -> Self {
        Self { result: Cell::new(None), waker: Cell::new(None) }
    }
    fn complete(&self, res: i32) {
        self.result.set(Some(res));
        if let Some(w) = self.waker.take() { w.wake(); }
    }
    fn wait(&self) -> WaitCompletion<'_> {
        WaitCompletion { slot: self }
    }
}

struct WaitCompletion<'a> { slot: &'a CompletionSlot }
impl<'a> Future for WaitCompletion<'a> {
    type Output = i32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<i32> {
        match self.slot.result.take() {
            Some(r) => Poll::Ready(r),
            None => { self.slot.waker.set(Some(cx.waker().clone())); Poll::Pending }
        }
    }
}

// --- NBD request for send_task ---

struct NbdSendRequest {
    tag: u16,
    op: u32,
    offset: u64,
    len: usize,
    buf_ptr: *const u8,
    handle: u64,
}
unsafe impl Send for NbdSendRequest {}

// --- Networking helpers ---

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
            libc::close(fd); return Err(std::io::Error::last_os_error());
        }
        if listen(fd, 16) < 0 {
            libc::close(fd); return Err(std::io::Error::last_os_error());
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
    use std::io::{Read, Write};
    let mut stream = unsafe { UnixStream::from_raw_fd(fd) };
    let mut m = [0u8; 8]; let mut i = [0u8; 8]; let mut h = [0u8; 2];
    let readall = |s: &mut UnixStream, b: &mut [u8]| -> std::io::Result<()> {
        let mut d = 0;
        while d < b.len() { let n = s.read(&mut b[d..])?; if n == 0 { return Err(std::io::ErrorKind::UnexpectedEof.into()); } d += n; }
        Ok(())
    };
    readall(&mut stream, &mut m)?; readall(&mut stream, &mut i)?; readall(&mut stream, &mut h)?;
    stream.write_all(&1u32.to_be().to_ne_bytes())?;
    stream.write_all(&0x49484156454F5054u64.to_be().to_ne_bytes())?;
    stream.write_all(&1u32.to_be().to_ne_bytes())?;
    stream.write_all(&0u32.to_be().to_ne_bytes())?;
    let mut sz = [0u8; 8]; let mut tf = [0u8; 2]; let mut pad = [0u8; 124];
    readall(&mut stream, &mut sz)?; readall(&mut stream, &mut tf)?; readall(&mut stream, &mut pad)?;
    let _ = stream.into_raw_fd();
    Ok(u64::from_be_bytes(sz))
}

// --- io_uring async helpers ---

async fn uring_read_exact(q: &UblkQueue<'_>, fd: RawFd, buf: &mut [u8]) -> Result<(), UblkError> {
    let mut done = 0;
    while done < buf.len() {
        let sqe = io_uring::opcode::Read::new(
            io_uring::types::Fd(fd), buf[done..].as_mut_ptr(), (buf.len() - done) as u32,
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
            io_uring::types::Fd(fd), buf[done..].as_ptr(), (buf.len() - done) as u32,
        ).build();
        let n = q.ublk_submit_sqe(sqe).await;
        if n <= 0 { return Err(UblkError::OtherError(-(libc::EIO as i32))); }
        done += n as usize;
    }
    Ok(())
}

// --- Per-tag IO task ---

async fn io_task(
    q: &UblkQueue<'_>,
    tag: u16,
    buf: &IoBuf<u8>,
    slot: &CompletionSlot,
    send_tx: &async_channel::Sender<NbdSendRequest>,
    seq: &Cell<u32>,
) -> Result<(), UblkError> {
    q.submit_io_prep_cmd(tag, BufDesc::Slice(buf.as_slice()), 0, Some(buf)).await?;

    loop {
        let iod = q.get_iod(tag);
        let op = iod.op_flags & 0xff;
        let offset = iod.start_sector * 512;
        let bytes = (iod.nr_sectors as usize) * 512;

        let s = seq.get().wrapping_add(1);
        seq.set(s);
        let handle = ((tag as u64) << 32) | (s as u64);

        match op {
            sys::UBLK_IO_OP_READ | sys::UBLK_IO_OP_WRITE => {
                let _ = send_tx.send(NbdSendRequest {
                    tag, op, offset, len: bytes,
                    buf_ptr: buf.as_mut_ptr() as *const u8,
                    handle,
                }).await;

                let result = slot.wait().await;

                q.submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), result).await?;
            }
            _ => {
                q.submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), bytes as i32).await?;
            }
        }
    }
}

// --- Send task: serialize NBD requests to vsock ---

async fn send_task(
    q: &UblkQueue<'_>,
    vsock_fd: RawFd,
    rx: async_channel::Receiver<NbdSendRequest>,
) -> Result<(), UblkError> {
    while let Ok(req) = rx.recv().await {
        let mut hdr = [0u8; 28];
        hdr[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        let cmd = if req.op == sys::UBLK_IO_OP_READ { NBD_CMD_READ } else { NBD_CMD_WRITE };
        hdr[6..8].copy_from_slice(&cmd.to_be_bytes());
        hdr[8..16].copy_from_slice(&req.handle.to_be_bytes());
        hdr[16..24].copy_from_slice(&req.offset.to_be_bytes());
        hdr[24..28].copy_from_slice(&(req.len as u32).to_be_bytes());

        uring_write_all(q, vsock_fd, &hdr).await?;

        if req.op == sys::UBLK_IO_OP_WRITE {
            let data = unsafe { std::slice::from_raw_parts(req.buf_ptr, req.len) };
            uring_write_all(q, vsock_fd, data).await?;
        }
    }
    Ok(())
}

// --- Recv task: read NBD replies and dispatch to io_tasks ---

async fn recv_task(
    q: &UblkQueue<'_>,
    vsock_fd: RawFd,
    bufs: &[IoBuf<u8>],
    slots: &[CompletionSlot],
) -> Result<(), UblkError> {
    loop {
        let mut reply = [0u8; 16];
        uring_read_exact(q, vsock_fd, &mut reply).await?;

        let error = u32::from_be_bytes(reply[4..8].try_into().unwrap());
        let handle = u64::from_be_bytes(reply[8..16].try_into().unwrap());
        let tag = (handle >> 32) as u16;

        if error != 0 {
            slots[tag as usize].complete(-(libc::EIO as i32));
            continue;
        }

        // For READ replies, data follows the header
        // We read data into the tag's buffer, then complete
        let iod = q.get_iod(tag);
        let op = iod.op_flags & 0xff;
        let bytes = (iod.nr_sectors as usize) * 512;

        if op == sys::UBLK_IO_OP_READ {
            let buf_slice = unsafe {
                std::slice::from_raw_parts_mut(bufs[tag as usize].as_mut_ptr(), bytes)
            };
            match uring_read_exact(q, vsock_fd, buf_slice).await {
                Ok(()) => slots[tag as usize].complete(bytes as i32),
                Err(_) => slots[tag as usize].complete(-(libc::EIO as i32)),
            }
        } else {
            slots[tag as usize].complete(bytes as i32);
        }
    }
}

// --- Event loop ---

async fn poll_and_handle_queue(
    q: &UblkQueue<'_>,
    exec: &smol::LocalExecutor<'_>,
    tasks: &[smol::Task<()>],
) -> Result<(), UblkError> {
    let queue_fd = libublk::io::with_queue_ring(q, |ring| ring.as_raw_fd());
    let async_queue = smol::Async::new(unsafe { std::fs::File::from_raw_fd(queue_fd) })
        .map_err(|_| UblkError::OtherError(-libc::EINVAL))?;

    let poll_uring = || async {
        libublk::io::with_queue_ring_mut(q, |ring| ring.submit_and_wait(0))?;
        async_queue.readable().await.map_err(UblkError::IOError)?;
        Ok(false)
    };

    let reap_events = |_| {
        libublk::io::with_queue_ring_mut(q, |ring| {
            ublk_reap_events_with_handler(ring, |cqe| {
                ublk_wake_task(cqe.user_data(), cqe);
            })
        })
    };

    let run_ops = || { while exec.try_tick() {} };
    let is_done = || tasks.iter().all(|t| t.is_finished());

    run_uring_tasks(poll_uring, reap_events, run_ops, is_done).await?;

    let _ = async_queue.into_inner().map(|f| f.into_raw_fd());
    Ok(())
}

// --- Queue handler ---

fn run_async_queue(qid: u16, dev: &UblkDev, vsock_fd: RawFd) {
    let _ = libublk::io::ublk_init_task_ring(|cell| {
        if cell.get().is_none() {
            let ring = IoUring::<io_uring::squeue::Entry, io_uring::cqueue::Entry>::builder()
                .setup_cqsize(512)
                .setup_coop_taskrun()
                .build(512)
                .map_err(UblkError::IOError)?;
            cell.set(RefCell::new(ring))
                .map_err(|_| UblkError::OtherError(-libc::EEXIST))?;
        }
        Ok(())
    });

    let q = Rc::new(UblkQueue::new(qid, dev).unwrap());
    let depth = dev.dev_info.queue_depth as usize;

    let bufs: Rc<Vec<IoBuf<u8>>> = Rc::new((0..depth)
        .map(|_| IoBuf::new(dev.dev_info.max_io_buf_bytes as usize))
        .collect());
    let slots: Rc<Vec<CompletionSlot>> = Rc::new((0..depth).map(|_| CompletionSlot::new()).collect());
    let seq = Rc::new(Cell::new(0u32));

    let (send_tx, send_rx) = async_channel::bounded::<NbdSendRequest>(depth);

    let exec = Rc::new(smol::LocalExecutor::new());
    let mut tasks: Vec<smol::Task<()>> = Vec::new();

    for tag in 0..depth {
        let q_c = q.clone();
        let tx = send_tx.clone();
        let bufs_c = bufs.clone();
        let slots_c = slots.clone();
        let seq_c = seq.clone();
        tasks.push(exec.spawn(async move {
            match io_task(&q_c, tag as u16, &bufs_c[tag], &slots_c[tag], &tx, &seq_c).await {
                Err(UblkError::QueueIsDown) | Ok(_) => {}
                Err(e) => msg!("io_task[{}] error: {:?}", tag, e),
            }
        }));
    }
    drop(send_tx);

    let q_send = q.clone();
    tasks.push(exec.spawn(async move {
        if let Err(e) = send_task(&q_send, vsock_fd, send_rx).await {
            msg!("send_task error: {:?}", e);
        }
    }));

    let q_recv = q.clone();
    let bufs_recv = bufs.clone();
    let slots_recv = slots.clone();
    tasks.push(exec.spawn(async move {
        if let Err(e) = recv_task(&q_recv, vsock_fd, &bufs_recv, &slots_recv).await {
            msg!("recv_task error: {:?}", e);
        }
    }));

    smol::block_on(exec.run(async {
        if let Err(e) = poll_and_handle_queue(&q, &exec, &tasks).await {
            msg!("q{} event loop error: {:?}", qid, e);
        }
    }));
}

// --- Utilities ---

fn sd_notify_ready() {
    let sock_path = match std::env::var("NOTIFY_SOCKET") { Ok(p) => p, Err(_) => return };
    unsafe {
        let fd = socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0);
        if fd < 0 { return; }
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        addr.sun_family = libc::AF_UNIX as u16;
        let pb = sock_path.as_bytes();
        if pb[0] == b'@' { addr.sun_path[0] = 0; for (i,&b) in pb[1..].iter().enumerate() { if i+1 >= addr.sun_path.len() { break; } addr.sun_path[i+1] = b as i8; } }
        else { for (i,&b) in pb.iter().enumerate() { if i >= addr.sun_path.len()-1 { break; } addr.sun_path[i] = b as i8; } }
        let len = std::mem::size_of::<libc::sa_family_t>() + pb.len();
        libc::sendto(fd, b"READY=1".as_ptr() as *const _, 7, 0, &addr as *const _ as *const _, len as u32);
        libc::close(fd);
    }
}

fn move_to_root_cgroup() {
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", format!("{}\n", std::process::id()));
}

// --- Main ---

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("Usage: ublk-vsock <device> <vsock_port> [num_queues]");
        std::process::exit(1);
    }
    let vsock_port: u32 = args[2].parse().expect("invalid port");
    let num_queues: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);

    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN); }

    msg!("listening on vsock port {} (queues={}, pipelined)", vsock_port, num_queues);
    let lsock = vsock_listen(vsock_port).expect("vsock listen failed");

    let connections: Arc<Mutex<Vec<(RawFd, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    for i in 0..num_queues {
        let fd = vsock_accept(lsock).expect("accept failed");
        let export_size = nbd_handshake_sync(fd).expect("handshake failed");
        msg!("connection {}: export_size={} MB", i, export_size / (1024 * 1024));
        connections.lock().unwrap().push((fd, export_size));
    }
    unsafe { libc::close(lsock); }

    let export_size = connections.lock().unwrap()[0].1;

    let ctrl = UblkCtrlBuilder::default()
        .name("bcvk")
        .nr_queues(num_queues)
        .depth(64u16)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .expect("ublk ctrl build failed");

    let sz = export_size;
    let tgt_init = move |dev: &mut UblkDev| { dev.set_default_params(sz); Ok(()) };

    let conns = connections.clone();
    let q_fn = move |qid: u16, dev: &UblkDev| {
        let (fd, _) = conns.lock().unwrap().remove(0);
        run_async_queue(qid, dev, fd);
    };

    let nq = num_queues;
    let res = ctrl.run_target(tgt_init, q_fn, move |ctrl| {
        msg!("/dev/ublkb{} ready ({} queues, pipelined)", ctrl.dev_info().dev_id, nq);
        sd_notify_ready();
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN); }
        move_to_root_cgroup();
    });

    if let Err(e) = res { msg!("FAILED: {:?}", e); std::process::exit(1); }
}
