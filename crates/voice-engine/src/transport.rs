use std::future::Future;
use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

/// Datagram seam between the pipeline and the network. Adapters for other satellites
/// (Wyoming, ESPHome) or an io_uring backend implement this and leave the pipeline untouched.
pub trait Transport {
    fn recv(&mut self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, SocketAddr)>>;
    fn send(&mut self, buf: &[u8], to: SocketAddr) -> impl Future<Output = io::Result<()>>;
}

pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    /// `busy_poll_us` enables SO_BUSY_POLL on Linux and is ignored elsewhere. Must be called
    /// inside a tokio runtime.
    pub fn bind(addr: SocketAddr, recv_buffer: usize, busy_poll_us: Option<u32>) -> io::Result<Self> {
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_nonblocking(true)?;
        socket.set_recv_buffer_size(recv_buffer)?;
        if let Some(us) = busy_poll_us {
            set_busy_poll(&socket, us);
        }
        socket.bind(&addr.into())?;
        Ok(Self { socket: UdpSocket::from_std(socket.into())? })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// Best effort: before Linux 5.7 this needs CAP_NET_ADMIN, which the pod does not have.
#[cfg(target_os = "linux")]
fn set_busy_poll(socket: &Socket, us: u32) {
    use std::os::fd::AsRawFd;
    let value = us as libc::c_int;
    // SAFETY: valid fd, and the pointer/length describe a live c_int.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BUSY_POLL,
            std::ptr::from_ref(&value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        tracing::warn!(error = %io::Error::last_os_error(), "SO_BUSY_POLL not set");
    }
}

#[cfg(not(target_os = "linux"))]
fn set_busy_poll(_socket: &Socket, _us: u32) {}

impl Transport for UdpTransport {
    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    async fn send(&mut self, buf: &[u8], to: SocketAddr) -> io::Result<()> {
        self.socket.send_to(buf, to).await.map(|_| ())
    }
}
