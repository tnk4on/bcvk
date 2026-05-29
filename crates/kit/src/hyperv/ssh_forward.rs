//! SSH port forwarding: TCP proxy from localhost:PORT to VM_IP:22.
//!
//! Equivalent to macOS gvproxy's `expose_ssh_port()`.

use color_eyre::Result;
use std::sync::Arc;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info};

#[derive(Debug)]
pub struct SshForward {
    port: u16,
    stop: Arc<Notify>,
    handle: JoinHandle<()>,
}

impl SshForward {
    pub async fn start(vm_ip: &str) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        Self::start_with_listener(listener, vm_ip).await
    }

    pub async fn start_on_port(vm_ip: &str, port: u16) -> Result<Self> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
        Self::start_with_listener(listener, vm_ip).await
    }

    pub async fn start_on_ports(vm_ip: &str, host_port: u16, guest_port: u16) -> Result<Self> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", host_port)).await?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(Notify::new());
        let vm_addr = format!("{}:{}", vm_ip, guest_port);

        info!("port forward: localhost:{} → {}", port, vm_addr);

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
                                            debug!("port forward connect to {} failed: {}", addr, e);
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                debug!("port forward accept error: {}", e);
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { port, stop, handle })
    }

    async fn start_with_listener(listener: TcpListener, vm_ip: &str) -> Result<Self> {
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

impl Drop for SshForward {
    fn drop(&mut self) {
        self.stop.notify_waiters();
        self.handle.abort();
    }
}
