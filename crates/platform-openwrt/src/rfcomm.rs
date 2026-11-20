// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! An RFCOMM service of a device, opened directly: its channel read from the device's SDP records, then an RFCOMM
//! connection to it. This works when bluetoothd does not know the device's services, as when its own service
//! discovery after a pairing was cut short.

// The sockets exist on Linux only; elsewhere only the tests use the SDP encoding.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io;
use std::os::fd::OwnedFd;
use std::time::Duration;

const SERVICE_SEARCH_ATTRIBUTE_REQUEST: u8 = 0x06;
const SERVICE_SEARCH_ATTRIBUTE_RESPONSE: u8 = 0x07;
const ATTR_PROTOCOL_DESCRIPTOR_LIST: u16 = 0x0004;
const RFCOMM_UUID16: u16 = 0x0003;
/// The most bytes of attribute lists asked for in one response.
const MAX_ATTRIBUTE_BYTES: u16 = 0x0400;
/// The most responses read for one query, against a device that keeps answering with a continuation.
const MAX_RESPONSES: u16 = 16;

/// The RFCOMM channel of the service `uuid` (a 128-bit UUID as text) of the device at `address`
/// (`AA:BB:CC:DD:EE:FF`), from its SDP records. Fails with `NotFound` when the device does not offer it. Waits for
/// up to `timeout` for each step.
#[cfg(target_os = "linux")]
pub fn find_channel(address: &str, uuid: &str, timeout: Duration) -> io::Result<u8> {
    let bdaddr = parse_address(address)?;
    let uuid = parse_uuid(uuid)?;
    let sdp = linux::connect_sdp(bdaddr, timeout)?;
    let mut lists = Vec::new();
    let mut continuation: Vec<u8> = Vec::new();
    for transaction in 1..=MAX_RESPONSES {
        linux::send(&sdp, &request(transaction, &uuid, &continuation))?;
        let reply = linux::recv(&sdp)?;
        let (part, next) = response(&reply, transaction)?;
        lists.extend_from_slice(part);
        if next.is_empty() {
            return rfcomm_channel(&lists)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "the device does not offer the service",
                )
            });
        }
        continuation = next.to_vec();
    }
    Err(invalid("too many continued responses"))
}

/// Opens an encrypted RFCOMM connection to `channel` of the paired device at `address`, waiting for up to
/// `timeout`. The socket blocks and has no timeouts once connected.
#[cfg(target_os = "linux")]
pub fn connect(address: &str, channel: u8, timeout: Duration) -> io::Result<OwnedFd> {
    linux::connect_rfcomm(parse_address(address)?, channel, timeout)
}

#[cfg(not(target_os = "linux"))]
pub fn find_channel(_address: &str, _uuid: &str, _timeout: Duration) -> io::Result<u8> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SDP needs Linux",
    ))
}

#[cfg(not(target_os = "linux"))]
pub fn connect(_address: &str, _channel: u8, _timeout: Duration) -> io::Result<OwnedFd> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "RFCOMM needs Linux",
    ))
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("SDP: {what}"))
}

/// The address's bytes in the order the kernel takes them: least significant first.
fn parse_address(address: &str) -> io::Result<[u8; 6]> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "bad Bluetooth address");
    let parts: Vec<u8> = address
        .split(':')
        .map(|part| u8::from_str_radix(part, 16))
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    let mut bytes: [u8; 6] = parts.try_into().map_err(|_| bad())?;
    bytes.reverse();
    Ok(bytes)
}

/// A 128-bit UUID's bytes, most significant first, as SDP carries them.
fn parse_uuid(uuid: &str) -> io::Result<[u8; 16]> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "bad UUID");
    if hex.len() != 32 {
        return Err(bad());
    }
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2).ok_or_else(bad)?, 16)
            .map_err(|_| bad())?;
    }
    Ok(bytes)
}

/// A request for the protocol descriptor list of the records of the service `uuid`, carrying the continuation
/// state of the previous response.
fn request(transaction: u16, uuid: &[u8; 16], continuation: &[u8]) -> Vec<u8> {
    // Service search pattern: a sequence of one 128-bit UUID.
    let mut params = vec![0x35, 0x11, 0x1c];
    params.extend_from_slice(uuid);
    params.extend_from_slice(&MAX_ATTRIBUTE_BYTES.to_be_bytes());
    // Attribute ID list: a sequence of one attribute ID.
    params.extend_from_slice(&[0x35, 0x03, 0x09]);
    params.extend_from_slice(&ATTR_PROTOCOL_DESCRIPTOR_LIST.to_be_bytes());
    params.push(continuation.len() as u8);
    params.extend_from_slice(continuation);

    let mut pdu = vec![SERVICE_SEARCH_ATTRIBUTE_REQUEST];
    pdu.extend_from_slice(&transaction.to_be_bytes());
    pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
    pdu.extend_from_slice(&params);
    pdu
}

