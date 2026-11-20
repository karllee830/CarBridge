// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The GATT exchange with the head unit, as a minimal ATT client on the LE link's ATT channel.
//!
//! The head unit's GATT server never answers a descriptor write, so the phone does not subscribe to the server info
//! characteristic: it receives the indication without writing the CCCD. Its Bluetooth stack is also a GATT client on
//! the same link, and the phone answers each of its requests.

use std::fmt;
use std::io;

use crate::att::{self, opcode};
use crate::exchange::{ClientInfo, ServerInfo};

/// The Carlink GATT service.
pub const SERVICE_UUID: [u8; 16] = uuid(0x00);
/// Characteristic the phone writes its client info to.
pub const CLIENT_INFO_UUID: [u8; 16] = uuid(0x01);
/// Characteristic the head unit indicates its server info on.
pub const SERVER_INFO_UUID: [u8; 16] = uuid(0x02);

/// `2abcc850-9935-4f8a-ba84-1234567891xx`.
const fn uuid(last: u8) -> [u8; 16] {
    [
        0x2a, 0xbc, 0xc8, 0x50, 0x99, 0x35, 0x4f, 0x8a, 0xba, 0x84, 0x12, 0x34, 0x56, 0x78, 0x91,
        last,
    ]
}

/// Largest client info the head unit collects.
pub const MAX_CLIENT_INFO: usize = 1024;

/// A packet-preserving connection to the head unit's ATT channel.
#[allow(async_fn_in_trait)]
pub trait AttChannel {
    /// Sends one PDU.
    async fn send(&mut self, pdu: &[u8]) -> io::Result<()>;
    /// Receives one PDU into `buf` and returns its length; 0 when the link is gone.
    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

/// Why the exchange failed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The link closed before the head unit sent its server info.
    Closed,
    /// The head unit answered a request with an error.
    Att {
        request: u8,
        code: u8,
    },
    /// The head unit answered with something that is not a valid answer to the request.
    UnexpectedResponse {
        request: u8,
        response: u8,
    },
    /// The head unit does not have the Carlink service or one of its characteristics.
    ServiceNotFound,
    /// The client info is longer than the head unit collects.
    ClientInfoTooLong(usize),
    /// The server info is not valid JSON of the expected shape.
    ServerInfo(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "ATT channel: {err}"),
            Error::Closed => f.write_str("LE link closed before the server info"),
            Error::Att { request, code } => {
                write!(f, "ATT request {request:#04x} failed with {code:#04x}")
            }
            Error::UnexpectedResponse { request, response } => {
                write!(
                    f,
                    "ATT request {request:#04x} answered with {response:#04x}"
                )
            }
            Error::ServiceNotFound => f.write_str("head unit has no Carlink GATT service"),
            Error::ClientInfoTooLong(len) => write!(f, "client info of {len} bytes is too long"),
            Error::ServerInfo(err) => write!(f, "server info: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

/// Handles of the Carlink characteristics on the head unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handles {
    pub client_info: u16,
    pub server_info: u16,
}

/// The phone's side of one ATT link.
pub struct AttClient<C> {
    channel: C,
    mtu: u16,
    /// Value of an indication on the server info characteristic that arrived while a request was outstanding.
    server_info: Option<Vec<u8>>,
    server_info_handle: Option<u16>,
    buf: Vec<u8>,
}

impl<C: AttChannel> AttClient<C> {
    pub fn new(channel: C) -> AttClient<C> {
        AttClient {
            channel,
            mtu: att::DEFAULT_MTU,
            server_info: None,
            server_info_handle: None,
            buf: vec![0; 1024],
        }
    }

    /// The ATT MTU in use.
    pub fn mtu(&self) -> u16 {
        self.mtu
    }

    /// Runs the whole exchange: MTU, discovery, client info, server info.
    pub async fn exchange(&mut self, client: &ClientInfo) -> Result<ServerInfo, Error> {
        let server_info = self.send_client_info(client).await?;
        let indicated = self.wait_for_indication(server_info).await?;
        ServerInfo::parse(&indicated).map_err(Error::ServerInfo)
    }

    /// Runs the exchange up to the client info the head unit has taken: MTU, discovery, client info. Returns the
    /// handle of the server info, which [`AttClient::wait_for_indication`] then waits for; one indicated meanwhile is
    /// kept for it.
    pub async fn send_client_info(&mut self, client: &ClientInfo) -> Result<u16, Error> {
        let value = client.to_json();
        if value.len() > MAX_CLIENT_INFO {
            return Err(Error::ClientInfoTooLong(value.len()));
        }
        self.exchange_mtu().await?;
        let handles = self.discover().await?;
        self.server_info_handle = Some(handles.server_info);
        self.write(handles.client_info, &value).await?;
        Ok(handles.server_info)
    }

    /// Asks for an ATT MTU of [`att::MTU`].
    pub async fn exchange_mtu(&mut self) -> Result<u16, Error> {
        let response = self
            .request(
                &att::exchange_mtu_request(att::MTU),
                opcode::EXCHANGE_MTU_RSP,
            )
            .await?;
        let server = response
            .get(1..3)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or(Error::UnexpectedResponse {
                request: opcode::EXCHANGE_MTU_REQ,
                response: response[0],
            })?;
        self.mtu = server.clamp(att::DEFAULT_MTU, att::MTU);
        Ok(self.mtu)
    }

