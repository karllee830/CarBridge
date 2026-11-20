// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Sockets for the AirPlay sessions: blocking TCP streams and listeners, and UDP datagrams, on one local address.
//!
//! What the sockets send goes out marked DSCP CS5, 802.1d priority 5, which Wi-Fi drivers send in WMM's video access
//! category, ahead of the access point's other traffic.
//!
//! The TCP connections probe a peer that falls silent: one that stops answering, as an iPhone that leaves the access
//! point does without closing its connections, ends them within about [`PEER_TIMEOUT`], and their reads fail with
//! [`io::ErrorKind::TimedOut`].

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carbridge_airplay::net::{
    Datagram as DatagramSocket, Listener as StreamListener, Sockets as SocketFactory,
};
use carbridge_iap2::{Received, Stream};

/// How often a blocking accept looks for a closed listener or an expired timeout.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// DSCP CS5 in the IP header's type of service or traffic class byte.
const TRAFFIC_CLASS: i32 = 0xa0;
/// How long a TCP connection may go without word from the peer before it is probed, and how often it is probed then.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(5);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
/// How long a peer may leave the probes, or what was sent to it, unanswered before the connection is given up.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(15);
/// The unanswered probes that end a connection where [`PEER_TIMEOUT`] cannot be set directly.
const KEEPALIVE_PROBES: u32 =
    ((PEER_TIMEOUT.as_secs() - KEEPALIVE_IDLE.as_secs()) / KEEPALIVE_INTERVAL.as_secs()) as u32;

/// A TCP connection. Sends and receives may run on different threads.
pub struct Tcp {
    reader: Mutex<TcpStream>,
    writer: Mutex<TcpStream>,
}

impl Tcp {
    pub fn new(stream: TcpStream) -> io::Result<Tcp> {
        stream.set_nodelay(true)?;
        mark(&stream, stream.local_addr()?.ip());
        keep_alive(&stream)?;
        Ok(Tcp {
            writer: Mutex::new(stream.try_clone()?),
            reader: Mutex::new(stream),
        })
    }
}

impl Stream for Tcp {
    fn send(&self, data: &[u8]) -> io::Result<()> {
        self.writer.lock().unwrap().write_all(data)
    }

    fn receive(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Received> {
        let mut reader = self.reader.lock().unwrap();
        // Some systems refuse socket options once the peer has shut the connection down.
        match reader.set_read_timeout(Some(timeout.max(Duration::from_millis(1)))) {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => return Ok(Received::End),
            other => other?,
        }
        match reader.read(buffer) {
            Ok(0) => Ok(Received::End),
            Ok(n) => Ok(Received::Bytes(n)),
            Err(err) => read_failed(err),
        }
    }

    fn shutdown(&self) {
        let _ = self.writer.lock().unwrap().shutdown(Shutdown::Both);
    }
}

/// What a failed TCP read means: the read timeout ran out ([`io::ErrorKind::WouldBlock`]), or the connection failed,
/// as when the peer stopped answering ([`io::ErrorKind::TimedOut`]).
fn read_failed(err: io::Error) -> io::Result<Received> {
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(Received::Nothing)
    } else {
        Err(err)
    }
}

/// A TCP listener on an ephemeral port that admits one peer address, or any when it is unspecified.
pub struct Listener {
    listener: TcpListener,
    port: u16,
    peer: IpAddr,
    closed: AtomicBool,
}

impl Listener {
    pub fn bind(local: IpAddr, port: u16, peer: IpAddr) -> io::Result<Listener> {
        let listener = TcpListener::bind((local, port))?;
        listener.set_nonblocking(true)?;
        Ok(Listener {
            port: listener.local_addr()?.port(),
            listener,
            peer,
            closed: AtomicBool::new(false),
        })
    }

    /// Waits up to `timeout` for a connection from the admitted peer.
    pub fn accept_tcp(&self, timeout: Duration) -> io::Result<Option<(TcpStream, SocketAddr)>> {
        let end = Instant::now() + timeout;
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    if !self.peer.is_unspecified() && addr.ip() != self.peer {
                        continue;
                    }
                    stream.set_nonblocking(false)?;
                    return Ok(Some((stream, addr)));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }
            let now = Instant::now();
            if now >= end {
                return Ok(None);
            }
            std::thread::sleep(ACCEPT_POLL.min(end - now));
        }
    }
}

impl StreamListener for Listener {
    fn port(&self) -> u16 {
        self.port
    }

