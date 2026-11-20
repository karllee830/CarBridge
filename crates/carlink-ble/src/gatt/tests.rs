// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::*;
use crate::exchange::{Band, NetworkType, UNKNOWN_MAC};

struct Pipe {
    tx: UnboundedSender<Vec<u8>>,
    rx: UnboundedReceiver<Vec<u8>>,
}

impl AttChannel for Pipe {
    async fn send(&mut self, pdu: &[u8]) -> io::Result<()> {
        self.tx
            .send(pdu.to_vec())
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }

    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.rx.recv().await {
            Some(pdu) => {
                buf[..pdu.len()].copy_from_slice(&pdu);
                Ok(pdu.len())
            }
            None => Ok(0),
        }
    }
}

fn pipes() -> (Pipe, Pipe) {
    let (a_tx, a_rx) = unbounded_channel();
    let (b_tx, b_rx) = unbounded_channel();
    (Pipe { tx: a_tx, rx: b_rx }, Pipe { tx: b_tx, rx: a_rx })
}

const CLIENT_INFO_HANDLE: u16 = 0x12;
const SERVER_INFO_HANDLE: u16 = 0x14;
const SERVER_INFO: &[u8] =
    br#"{"name":"","ssid":"DIRECT-vs-AB12CD","psk":"12345678","mac":"02:00:00:00:00:00","freq":5180,"port":0,"type":1001}"#;

#[derive(Default, Debug)]
struct Seen {
    own_request_answer: Option<Vec<u8>>,
    client_info: Vec<u8>,
    long_write: bool,
    cccd_written: bool,
    confirmed: bool,
}

/// A head unit's GATT server, written from the protocol description.
async fn head_unit(mut pipe: Pipe, mtu: u16, indicate_before_write_rsp: bool) -> Seen {
    let mut seen = Seen::default();
    let mut prepared = Vec::new();
    let chars: [(u16, [u8; 16], u8); 2] = [
        (0x11, CLIENT_INFO_UUID, 0x08),
        (0x13, SERVER_INFO_UUID, 0x10),
    ];
    // The head unit's own request, right after connecting.
    pipe.send(&[0x08, 0x01, 0x00, 0xFF, 0xFF, 0x3A, 0x2B])
        .await
        .unwrap();
    let mut agreed = att::DEFAULT_MTU;
    let mut buf = [0u8; 1024];
    loop {
        let len = pipe.recv(&mut buf).await.unwrap();
        if len == 0 {
            return seen;
        }
        let pdu = buf[..len].to_vec();
        let indication = || {
            let mut ind = vec![opcode::HANDLE_VALUE_IND];
            ind.extend_from_slice(&SERVER_INFO_HANDLE.to_le_bytes());
            ind.extend_from_slice(SERVER_INFO);
            ind.extend([0, 0]);
            ind
        };
        match pdu[0] {
            opcode::ERROR_RSP => seen.own_request_answer = Some(pdu),
            opcode::EXCHANGE_MTU_REQ => {
                let asked = u16::from_le_bytes([pdu[1], pdu[2]]);
                agreed = asked.min(mtu);
                pipe.send(&[opcode::EXCHANGE_MTU_RSP, mtu as u8, (mtu >> 8) as u8])
                    .await
                    .unwrap();
            }
            opcode::FIND_BY_TYPE_VALUE_REQ => {
                let mut uuid = pdu[7..23].to_vec();
                uuid.reverse();
                assert_eq!(uuid, SERVICE_UUID);
                pipe.send(&[opcode::FIND_BY_TYPE_VALUE_RSP, 0x10, 0x00, 0x16, 0x00])
                    .await
                    .unwrap();
            }
            opcode::READ_BY_TYPE_REQ => {
                let start = u16::from_le_bytes([pdu[1], pdu[2]]);
                let fit = usize::from(agreed - 2) / 21;
                let entries: Vec<_> = chars.iter().filter(|c| c.0 >= start).take(fit).collect();
                if entries.is_empty() {
                    pipe.send(&att::error_response(
                        pdu[0],
                        start,
                        att::error::ATTRIBUTE_NOT_FOUND,
                    ))
                    .await
                    .unwrap();
                    continue;
                }
                let mut rsp = vec![opcode::READ_BY_TYPE_RSP, 21];
                for (decl, uuid, props) in entries {
                    rsp.extend_from_slice(&decl.to_le_bytes());
                    rsp.push(*props);
                    rsp.extend_from_slice(&(decl + 1).to_le_bytes());
                    rsp.extend(uuid.iter().rev());
                }
                pipe.send(&rsp).await.unwrap();
            }
            opcode::WRITE_REQ if u16::from_le_bytes([pdu[1], pdu[2]]) == 0x15 => {
                seen.cccd_written = true
            }
            opcode::WRITE_REQ => {
                assert_eq!(u16::from_le_bytes([pdu[1], pdu[2]]), CLIENT_INFO_HANDLE);
                seen.client_info = pdu[3..].to_vec();
                if indicate_before_write_rsp {
                    pipe.send(&indication()).await.unwrap();
                    pipe.send(&[opcode::WRITE_RSP]).await.unwrap();
                } else {
                    pipe.send(&[opcode::WRITE_RSP]).await.unwrap();
                    pipe.send(&indication()).await.unwrap();
                }
            }
            opcode::PREPARE_WRITE_REQ => {
                seen.long_write = true;
                assert!(len <= usize::from(agreed));
                let offset = usize::from(u16::from_le_bytes([pdu[3], pdu[4]]));
                assert_eq!(offset, prepared.len());
                prepared.extend_from_slice(&pdu[5..]);
                let mut rsp = pdu.clone();
                rsp[0] = opcode::PREPARE_WRITE_RSP;
                pipe.send(&rsp).await.unwrap();
            }
            opcode::EXECUTE_WRITE_REQ => {
                seen.client_info = std::mem::take(&mut prepared);
                pipe.send(&[opcode::EXECUTE_WRITE_RSP]).await.unwrap();
                pipe.send(&indication()).await.unwrap();
            }
            opcode::HANDLE_VALUE_CFM => {
                seen.confirmed = true;
                return seen;
            }
            other => panic!("unexpected PDU {other:#04x}"),
        }
    }
}

