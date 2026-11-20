// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The screen the head unit is cast while CarPlay shows nothing, chosen from what CarPlay is doing: how to pair an
//! iPhone while none is paired, how to pair one while pairing is open, that an iPhone is awaited or connecting, or why
//! CarPlay cannot run.

use std::fmt;

use crate::status::CarPlayStatus;

/// A screen shown while CarPlay shows nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StateScreen {
    /// Waiting for a paired iPhone.
    #[default]
    Waiting,
    /// No iPhone is paired: how to pair one.
    Unpaired,
    /// iPhones can pair now: how to.
    Pairing,
    /// An iPhone is connected and CarPlay is starting.
    Connecting,
    /// CarPlay cannot run: no MFi identity it can authenticate with.
    NoMfi,
    /// CarPlay cannot run: Bluetooth is not ready.
    NoBluetooth,
    /// CarPlay cannot run: no address to receive the iPhone on.
    NoNetwork,
    /// CarPlay cannot run, for another reason.
    Unavailable,
}

impl StateScreen {
    /// The screen for CarPlay's status at `now`, in Unix seconds.
    pub fn choose(carplay: &CarPlayStatus, now: u64) -> StateScreen {
        if let Some(unavailable) = &carplay.unavailable {
            return match unavailable.reason {
                // Turned off, CarPlay is not missed: the waiting screen shows.
                "off" => StateScreen::Waiting,
                "mfi" => StateScreen::NoMfi,
                "bluetooth" => StateScreen::NoBluetooth,
                "network" => StateScreen::NoNetwork,
                _ => StateScreen::Unavailable,
            };
        }
        if carplay.iphone.is_some() || carplay.iphones.iter().any(|phone| phone.connected) {
            return StateScreen::Connecting;
        }
        if carplay.pairing_until > now {
            return StateScreen::Pairing;
        }
        if carplay.iphones.is_empty() {
            return StateScreen::Unpaired;
        }
        StateScreen::Waiting
    }

    /// The base name of the screen's clips; `None` for the waiting screen, whose clips are named by the options.
    pub fn clip_name(self) -> Option<&'static str> {
        Some(match self {
            StateScreen::Waiting => return None,
            StateScreen::Unpaired => "unpaired",
            StateScreen::Pairing => "pairing",
            StateScreen::Connecting => "connecting",
            StateScreen::NoMfi => "unavailable-mfi",
            StateScreen::NoBluetooth => "unavailable-bluetooth",
            StateScreen::NoNetwork => "unavailable-network",
            StateScreen::Unavailable => "unavailable",
        })
    }

    /// Every screen with clips of its own.
    pub const NAMED: [StateScreen; 7] = [
        StateScreen::Unpaired,
        StateScreen::Pairing,
        StateScreen::Connecting,
        StateScreen::NoMfi,
        StateScreen::NoBluetooth,
        StateScreen::NoNetwork,
        StateScreen::Unavailable,
    ];
}

impl fmt::Display for StateScreen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StateScreen::Waiting => "the waiting screen",
            StateScreen::Unpaired => "the screen on pairing an iPhone",
            StateScreen::Pairing => "the pairing screen",
            StateScreen::Connecting => "the connecting screen",
            StateScreen::NoMfi => "the screen of CarPlay without an MFi identity",
            StateScreen::NoBluetooth => "the screen of CarPlay without Bluetooth",
            StateScreen::NoNetwork => "the screen of CarPlay without a network address",
            StateScreen::Unavailable => "the screen of CarPlay unavailable",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{PairedIphone, Unavailable};

    fn phone(connected: bool) -> PairedIphone {
        PairedIphone {
            address: "AA:BB:CC:DD:EE:FF".into(),
            name: Some("iPhone".into()),
            connected,
        }
    }

    #[test]
    fn the_screen_follows_carplay() {
        let mut carplay = CarPlayStatus::default();
        assert_eq!(StateScreen::choose(&carplay, 100), StateScreen::Unpaired);
        carplay.pairing_until = 200;
        assert_eq!(StateScreen::choose(&carplay, 100), StateScreen::Pairing);
        // Pairing closed again.
        assert_eq!(StateScreen::choose(&carplay, 200), StateScreen::Unpaired);
        carplay.iphones = vec![phone(false)];
        assert_eq!(StateScreen::choose(&carplay, 100), StateScreen::Pairing);
        assert_eq!(StateScreen::choose(&carplay, 300), StateScreen::Waiting);
        carplay.iphones = vec![phone(true)];
        assert_eq!(StateScreen::choose(&carplay, 300), StateScreen::Connecting);
        carplay.iphones = vec![phone(false)];
        carplay.iphone = Some("192.168.77.20".into());
        assert_eq!(StateScreen::choose(&carplay, 100), StateScreen::Connecting);
    }

    #[test]
    fn why_carplay_is_unavailable_comes_first() {
        let mut carplay = CarPlayStatus {
            iphones: vec![phone(true)],
            pairing_until: 200,
            ..CarPlayStatus::default()
        };
        for (reason, screen) in [
            ("mfi", StateScreen::NoMfi),
            ("bluetooth", StateScreen::NoBluetooth),
            ("network", StateScreen::NoNetwork),
            ("receiver", StateScreen::Unavailable),
            ("off", StateScreen::Waiting),
        ] {
            carplay.unavailable = Some(Unavailable {
                reason,
                detail: None,
            });
            assert_eq!(StateScreen::choose(&carplay, 100), screen, "{reason}");
        }
    }

    #[test]
    fn clip_names() {
        assert_eq!(StateScreen::Waiting.clip_name(), None);
        let names: Vec<_> = StateScreen::NAMED
            .iter()
            .map(|screen| screen.clip_name().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "unpaired",
                "pairing",
                "connecting",
                "unavailable-mfi",
                "unavailable-bluetooth",
                "unavailable-network",
                "unavailable"
            ]
        );
    }
}
