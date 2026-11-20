// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The kernel's Bluetooth management interface, for settings bluetoothd does not offer over D-Bus, and the links the
//! kernel holds.

use std::io;

/// Sets the major and minor class of device of the adapter `hciN`; the kernel adds the service classes of the
/// registered profiles. `minor` is the class's low byte, its two format bits zero.
pub fn set_device_class(adapter: &str, major: u8, minor: u8) -> io::Result<()> {
    command(
        adapter,
        OP_SET_DEV_CLASS,
        &[major, minor],
        "set device class",
    )
    .map(drop)
}

/// A link the adapter holds to a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The device's address, most significant byte first, as it is written.
    pub address: [u8; 6],
    /// The link is LE, to a public or a random address; else it is BR/EDR.
    pub le: bool,
}

/// The links the adapter `hciN` holds now.
pub fn connections(adapter: &str) -> io::Result<Vec<Connection>> {
    let reply = command(adapter, OP_GET_CONNECTIONS, &[], "get connections")?;
    parse_connections(&reply)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "get connections: short reply"))
}

/// The reply to Get Connections: a count, then each device's address, least significant byte first, and its type.
fn parse_connections(reply: &[u8]) -> Option<Vec<Connection>> {
    let count = usize::from(u16::from_le_bytes([*reply.first()?, *reply.get(1)?]));
    let entries = reply.get(2..2 + count * 7)?;
    Some(
        entries
            .chunks_exact(7)
            .map(|entry| {
                let mut address = [0u8; 6];
                for (i, byte) in address.iter_mut().enumerate() {
                    *byte = entry[5 - i];
                }
                Connection {
                    address,
                    le: entry[6] != ADDRESS_BR_EDR,
                }
            })
            .collect(),
    )
}

/// Adds `uuid` (a 128-bit UUID as text) to the adapter's service list with the service classes `service_hint`,
/// the class of device's bits 16 to 23: 0x20 is Audio. Profiles bluetoothd registers for unknown UUIDs add no
/// service class of their own.
pub fn add_uuid(adapter: &str, uuid: &str, service_hint: u8) -> io::Result<()> {
    let mut params = uuid_le(uuid)?.to_vec();
    params.push(service_hint);
    command(adapter, OP_ADD_UUID, &params, "add UUID").map(drop)
}

/// A 128-bit UUID's bytes in the order the management interface takes them: least significant first.
fn uuid_le(uuid: &str) -> io::Result<[u8; 16]> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "bad UUID");
    if hex.len() != 32 {
        return Err(bad());
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let j = 15 - i;
        *byte = u8::from_str_radix(hex.get(2 * j..2 * j + 2).ok_or_else(bad)?, 16)
            .map_err(|_| bad())?;
    }
    Ok(bytes)
}

const OP_ADD_UUID: u16 = 0x0010;
const OP_SET_DEV_CLASS: u16 = 0x000e;
const OP_GET_CONNECTIONS: u16 = 0x0015;
/// The address type of a BR/EDR device; 1 and 2 are LE public and LE random.
const ADDRESS_BR_EDR: u8 = 0;

/// Sends one management command for the adapter `hciN`, waits for its reply, and returns the reply's parameters.
#[cfg(target_os = "linux")]
fn command(adapter: &str, opcode: u16, params: &[u8], what: &str) -> io::Result<Vec<u8>> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    const BTPROTO_HCI: libc::c_int = 1;
    const HCI_DEV_NONE: u16 = 0xffff;
    const HCI_CHANNEL_CONTROL: u16 = 3;
    const EV_CMD_COMPLETE: u16 = 0x0001;
    const EV_CMD_STATUS: u16 = 0x0002;
    const TIMEOUT: Duration = Duration::from_secs(2);

    #[repr(C)]
    struct SockaddrHci {
        family: libc::sa_family_t,
        dev: u16,
        channel: u16,
    }

    let index: u16 = adapter
        .strip_prefix("hci")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "adapter is not hciN"))?;

    // SAFETY: plain socket calls on a descriptor owned by `fd`; the buffers outlive the calls.
    unsafe {
        let raw = libc::socket(
            libc::AF_BLUETOOTH,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            BTPROTO_HCI,
        );
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(raw);
        let addr = SockaddrHci {
            family: libc::AF_BLUETOOTH as libc::sa_family_t,
            dev: HCI_DEV_NONE,
            channel: HCI_CHANNEL_CONTROL,
        };
        if libc::bind(
            fd.as_raw_fd(),
            &addr as *const SockaddrHci as *const libc::sockaddr,
            std::mem::size_of::<SockaddrHci>() as libc::socklen_t,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        let tv = libc::timeval {
            tv_sec: TIMEOUT.as_secs() as _,
            tv_usec: 0,
        };
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );

        let mut cmd = Vec::with_capacity(6 + params.len());
        cmd.extend_from_slice(&opcode.to_le_bytes());
        cmd.extend_from_slice(&index.to_le_bytes());
        cmd.extend_from_slice(&(params.len() as u16).to_le_bytes());
        cmd.extend_from_slice(params);
        if libc::write(
            fd.as_raw_fd(),
            cmd.as_ptr() as *const libc::c_void,
            cmd.len(),
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }

        let deadline = Instant::now() + TIMEOUT;
        let mut buf = [0u8; 512];
        while Instant::now() < deadline {
            let n = libc::read(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            );
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let ev = &buf[..n as usize];
            if ev.len() < 9 {
                continue;
            }
            let event = u16::from_le_bytes([ev[0], ev[1]]);
            let ev_index = u16::from_le_bytes([ev[2], ev[3]]);
            let ev_opcode = u16::from_le_bytes([ev[6], ev[7]]);
            if (event != EV_CMD_COMPLETE && event != EV_CMD_STATUS)
                || ev_index != index
                || ev_opcode != opcode
            {
                continue;
            }
            return match ev[8] {
                // Command Complete carries the reply's parameters after the status.
                0 => Ok(ev.get(9..).unwrap_or_default().to_vec()),
                status => Err(io::Error::other(format!(
                    "{what}: management status {status:#04x}"
                ))),
            };
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{what}: no reply"),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn command(_adapter: &str, _opcode: u16, _params: &[u8], _what: &str) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Bluetooth management needs Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_bytes_least_significant_first() {
        let bytes = uuid_le("ec884348-cd41-40a2-9727-575d50bf1fd3").unwrap();
        assert_eq!(bytes[0], 0xd3);
        assert_eq!(bytes[15], 0xec);
        assert!(uuid_le("ec884348").is_err());
    }

    #[test]
    fn connections_from_the_reply() {
        let reply = [
            2, 0, // two links
            0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0, // BR/EDR
            0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 2, // LE random
        ];
        assert_eq!(
            parse_connections(&reply),
            Some(vec![
                Connection {
                    address: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
                    le: false
                },
                Connection {
                    address: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
                    le: true
                },
            ])
        );
        assert_eq!(parse_connections(&[0, 0]), Some(vec![]));
        assert_eq!(parse_connections(&[1, 0, 1, 2, 3]), None);
        assert_eq!(parse_connections(&[]), None);
    }
}