fn client() -> ClientInfo {
    ClientInfo {
        id: "a1b2c3".into(),
        name: "iPhone".into(),
        model: "CarBridge".into(),
        band: Band::Ghz5,
        mac: UNKNOWN_MAC.into(),
        network: NetworkType::P2pGroup,
        channel: 0,
        manufacturer: None,
        direct: None,
    }
}

async fn run(mtu: u16, indicate_before_write_rsp: bool) -> (ServerInfo, Seen, u16) {
    let (phone, car) = pipes();
    let car = tokio::spawn(head_unit(car, mtu, indicate_before_write_rsp));
    let mut att = AttClient::new(phone);
    let info = att.exchange(&client()).await.unwrap();
    let mtu = att.mtu();
    drop(att);
    (info, car.await.unwrap(), mtu)
}

#[tokio::test]
async fn exchange_with_large_mtu() {
    let (info, seen, mtu) = run(512, false).await;
    assert_eq!(mtu, 512);
    assert_eq!(info.ssid, "DIRECT-vs-AB12CD");
    assert_eq!(info.freq, 5180);
    assert_eq!(seen.client_info, client().to_json());
    assert!(!seen.long_write);
    assert!(!seen.cccd_written);
    assert!(seen.confirmed);
    assert_eq!(
        seen.own_request_answer.as_deref(),
        Some(&[0x01, 0x08, 0x01, 0x00, 0x0A][..])
    );
}

#[tokio::test]
async fn long_write_with_default_mtu() {
    let (info, seen, mtu) = run(23, false).await;
    assert_eq!(mtu, 23);
    assert!(seen.long_write);
    assert_eq!(seen.client_info, client().to_json());
    assert_eq!(info.network(), Some(NetworkType::P2pGroup));
}

#[tokio::test]
async fn indication_before_write_response() {
    let (info, seen, _) = run(512, true).await;
    assert_eq!(info.psk, "12345678");
    assert!(seen.confirmed);
}

#[tokio::test]
async fn link_closed_before_server_info() {
    let (phone, car) = pipes();
    drop(car);
    let err = AttClient::new(phone).exchange(&client()).await.unwrap_err();
    assert!(matches!(err, Error::Io(_) | Error::Closed), "{err}");
}

#[tokio::test]
async fn no_carlink_service() {
    let (phone, mut car) = pipes();
    let car = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        car.recv(&mut buf).await.unwrap();
        car.send(&[opcode::EXCHANGE_MTU_RSP, 0, 2]).await.unwrap();
        car.recv(&mut buf).await.unwrap();
        car.send(&att::error_response(
            opcode::FIND_BY_TYPE_VALUE_REQ,
            1,
            att::error::ATTRIBUTE_NOT_FOUND,
        ))
        .await
        .unwrap();
        car
    });
    let err = AttClient::new(phone).exchange(&client()).await.unwrap_err();
    assert!(matches!(err, Error::ServiceNotFound), "{err}");
    drop(car.await);
}