    fn accept(&self, timeout: Duration) -> io::Result<Option<Arc<dyn Stream>>> {
        Ok(match self.accept_tcp(timeout)? {
            Some((stream, _)) => Some(Arc::new(Tcp::new(stream)?)),
            None => None,
        })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// A UDP socket bound to an ephemeral port.
pub struct Datagram {
    socket: UdpSocket,
    port: u16,
}

impl DatagramSocket for Datagram {
    fn port(&self) -> u16 {
        self.port
    }

    fn send_to(&self, data: &[u8], peer: SocketAddr) -> io::Result<()> {
        self.socket.send_to(data, peer).map(drop)
    }

    fn receive_from(
        &self,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        self.socket
            .set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
        match self.socket.recv_from(buffer) {
            Ok(received) => Ok(Some(received)),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(err) => Err(err),
        }
    }

    fn close(&self) {}
}

#[cfg(unix)]
fn set_receive_buffer(socket: &UdpSocket, bytes: usize) -> io::Result<()> {
    let value = libc::c_int::try_from(bytes).unwrap_or(libc::c_int::MAX);
    set_option(socket, libc::SOL_SOCKET, libc::SO_RCVBUF, value)
}

/// Marks what `socket`, on the local address `local`, sends with [`TRAFFIC_CLASS`]. A socket that cannot be marked
/// still works, unmarked.
#[cfg(unix)]
fn mark(socket: &impl std::os::fd::AsRawFd, local: IpAddr) {
    let _ = match local {
        IpAddr::V4(_) => set_option(socket, libc::IPPROTO_IP, libc::IP_TOS, TRAFFIC_CLASS),
        IpAddr::V6(_) => set_option(socket, libc::IPPROTO_IPV6, libc::IPV6_TCLASS, TRAFFIC_CLASS),
    };
}

/// Has the connection probe a silent peer and give it up once the peer leaves the probes, or data sent to it,
/// unanswered for [`PEER_TIMEOUT`].
#[cfg(unix)]
fn keep_alive(stream: &TcpStream) -> io::Result<()> {
    let seconds = |d: Duration| libc::c_int::try_from(d.as_secs()).unwrap_or(libc::c_int::MAX);
    set_option(stream, libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    set_option(
        stream,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPIDLE,
        seconds(KEEPALIVE_IDLE),
    )?;
    #[cfg(target_vendor = "apple")]
    set_option(
        stream,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPALIVE,
        seconds(KEEPALIVE_IDLE),
    )?;
    set_option(
        stream,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPINTVL,
        seconds(KEEPALIVE_INTERVAL),
    )?;
    set_option(
        stream,
        libc::IPPROTO_TCP,
        libc::TCP_KEEPCNT,
        libc::c_int::try_from(KEEPALIVE_PROBES).unwrap_or(libc::c_int::MAX),
    )?;
    // Bounds the probes and the retransmissions of unanswered data alike.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    set_option(
        stream,
        libc::IPPROTO_TCP,
        libc::TCP_USER_TIMEOUT,
        libc::c_int::try_from(PEER_TIMEOUT.as_millis()).unwrap_or(libc::c_int::MAX),
    )?;
    Ok(())
}

#[cfg(unix)]
fn set_option(
    socket: &impl std::os::fd::AsRawFd,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    // SAFETY: the descriptor is the socket's own, and the option value is a c_int that lives through the call.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            level,
            name,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Opens listeners and datagram sockets on one local address, the one the iPhone reaches.
pub struct Sockets {
    local: IpAddr,
}

impl Sockets {
    pub fn new(local: IpAddr) -> Sockets {
        Sockets { local }
    }
}

impl SocketFactory for Sockets {
    fn listen(&self, peer: IpAddr) -> io::Result<Arc<dyn StreamListener>> {
        Ok(Arc::new(Listener::bind(self.local, 0, peer)?))
    }

    fn datagram(
        &self,
        _peer: IpAddr,
        receive_buffer: Option<usize>,
    ) -> io::Result<Arc<dyn DatagramSocket>> {
        let socket = UdpSocket::bind((self.local, 0))?;
        mark(&socket, self.local);
        if let Some(bytes) = receive_buffer {
            // The kernel may cap the size, or refuse it; the socket works with what it has.
            let _ = set_receive_buffer(&socket, bytes);
        }
        Ok(Arc::new(Datagram {
            port: socket.local_addr()?.port(),
            socket,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    fn option(
        socket: &impl std::os::fd::AsRawFd,
        level: libc::c_int,
        name: libc::c_int,
    ) -> libc::c_int {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: the descriptor is the socket's own, and value and len live through the call.
        let result = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                level,
                name,
                (&mut value as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        assert_eq!(result, 0);
        value
    }

    fn traffic_class(socket: &impl std::os::fd::AsRawFd) -> libc::c_int {
        option(socket, libc::IPPROTO_IP, libc::IP_TOS)
    }

    #[test]
    fn connections_probe_a_silent_peer() {
        let listener = TcpListener::bind((LOCAL, 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let tcp = Tcp::new(stream).unwrap();
        let socket = &*tcp.reader.lock().unwrap();
        assert_ne!(option(socket, libc::SOL_SOCKET, libc::SO_KEEPALIVE), 0);
        assert_eq!(option(socket, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL), 2);
        assert_eq!(option(socket, libc::IPPROTO_TCP, libc::TCP_KEEPCNT), 5);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(option(socket, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE), 5);
            assert_eq!(
                option(socket, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT),
                15_000
            );
        }
        #[cfg(target_vendor = "apple")]
        assert_eq!(option(socket, libc::IPPROTO_TCP, libc::TCP_KEEPALIVE), 5);
    }

    #[test]
    fn a_timed_out_connection_is_an_error_not_a_quiet_read() {
        assert!(matches!(
            read_failed(io::ErrorKind::WouldBlock.into()),
            Ok(Received::Nothing)
        ));
        let err = read_failed(io::Error::from_raw_os_error(libc::ETIMEDOUT)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(read_failed(io::ErrorKind::ConnectionReset.into()).is_err());
    }

    #[test]
    fn sockets_are_marked() {
        let listener = TcpListener::bind((LOCAL, 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let tcp = Tcp::new(stream).unwrap();
        assert_eq!(traffic_class(&*tcp.writer.lock().unwrap()), TRAFFIC_CLASS);
        let socket = UdpSocket::bind((LOCAL, 0)).unwrap();
        mark(&socket, LOCAL);
        assert_eq!(traffic_class(&socket), TRAFFIC_CLASS);
    }

    #[test]
    fn stream_round_trip() {
        let sockets = Sockets::new(LOCAL);
        let listener = sockets.listen(LOCAL).unwrap();
        let port = listener.port();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect((LOCAL, port)).unwrap();
            stream.write_all(b"ping").unwrap();
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).unwrap();
            buf
        });
        let server = listener.accept(Duration::from_secs(5)).unwrap().unwrap();
        let mut buf = [0u8; 16];
        let Received::Bytes(n) = server.receive(&mut buf, Duration::from_secs(5)).unwrap() else {
            panic!("data")
        };
        assert_eq!(&buf[..n], b"ping");
        server.send(b"pong").unwrap();
        assert_eq!(&client.join().unwrap(), b"pong");
        assert!(matches!(
            server.receive(&mut buf, Duration::from_millis(20)).unwrap(),
            Received::End | Received::Nothing
        ));
    }

    #[test]
    fn accept_times_out_and_closes() {
        let listener = Listener::bind(LOCAL, 0, LOCAL).unwrap();
        assert!(StreamListener::accept(&listener, Duration::from_millis(30))
            .unwrap()
            .is_none());
        StreamListener::close(&listener);
        assert!(StreamListener::accept(&listener, Duration::from_secs(5))
            .unwrap()
            .is_none());
    }

    #[test]
    fn receive_times_out() {
        let listener = Listener::bind(LOCAL, 0, IpAddr::V4(Ipv4Addr::UNSPECIFIED)).unwrap();
        let port = StreamListener::port(&listener);
        let _client = TcpStream::connect((LOCAL, port)).unwrap();
        let server = StreamListener::accept(&listener, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut buf = [0u8; 16];
        assert!(matches!(
            server.receive(&mut buf, Duration::from_millis(20)).unwrap(),
            Received::Nothing
        ));
    }

    #[test]
    fn datagrams() {
        let sockets = Sockets::new(LOCAL);
        let receiver = sockets.datagram(LOCAL, Some(1 << 20)).unwrap();
        let sender = sockets.datagram(LOCAL, None).unwrap();
        sender
            .send_to(b"rtp", SocketAddr::new(LOCAL, receiver.port()))
            .unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = receiver
            .receive_from(&mut buf, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"rtp");
        assert_eq!(from.port(), sender.port());
        receiver.send_to(b"ntp", from).unwrap();
        let (n, _) = sender
            .receive_from(&mut buf, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ntp");
        assert!(receiver
            .receive_from(&mut buf, Duration::from_millis(20))
            .unwrap()
            .is_none());
    }
}
