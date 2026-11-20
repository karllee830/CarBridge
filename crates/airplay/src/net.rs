// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from the socket handling of DiPlay's
// airplay/AirPlaySession.kt, airplay/ScreenStream.kt, airplay/AudioStream.kt and airplay/IapTunnel.kt (DiPlay commit
// 615b32c).

//! The sockets a session opens for its streams, supplied by the host so that it can set them up as its network
//! needs (keep-alive, traffic class, receive buffers). [`StdSockets`] is a plain implementation.
//!
//! Every listener takes connections from the session's iPhone only. A connection is an iAP2 [`Stream`]: a
//! receive that finds nothing in time is [`Received::Nothing`], and an error, such as a peer that stopped
//! answering, ends whatever runs on it.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use carbridge_iap2::{Received, Stream};

/// A TCP listener on a port of its own.
pub trait Listener: Send + Sync {
    fn port(&self) -> u16;

    /// The next connection from the peer; `None` when `timeout` passed or the listener was closed.
    fn accept(&self, timeout: Duration) -> io::Result<Option<Arc<dyn Stream>>>;

    fn close(&self);
}

/// A UDP socket on a port of its own.
pub trait Datagram: Send + Sync {
    fn port(&self) -> u16;

    fn send_to(&self, bytes: &[u8], to: SocketAddr) -> io::Result<()>;

    /// The next datagram and its sender; `None` when `timeout` passed.
    fn receive_from(
        &self,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Option<(usize, SocketAddr)>>;

    fn close(&self);
}

/// Opens the sockets of a session with `peer`, the iPhone.
pub trait Sockets: Send + Sync {
    /// Listens for TCP connections from `peer` on a new port.
    fn listen(&self, peer: IpAddr) -> io::Result<Arc<dyn Listener>>;

    /// A UDP socket on a new port for traffic with `peer`, asking for `receive_buffer` bytes of kernel buffer.
    fn datagram(
        &self,
        peer: IpAddr,
        receive_buffer: Option<usize>,
    ) -> io::Result<Arc<dyn Datagram>>;
}

/// The same address, whether written as IPv4 or as IPv4-mapped IPv6.
pub fn same_host(a: IpAddr, b: IpAddr) -> bool {
    let canonical = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    canonical(a) == canonical(b)
}

/// Sockets from the standard library, bound on all addresses.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdSockets;

const ACCEPT_POLL: Duration = Duration::from_millis(20);

impl Sockets for StdSockets {
    fn listen(&self, peer: IpAddr) -> io::Result<Arc<dyn Listener>> {
        let any: IpAddr = if peer.is_ipv4() {
            IpAddr::from([0, 0, 0, 0])
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        let listener = TcpListener::bind((any, 0))?;
        listener.set_nonblocking(true)?;
        Ok(Arc::new(StdListener {
            port: listener.local_addr()?.port(),
            listener,
            peer,
            closed: AtomicBool::new(false),
        }))
    }

    fn datagram(
        &self,
        peer: IpAddr,
        _receive_buffer: Option<usize>,
    ) -> io::Result<Arc<dyn Datagram>> {
        let any: IpAddr = if peer.is_ipv4() {
            IpAddr::from([0, 0, 0, 0])
        } else {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        };
        let socket = UdpSocket::bind((any, 0))?;
        Ok(Arc::new(StdDatagram {
            port: socket.local_addr()?.port(),
            socket,
            closed: AtomicBool::new(false),
        }))
    }
}

struct StdListener {
    listener: TcpListener,
    port: u16,
    peer: IpAddr,
    closed: AtomicBool,
}

impl Listener for StdListener {
    fn port(&self) -> u16 {
        self.port
    }