/// The attribute list bytes of a response and its continuation state, empty when the response is the last.
fn response(pdu: &[u8], transaction: u16) -> io::Result<(&[u8], &[u8])> {
    if pdu.len() < 7 {
        return Err(invalid("short response"));
    }
    if pdu[0] != SERVICE_SEARCH_ATTRIBUTE_RESPONSE {
        return Err(invalid(&format!("response {:#04x}", pdu[0])));
    }
    if u16::from_be_bytes([pdu[1], pdu[2]]) != transaction {
        return Err(invalid("wrong transaction"));
    }
    let truncated = || invalid("truncated response");
    let params = pdu
        .get(5..5 + usize::from(u16::from_be_bytes([pdu[3], pdu[4]])))
        .ok_or_else(truncated)?;
    let count = usize::from(u16::from_be_bytes([
        *params.first().ok_or_else(truncated)?,
        *params.get(1).ok_or_else(truncated)?,
    ]));
    let lists = params.get(2..2 + count).ok_or_else(truncated)?;
    let rest = &params[2 + count..];
    let length = usize::from(*rest.first().ok_or_else(truncated)?);
    let continuation = rest.get(1..1 + length).ok_or_else(truncated)?;
    Ok((lists, continuation))
}

/// One SDP data element: its type and its value's bytes.
struct Element<'a> {
    kind: u8,
    value: &'a [u8],
}

const KIND_UINT: u8 = 1;
const KIND_UUID: u8 = 3;
const KIND_SEQUENCE: u8 = 6;

/// Splits the first data element off `data`.
fn element(data: &[u8]) -> io::Result<(Element<'_>, &[u8])> {
    let truncated = || invalid("truncated data element");
    let header = *data.first().ok_or_else(truncated)?;
    let kind = header >> 3;
    let (length, start) = match header & 0x07 {
        _ if kind == 0 => (0, 1),
        size @ 0..=4 => (1usize << size, 1),
        5 => (usize::from(*data.get(1).ok_or_else(truncated)?), 2),
        6 => {
            let bytes = data.get(1..3).ok_or_else(truncated)?;
            (usize::from(u16::from_be_bytes([bytes[0], bytes[1]])), 3)
        }
        _ => {
            let bytes = data.get(1..5).ok_or_else(truncated)?;
            (
                u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
                5,
            )
        }
    };
    let value = data.get(start..start + length).ok_or_else(truncated)?;
    Ok((Element { kind, value }, &data[start + length..]))
}

/// The elements of a sequence's value.
fn elements(mut data: &[u8]) -> io::Result<Vec<Element<'_>>> {
    let mut list = Vec::new();
    while !data.is_empty() {
        let (item, rest) = element(data)?;
        list.push(item);
        data = rest;
    }
    Ok(list)
}

/// Whether a UUID element is RFCOMM's, in its 16-, 32- or 128-bit form.
fn is_rfcomm(element: &Element<'_>) -> bool {
    const BASE_TAIL: [u8; 12] = [
        0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb,
    ];
    element.kind == KIND_UUID
        && match element.value {
            [a, b] => u16::from_be_bytes([*a, *b]) == RFCOMM_UUID16,
            [0, 0, a, b] => u16::from_be_bytes([*a, *b]) == RFCOMM_UUID16,
            [0, 0, a, b, tail @ ..] => {
                u16::from_be_bytes([*a, *b]) == RFCOMM_UUID16 && *tail == BASE_TAIL
            }
            _ => false,
        }
}

