// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The LE link to the head unit: a kernel L2CAP socket on the ATT channel.

use std::io;

use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::AddressType;

use crate::advert::Address;
use crate::gatt::AttChannel;

/// L2CAP channel identifier of ATT.
const ATT_CID: u16 = 4;

/// Connects to the head unit's ATT channel; `public` tells whether the advertisement came from a public address.
pub async fn connect(head_unit: Address, public: bool) -> io::Result<AttLink> {
    let addr_type = if public {
        AddressType::LePublic
    } else {
        AddressType::LeRandom
    };
    let socket = Socket::<SeqPacket>::new_seq_packet()?;
    // An unbound socket counts as BR/EDR, and some kernels then refuse to connect it to an LE address.
    socket.bind(SocketAddr {
        addr: bluer::Address::any(),
        addr_type: AddressType::LePublic,
        psm: 0,
        cid: ATT_CID,
    })?;
    let sa = SocketAddr {
        addr: bluer::Address(head_unit.0),
        addr_type,
        psm: 0,
        cid: ATT_CID,
    };
    Ok(AttLink(socket.connect(sa).await?))
}

/// An ATT channel over the LE link. Dropping it closes the socket; the kernel ends the link once no socket uses it.
pub struct AttLink(SeqPacket);

impl AttChannel for AttLink {
    async fn send(&mut self, pdu: &[u8]) -> io::Result<()> {
        self.0.send(pdu).await.map(drop)
    }

    async fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buf).await
    }
}