    fn accept(&self, timeout: Duration) -> io::Result<Option<Arc<dyn Stream>>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            match self.listener.accept() {
                Ok((stream, from)) if same_host(from.ip(), self.peer) => {
                    stream.set_nonblocking(false)?;
                    return Ok(Some(Arc::new(StdStream::new(stream)?)));
                }
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            std::thread::sleep(ACCEPT_POLL.min(deadline - now));
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// A TCP connection as a [`Stream`].
pub struct StdStream {
    reader: Mutex<TcpStream>,
    writer: Mutex<TcpStream>,
}

impl StdStream {
    pub fn new(stream: TcpStream) -> io::Result<StdStream> {
        stream.set_nodelay(true)?;
        Ok(StdStream {
            writer: Mutex::new(stream.try_clone()?),
            reader: Mutex::new(stream),
        })
    }
}

impl Stream for StdStream {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .write_all(bytes)
    }

    fn receive(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Received> {
        let mut reader = self.reader.lock().unwrap_or_else(|p| p.into_inner());
        reader.set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match reader.read(buffer) {
            Ok(0) => Ok(Received::End),
            Ok(count) => Ok(Received::Bytes(count)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(Received::Nothing)
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(Received::Nothing),
            Err(error) => Err(error),
        }
    }

    fn shutdown(&self) {
        let _ = self
            .writer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .shutdown(Shutdown::Both);
    }
}

struct StdDatagram {
    socket: UdpSocket,
    port: u16,
    closed: AtomicBool,
}

impl Datagram for StdDatagram {
    fn port(&self) -> u16 {
        self.port
    }

    fn send_to(&self, bytes: &[u8], to: SocketAddr) -> io::Result<()> {
        self.socket.send_to(bytes, to).map(|_| ())
    }

    fn receive_from(
        &self,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        if self.closed.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.socket
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match self.socket.recv_from(buffer) {
            Ok(received) => Ok(Some(received)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// Reads exactly `buffer.len()` bytes; `Ok(false)` when the stream ended first or `stop` says so.
pub fn read_exact(
    stream: &dyn Stream,
    buffer: &mut [u8],
    stop: &dyn Fn() -> bool,
) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        if stop() {
            return Ok(false);
        }
        match stream.receive(&mut buffer[filled..], Duration::from_millis(200))? {
            Received::Bytes(count) => filled += count,
            Received::Nothing => {}
            Received::End => return Ok(false),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listener_takes_its_peer() {
        let listener = StdSockets.listen(IpAddr::from([127, 0, 0, 1])).unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", listener.port())).unwrap();
        let stream = listener.accept(Duration::from_secs(2)).unwrap().unwrap();
        client.write_all(b"hello").unwrap();
        let mut buffer = [0; 5];
        assert!(read_exact(stream.as_ref(), &mut buffer, &|| false).unwrap());
        assert_eq!(&buffer, b"hello");
        stream.send(b"back").unwrap();
        let mut back = [0; 4];
        client.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"back");
        assert_eq!(
            stream
                .receive(&mut buffer, Duration::from_millis(20))
                .unwrap(),
            Received::Nothing
        );
        drop(client);
        assert_eq!(
            stream.receive(&mut buffer, Duration::from_secs(2)).unwrap(),
            Received::End
        );
        assert!(listener
            .accept(Duration::from_millis(30))
            .unwrap()
            .is_none());
        listener.close();
    }

    #[test]
    fn datagrams_round_trip() {
        let socket = StdSockets
            .datagram(IpAddr::from([127, 0, 0, 1]), None)
            .unwrap();
        let other = UdpSocket::bind("127.0.0.1:0").unwrap();
        other
            .send_to(b"ping", ("127.0.0.1", socket.port()))
            .unwrap();
        let mut buffer = [0; 16];
        let (count, from) = socket
            .receive_from(&mut buffer, Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..count], b"ping");
        socket.send_to(b"pong", from).unwrap();
        let (count, _) = other.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..count], b"pong");
        assert!(socket
            .receive_from(&mut buffer, Duration::from_millis(20))
            .unwrap()
            .is_none());
    }

    #[test]
    fn mapped_addresses_are_the_same_host() {
        let v4 = IpAddr::from([192, 168, 1, 2]);
        let mapped = IpAddr::V6(Ipv6Addr::from([0, 0, 0, 0, 0, 0xffff, 0xc0a8, 0x0102]));
        assert!(same_host(v4, mapped));
        assert!(!same_host(v4, IpAddr::from([192, 168, 1, 3])));
    }
}
