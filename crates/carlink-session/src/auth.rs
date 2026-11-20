// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Authentication on the auth channel, which the phone opens to the head unit.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use carbridge_carlink_proto::auth::{self, Handshake};
use carbridge_carlink_proto::frame::Message;
use carbridge_carlink_proto::header::{Category, Kind};
use carbridge_carlink_proto::pb::AuthResponse;
use carbridge_carlink_proto::{command, port};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::channel::Channel;
use crate::Error;

/// How long the phone waits for the auth channel to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the phone waits for the head unit's `AuthResponse`.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the phone waits, after `AuthConfirm`, for the head unit to close the auth channel.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

/// The head unit accepted the authentication; the phone still has to confirm it.
pub struct Accepted {
    pub session: auth::Session,
    channel: Channel,
}

/// The head unit's auth channel at `head_unit`.
pub fn auth_address(head_unit: Ipv4Addr) -> SocketAddr {
    SocketAddr::from((head_unit, port::AUTH))
}

/// Connects to the head unit's auth channel at `addr` ([`auth_address`]), sends the handshake's request and checks
/// the response.
pub async fn authenticate(addr: SocketAddr, handshake: Handshake) -> Result<Accepted, Error> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| Error::Timeout("auth connect"))??;
    let mut channel = Channel::new(stream, None);
    let request = Message::protobuf(
        Category::AUTH,
        command::auth::AUTH_REQUEST,
        Kind::Request,
        handshake.request(),
    );
    let sequence = channel.send(request).await?;
    let response = timeout(RESPONSE_TIMEOUT, async {
        loop {
            let message = channel.recv().await?;
            if message.category == Category::AUTH && message.command == command::auth::AUTH_RESPONSE
            {
                if message.sequence != sequence {
                    return Err(Error::Unexpected("AuthResponse to another request"));
                }
                return message.decode_body::<AuthResponse>().map_err(Error::from);
            }
        }
    })
    .await
    .map_err(|_| Error::Timeout("auth response"))??;
    let session = handshake.finish(&response)?;
    Ok(Accepted { session, channel })
}

impl Accepted {
    /// Sends `AuthConfirm` with the phone's address and waits briefly for the head unit to close the auth channel.
    /// The phone must already listen on its channels: the head unit connects to them right after.
    pub async fn confirm(mut self, phone_address: Ipv4Addr) -> Result<auth::Session, Error> {
        let confirm = auth::auth_confirm(&self.session.key, phone_address);
        let message = Message::protobuf(
            Category::AUTH,
            command::auth::AUTH_CONFIRM,
            Kind::Send,
            &confirm,
        );
        self.channel.send(message).await?;
        let _ = timeout(CLOSE_TIMEOUT, async {
            while self.channel.recv().await.is_ok() {}
        })
        .await;
        Ok(self.session)
    }
}
