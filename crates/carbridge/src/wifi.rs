// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What the head unit is asked for in the GATT exchange: the kind of network to open, and its channel.
//!
//! A head unit is asked for a Wi-Fi Direct group first, or for what worked with it last. One that refuses the network
//! asked for, or stays silent, or whose network cannot be joined, is asked for the other kind at the next connection;
//! the kind that worked is kept with its key for the connections after.
//!
//! The head unit's network runs on a radio of its own, but next to the router's access point, which the iPhone uses
//! for CarPlay: the head unit is asked for a 5 GHz channel in the other half of the band, outside the DFS channels.

use std::collections::HashMap;

use carbridge_carlink_ble::exchange::NetworkType;

/// How a connection that asked for a network went, as far as the network is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The network the head unit opened was joined; it is the kind given.
    Joined(NetworkType),
    /// The head unit refused the client info, or did not answer it with its server info.
    NotAnswered,
    /// The head unit opened a network that could not be joined, or gave no address on it.
    NotJoined,
}

/// The kind of network asked for next, by head unit, after one that did not work.
#[derive(Debug, Default)]
pub struct NetworkChoice {
    next: HashMap<String, NetworkType>,
}

impl NetworkChoice {
    /// The kind of network to ask the head unit `id` for, `kept` the one that worked with it last.
    pub fn ask(&self, id: &str, kept: Option<NetworkType>) -> NetworkType {
        self.next
            .get(id)
            .copied()
            .or(kept)
            .unwrap_or(NetworkType::P2pGroup)
    }

    /// Takes the outcome of a connection to the head unit `id` that asked for `asked`. Returns the kind to keep for
    /// it, when it joined one.
    pub fn outcome(
        &mut self,
        id: &str,
        asked: NetworkType,
        outcome: Outcome,
    ) -> Option<NetworkType> {
        match outcome {
            Outcome::Joined(kind) => {
                self.next.remove(id);
                Some(kind)
            }
            Outcome::NotAnswered | Outcome::NotJoined => {
                self.next.insert(id.to_owned(), other(asked));
                None
            }
        }
    }
}

/// The other kind of network.
pub fn other(kind: NetworkType) -> NetworkType {
    match kind {
        NetworkType::P2pGroup => NetworkType::SoftAp,
        NetworkType::SoftAp => NetworkType::P2pGroup,
    }
}

/// The name of a kind of network, as the store keeps it and the log tells it.
pub fn network_name(kind: NetworkType) -> &'static str {
    match kind {
        NetworkType::P2pGroup => "wifi-direct",
        NetworkType::SoftAp => "access-point",
    }
}

/// The kind of network a name gives, as [`network_name`] writes it.
pub fn parse_network(name: &str) -> Option<NetworkType> {
    match name {
        "wifi-direct" => Some(NetworkType::P2pGroup),
        "access-point" => Some(NetworkType::SoftAp),
        _ => None,
    }
}

/// The channel to ask the head unit for, away from the access point on `access_point`: in the upper half of the
/// 5 GHz band for an access point in the lower half, and the other way round; 0, none, when the access point's channel
/// is not known or it is not in the 5 GHz band.
pub fn channel_away_from(access_point: Option<u8>) -> u32 {
    match access_point {
        // U-NII-1 and U-NII-2A, below 5.35 GHz: ask for U-NII-3.
        Some(36..=64) => 149,
        // U-NII-2C and U-NII-3, above 5.47 GHz: ask for U-NII-1.
        Some(100..=177) => 36,
        _ => 0,
    }
}

/// The channel of a frequency in MHz, in the 2.4 or the 5 GHz band.
pub fn channel_of(freq: u32) -> Option<u32> {
    match freq {
        2484 => Some(14),
        2412..=2472 => Some((freq - 2407) / 5),
        5160..=5885 => Some((freq - 5000) / 5),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_direct_first_then_the_other_on_failure() {
        let mut choice = NetworkChoice::default();
        assert_eq!(choice.ask("car", None), NetworkType::P2pGroup);
        // Silent or refused: the access point next time, kept once it works.
        assert_eq!(
            choice.outcome("car", NetworkType::P2pGroup, Outcome::NotAnswered),
            None
        );
        assert_eq!(choice.ask("car", None), NetworkType::SoftAp);
        assert_eq!(choice.ask("another", None), NetworkType::P2pGroup);
        assert_eq!(
            choice.outcome(
                "car",
                NetworkType::SoftAp,
                Outcome::Joined(NetworkType::SoftAp)
            ),
            Some(NetworkType::SoftAp)
        );
        assert_eq!(
            choice.ask("car", Some(NetworkType::SoftAp)),
            NetworkType::SoftAp
        );
        // What worked is asked for first; when it fails, the other.
        assert_eq!(
            choice.outcome("car", NetworkType::SoftAp, Outcome::NotJoined),
            None
        );
        assert_eq!(
            choice.ask("car", Some(NetworkType::SoftAp)),
            NetworkType::P2pGroup
        );
        // Failing both ways goes back and forth.
        choice.outcome("car", NetworkType::P2pGroup, Outcome::NotAnswered);
        assert_eq!(
            choice.ask("car", Some(NetworkType::SoftAp)),
            NetworkType::SoftAp
        );
    }

    #[test]
    fn the_network_the_head_unit_opens_is_kept() {
        let mut choice = NetworkChoice::default();
        // Asked for Wi-Fi Direct, a head unit may open its access point all the same.
        assert_eq!(
            choice.outcome(
                "car",
                NetworkType::P2pGroup,
                Outcome::Joined(NetworkType::SoftAp)
            ),
            Some(NetworkType::SoftAp)
        );
        assert_eq!(
            choice.ask("car", Some(NetworkType::SoftAp)),
            NetworkType::SoftAp
        );
    }

    #[test]
    fn names() {
        for kind in [NetworkType::P2pGroup, NetworkType::SoftAp] {
            assert_eq!(parse_network(network_name(kind)), Some(kind));
            assert_eq!(other(other(kind)), kind);
        }
        assert_eq!(parse_network("p2p"), None);
    }

    #[test]
    fn a_channel_in_the_other_half_of_the_band() {
        assert_eq!(channel_away_from(Some(149)), 36);
        assert_eq!(channel_away_from(Some(165)), 36);
        assert_eq!(channel_away_from(Some(100)), 36);
        assert_eq!(channel_away_from(Some(36)), 149);
        assert_eq!(channel_away_from(Some(48)), 149);
        assert_eq!(channel_away_from(Some(64)), 149);
        assert_eq!(channel_away_from(Some(6)), 0);
        assert_eq!(channel_away_from(None), 0);
    }

    #[test]
    fn channels_of_frequencies() {
        assert_eq!(channel_of(5745), Some(149));
        assert_eq!(channel_of(5180), Some(36));
        assert_eq!(channel_of(5825), Some(165));
        assert_eq!(channel_of(2412), Some(1));
        assert_eq!(channel_of(2484), Some(14));
        assert_eq!(channel_of(0), None);
    }
}