/// The RFCOMM channel in a protocol descriptor list: the sequence that starts with RFCOMM's UUID, then the
/// channel. Lists nested in further sequences are searched too.
fn channel_in(list: &[Element<'_>]) -> io::Result<Option<u8>> {
    if let [protocol, channel, ..] = list {
        if is_rfcomm(protocol) && channel.kind == KIND_UINT && channel.value.len() == 1 {
            return Ok(Some(channel.value[0]));
        }
    }
    for item in list {
        if item.kind == KIND_SEQUENCE {
            if let Some(channel) = channel_in(&elements(item.value)?)? {
                return Ok(Some(channel));
            }
        }
    }
    Ok(None)
}

/// The RFCOMM channel of the first record in the attribute lists of a response that has one.
fn rfcomm_channel(lists: &[u8]) -> io::Result<Option<u8>> {
    if lists.is_empty() {
        return Ok(None);
    }
    let (outer, _) = element(lists)?;
    if outer.kind != KIND_SEQUENCE {
        return Err(invalid("attribute lists are not a sequence"));
    }
    for record in elements(outer.value)? {
        if record.kind != KIND_SEQUENCE {
            continue;
        }
        let attributes = elements(record.value)?;
        for pair in attributes.chunks(2) {
            let [id, value] = pair else { continue };
            let is_list = id.kind == KIND_UINT
                && id.value == ATTR_PROTOCOL_DESCRIPTOR_LIST.to_be_bytes()
                && value.kind == KIND_SEQUENCE;
            if is_list {
                if let Some(channel) = channel_in(&elements(value.value)?)? {
                    return Ok(Some(channel));
                }
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;

    const BTPROTO_L2CAP: libc::c_int = 0;
    const BTPROTO_RFCOMM: libc::c_int = 3;
    const SOL_BLUETOOTH: libc::c_int = 274;
    const BT_SECURITY: libc::c_int = 4;
    const BT_SECURITY_MEDIUM: u8 = 2;
    const SDP_PSM: u16 = 0x0001;
    const BDADDR_BREDR: u8 = 0x00;

    #[repr(C)]
    struct SockaddrL2 {
        family: libc::sa_family_t,
        psm: u16,
        bdaddr: [u8; 6],
        cid: u16,
        bdaddr_type: u8,
    }

    #[repr(C)]
    struct SockaddrRc {
        family: libc::sa_family_t,
        bdaddr: [u8; 6],
        channel: u8,
    }

    #[repr(C)]
    struct BtSecurity {
        level: u8,
        key_size: u8,
    }

    fn socket(kind: libc::c_int, protocol: libc::c_int) -> io::Result<OwnedFd> {
        // SAFETY: a plain socket call; the descriptor is owned by the returned value.
        unsafe {
            let raw = libc::socket(libc::AF_BLUETOOTH, kind | libc::SOCK_CLOEXEC, protocol);
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(raw))
        }
    }

    fn set_option<T>(
        fd: &OwnedFd,
        level: libc::c_int,
        name: libc::c_int,
        value: &T,
    ) -> io::Result<()> {
        // SAFETY: `value` outlives the call and is as long as stated.
        let result = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                level,
                name,
                value as *const T as *const libc::c_void,
                std::mem::size_of::<T>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Bounds sends, receives and the connect, which waits as a send does; zero for no bound.
    fn set_timeouts(fd: &OwnedFd, timeout: Duration) -> io::Result<()> {
        let tv = libc::timeval {
            tv_sec: timeout.as_secs() as _,
            tv_usec: timeout.subsec_micros() as _,
        };
        set_option(fd, libc::SOL_SOCKET, libc::SO_SNDTIMEO, &tv)?;
        set_option(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv)
    }

    fn connect<T>(fd: &OwnedFd, addr: &T) -> io::Result<()> {
        // SAFETY: `addr` outlives the call and is as long as stated.
        let result = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                addr as *const T as *const libc::sockaddr,
                std::mem::size_of::<T>() as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// An L2CAP channel to the SDP server of the device.
    pub fn connect_sdp(bdaddr: [u8; 6], timeout: Duration) -> io::Result<OwnedFd> {
        let fd = socket(libc::SOCK_SEQPACKET, BTPROTO_L2CAP)?;
        set_timeouts(&fd, timeout)?;
        let addr = SockaddrL2 {
            family: libc::AF_BLUETOOTH as libc::sa_family_t,
            psm: SDP_PSM.to_le(),
            bdaddr,
            cid: 0,
            bdaddr_type: BDADDR_BREDR,
        };
        connect(&fd, &addr)?;
        Ok(fd)
    }

    /// An encrypted RFCOMM connection to `channel` of the device, blocking and without timeouts once up.
    pub fn connect_rfcomm(bdaddr: [u8; 6], channel: u8, timeout: Duration) -> io::Result<OwnedFd> {
        let fd = socket(libc::SOCK_STREAM, BTPROTO_RFCOMM)?;
        set_option(
            &fd,
            SOL_BLUETOOTH,
            BT_SECURITY,
            &BtSecurity {
                level: BT_SECURITY_MEDIUM,
                key_size: 0,
            },
        )?;
        set_timeouts(&fd, timeout)?;
        let addr = SockaddrRc {
            family: libc::AF_BLUETOOTH as libc::sa_family_t,
            bdaddr,
            channel,
        };
        connect(&fd, &addr)?;
        set_timeouts(&fd, Duration::ZERO)?;
        Ok(fd)
    }

    pub fn send(fd: &OwnedFd, pdu: &[u8]) -> io::Result<()> {
        // SAFETY: the buffer outlives the call.
        let n = unsafe {
            libc::send(
                fd.as_raw_fd(),
                pdu.as_ptr() as *const libc::c_void,
                pdu.len(),
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn recv(fd: &OwnedFd) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; 2048];
        // SAFETY: the buffer outlives the call and is as long as stated.
        let n = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        buf.truncate(n as usize);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IAP2: &str = "00000000-deca-fade-deca-deafdecacafe";

    #[test]
    fn addresses_and_uuids() {
        assert_eq!(
            parse_address("0A:1B:2C:3D:4E:5F").unwrap(),
            [0x5f, 0x4e, 0x3d, 0x2c, 0x1b, 0x0a]
        );
        assert!(parse_address("0A:1B:2C:3D:4E").is_err());
        let uuid = parse_uuid(IAP2).unwrap();
        assert_eq!(uuid[..4], [0, 0, 0, 0]);
        assert_eq!(uuid[12..], [0xde, 0xca, 0xca, 0xfe]);
        assert!(parse_uuid("deca").is_err());
    }

    #[test]
    fn request_bytes() {
        let pdu = request(1, &parse_uuid(IAP2).unwrap(), &[]);
        assert_eq!(pdu[..5], [0x06, 0x00, 0x01, 0x00, 0x1b]);
        assert_eq!(pdu[5..8], [0x35, 0x11, 0x1c]);
        assert_eq!(pdu[24..], [0x04, 0x00, 0x35, 0x03, 0x09, 0x00, 0x04, 0x00]);
        assert_eq!(request(2, &[0; 16], &[0xaa])[31..], [0x01, 0xaa]);
    }

    /// A response holding one record whose protocol descriptor list is L2CAP, then RFCOMM on `channel`.
    fn reply(transaction: u16, channel: u8) -> Vec<u8> {
        let l2cap = [0x35, 0x03, 0x19, 0x01, 0x00];
        let rfcomm = [0x35, 0x05, 0x19, 0x00, 0x03, 0x08, channel];
        let mut list = vec![0x35, (l2cap.len() + rfcomm.len()) as u8];
        list.extend_from_slice(&l2cap);
        list.extend_from_slice(&rfcomm);
        let mut record = vec![0x09, 0x00, 0x04];
        record.extend_from_slice(&list);
        let mut lists = vec![0x35, record.len() as u8 + 2, 0x35, record.len() as u8];
        lists.extend_from_slice(&record);
        let mut params = (lists.len() as u16).to_be_bytes().to_vec();
        params.extend_from_slice(&lists);
        params.push(0);
        let mut pdu = vec![0x07];
        pdu.extend_from_slice(&transaction.to_be_bytes());
        pdu.extend_from_slice(&(params.len() as u16).to_be_bytes());
        pdu.extend_from_slice(&params);
        pdu
    }

    #[test]
    fn channel_from_a_record() {
        let pdu = reply(3, 9);
        let (lists, continuation) = response(&pdu, 3).unwrap();
        assert!(continuation.is_empty());
        assert_eq!(rfcomm_channel(lists).unwrap(), Some(9));
    }

    #[test]
    fn no_record() {
        let pdu = [0x07, 0x00, 0x01, 0x00, 0x05, 0x00, 0x02, 0x35, 0x00, 0x00];
        let (lists, _) = response(&pdu, 1).unwrap();
        assert_eq!(rfcomm_channel(lists).unwrap(), None);
    }

    #[test]
    fn continued_response() {
        let pdu = [
            0x07, 0x00, 0x01, 0x00, 0x07, 0x00, 0x02, 0x35, 0x08, 0x02, 0xaa, 0xbb,
        ];
        let (lists, continuation) = response(&pdu, 1).unwrap();
        assert_eq!(lists, [0x35, 0x08]);
        assert_eq!(continuation, [0xaa, 0xbb]);
    }

    #[test]
    fn bad_responses() {
        assert!(response(&[0x01, 0x00, 0x01, 0x00, 0x02, 0x00, 0x01], 1).is_err());
        assert!(response(&reply(2, 1), 1).is_err());
        assert!(response(&[0x07, 0x00, 0x01, 0x00, 0x09, 0x00], 1).is_err());
    }
}