    /// Finds the Carlink service and its two characteristics.
    pub async fn discover(&mut self) -> Result<Handles, Error> {
        let response = self
            .request_or_not_found(
                &att::find_service_request(1, &SERVICE_UUID),
                opcode::FIND_BY_TYPE_VALUE_RSP,
            )
            .await?
            .ok_or(Error::ServiceNotFound)?;
        let (start, end) = att::parse_handle_ranges(&response)
            .and_then(|ranges| ranges.first().copied())
            .ok_or(Error::UnexpectedResponse {
                request: opcode::FIND_BY_TYPE_VALUE_REQ,
                response: response[0],
            })?;
        let (mut client_info, mut server_info) = (None, None);
        let mut next = start;
        while next <= end && next != 0 {
            let request = att::read_by_type_request(next, end, att::CHARACTERISTIC);
            let Some(response) = self
                .request_or_not_found(&request, opcode::READ_BY_TYPE_RSP)
                .await?
            else {
                break;
            };
            let found = att::parse_characteristics(&response)
                .filter(|found| !found.is_empty())
                .ok_or(Error::UnexpectedResponse {
                    request: opcode::READ_BY_TYPE_REQ,
                    response: response[0],
                })?;
            for characteristic in &found {
                if characteristic.uuid == CLIENT_INFO_UUID {
                    client_info = Some(characteristic.value_handle);
                } else if characteristic.uuid == SERVER_INFO_UUID {
                    server_info = Some(characteristic.value_handle);
                }
            }
            let last = found
                .iter()
                .map(|c| c.value_handle.max(c.declaration))
                .max()
                .unwrap_or(end);
            next = last.wrapping_add(1);
        }
        match (client_info, server_info) {
            (Some(client_info), Some(server_info)) => Ok(Handles {
                client_info,
                server_info,
            }),
            _ => Err(Error::ServiceNotFound),
        }
    }

    /// Writes `value`, as a long write when it does not fit in one Write Request.
    pub async fn write(&mut self, handle: u16, value: &[u8]) -> Result<(), Error> {
        let mtu = usize::from(self.mtu);
        if value.len() <= mtu - 3 {
            self.request(&att::write_request(handle, value), opcode::WRITE_RSP)
                .await?;
            return Ok(());
        }
        for (i, part) in value.chunks(mtu - 5).enumerate() {
            let offset = (i * (mtu - 5)) as u16;
            let request = att::prepare_write_request(handle, offset, part);
            let response = self.request(&request, opcode::PREPARE_WRITE_RSP).await?;
            if response[1..] != request[1..] {
                return Err(Error::UnexpectedResponse {
                    request: opcode::PREPARE_WRITE_REQ,
                    response: response[0],
                });
            }
        }
        self.request(&att::execute_write_request(), opcode::EXECUTE_WRITE_RSP)
            .await?;
        Ok(())
    }

    /// Waits for an indication on `handle`, confirming it, and returns its value. Answers the head unit's requests
    /// meanwhile.
    pub async fn wait_for_indication(&mut self, handle: u16) -> Result<Vec<u8>, Error> {
        self.server_info_handle = Some(handle);
        loop {
            if let Some(value) = self.server_info.take() {
                return Ok(value);
            }
            // A response with no request outstanding is dropped.
            let pdu = self.receive().await?;
            self.handle_unsolicited(&pdu).await?;
        }
    }

    /// Sends a request and waits for its response, which is `expected` or an Error Response.
    async fn request(&mut self, request: &[u8], expected: u8) -> Result<Vec<u8>, Error> {
        match self.request_or_not_found(request, expected).await? {
            Some(response) => Ok(response),
            None => Err(Error::Att {
                request: request[0],
                code: att::error::ATTRIBUTE_NOT_FOUND,
            }),
        }
    }

    /// Like `request`, but an Attribute Not Found error is `None`.
    async fn request_or_not_found(
        &mut self,
        request: &[u8],
        expected: u8,
    ) -> Result<Option<Vec<u8>>, Error> {
        self.channel.send(request).await?;
        loop {
            let pdu = self.receive().await?;
            if self.handle_unsolicited(&pdu).await? {
                continue;
            }
            if let Some((failed, code)) = att::parse_error(&pdu) {
                if failed != request[0] {
                    return Err(Error::UnexpectedResponse {
                        request: request[0],
                        response: pdu[0],
                    });
                }
                if code == att::error::ATTRIBUTE_NOT_FOUND {
                    return Ok(None);
                }
                return Err(Error::Att {
                    request: failed,
                    code,
                });
            }
            if pdu[0] != expected {
                return Err(Error::UnexpectedResponse {
                    request: request[0],
                    response: pdu[0],
                });
            }
            return Ok(Some(pdu));
        }
    }

    /// Handles a PDU that is not a response: answers the head unit's requests, confirms indications and keeps the
    /// server info. Returns false for a response.
    async fn handle_unsolicited(&mut self, pdu: &[u8]) -> Result<bool, Error> {
        let op = pdu[0];
        if att::is_request(op) {
            self.channel.send(&att::answer_peer_request(pdu)).await?;
            return Ok(true);
        }
        if att::is_response(op) {
            return Ok(false);
        }
        if op == opcode::HANDLE_VALUE_IND || op == opcode::HANDLE_VALUE_NTF {
            if op == opcode::HANDLE_VALUE_IND {
                self.channel.send(&att::confirmation()).await?;
            }
            let handle = pdu.get(1..3).map(|b| u16::from_le_bytes([b[0], b[1]]));
            if handle.is_some() && handle == self.server_info_handle {
                self.server_info = Some(pdu[3..].to_vec());
            }
        }
        // Commands and anything else need no answer.
        Ok(true)
    }

    async fn receive(&mut self) -> Result<Vec<u8>, Error> {
        let len = self.channel.recv(&mut self.buf).await?;
        if len == 0 {
            return Err(Error::Closed);
        }
        Ok(self.buf[..len].to_vec())
    }
}

#[cfg(test)]
mod tests;
