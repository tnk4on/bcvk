//! SSH port forwarding: TCP proxy from localhost:PORT to VM_IP:22.
//!
//! Equivalent to macOS gvproxy's `expose_ssh_port()`.

#[cfg(target_os = "windows")]
use color_eyre::Result;
#[cfg(target_os = "windows")]
use std::sync::Arc;
#[cfg(target_os = "windows")]
use tokio::io::copy_bidirectional;
#[cfg(target_os = "windows")]
use tokio::net::TcpListener;
#[cfg(target_os = "windows")]
use tokio::sync::Notify;
#[cfg(target_os = "windows")]
use tokio::task::JoinHandle;
#[cfg(target_os = "windows")]
use tracing::{debug, info};

#[cfg(target_os = "windows")]
#[derive(Debug)]
pub struct SshForward {
    port: u16,
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

#[cfg(target_os = "windows")]
impl SshForward {
    pub async fn start(vm_ip: &str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(Notify::new());
        let vm_addr = format!("{}:22", vm_ip);

        info!("SSH forward: localhost:{} → {}", port, vm_addr);

        let stop_clone = stop.clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_clone.notified() => break,
                    result = listener.accept() => {
                        match result {
                            Ok((mut client, _)) => {
                                let addr = vm_addr.clone();
                                tokio::spawn(async move {
                                    match tokio::net::TcpStream::connect(&addr).await {
                                        Ok(mut server) => {
                                            let _ = copy_bidirectional(&mut client, &mut server).await;
                                        }
                                        Err(e) => {
                                            debug!("SSH forward connect to {} failed: {}", addr, e);
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                debug!("SSH forward accept error: {}", e);
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { port, stop, handle })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn stop(&self) {
        self.stop.notify_waiters();
    }
}

#[cfg(target_os = "windows")]
impl Drop for SshForward {
    fn drop(&mut self) {
        self.stop.notify_waiters();
        self.handle.abort();
    }
}
