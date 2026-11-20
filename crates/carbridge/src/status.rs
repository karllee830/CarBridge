// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What CarBridge is doing, written as JSON to a file whenever it changes, for the web interface to show.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::mfi::MfiStatus;
use crate::store::SavedHeadUnit;

/// The state the web interface shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub version: &'static str,
    /// What the connection to the head unit is doing: `starting`, `scanning`, `pin-needed`, `connecting`,
    /// `connected`, `casting`, `retrying` or `stopped`.
    pub carlink: &'static str,
    /// The head unit of the current or last connection.
    pub head_unit: Option<String>,
    /// Why the last connection failed.
    pub carlink_error: Option<String>,
    /// The head units paired with.
    pub head_units: Vec<SavedHeadUnit>,
    /// How the last first pairing with a head unit, by the PIN it showed, went.
    pub pin_pairing: Option<PinPairing>,
    pub carplay: CarPlayStatus,
}

/// How a first pairing with a head unit, by the PIN it showed, went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PinPairing {
    /// Counts the outcomes, for one to be told from the one before.
    pub attempt: u64,
    /// `paired`; `refused`, the PIN, which is dropped; or `failed`, the connection, which is tried again with the
    /// same PIN.
    pub result: &'static str,
    /// For `failed`, what failed: `bluetooth`, `wifi`, `authentication` or `other`.
    pub reason: Option<&'static str>,
    /// For `failed`, the error.
    pub detail: Option<String>,
    /// The head unit, as the list of head units names it.
    pub head_unit: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CarPlayStatus {
    /// The receiver runs.
    pub receiver: bool,
    /// The address of the iPhone in session, if one is.
    pub iphone: Option<String>,
    /// The iPhone's screen is being received.
    pub screen: bool,
    /// Until when, in Unix seconds, iPhones can pair; 0 when they cannot.
    pub pairing_until: u64,
    /// The iPhones paired over Bluetooth.
    pub iphones: Vec<PairedIphone>,
    /// Why CarPlay cannot run, while it cannot.
    pub unavailable: Option<Unavailable>,
    /// Why pairing could not be opened for iPhones, after the last request, when it could not.
    pub pairing_error: Option<PairingError>,
    /// The MFi identity CarPlay authenticates with.
    pub mfi: MfiStatus,
}

/// Why CarPlay cannot run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unavailable {
    /// `off`, CarPlay is turned off; `mfi`, no MFi identity it can authenticate with; `network`, no IPv4 address to
    /// receive the iPhone on; `receiver`, the receiver does not start; `bluetooth`, Bluetooth is not ready for iPhones.
    pub reason: &'static str,
    /// The error, in words.
    pub detail: Option<String>,
}

/// Why pairing could not be opened for iPhones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairingError {
    /// Counts the failures, for one to be told from the one before.
    pub attempt: u64,
    pub detail: String,
}

/// An iPhone paired over Bluetooth.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PairedIphone {
    pub address: String,
    pub name: Option<String>,
    /// It is connected over Bluetooth now.
    pub connected: bool,
}

/// The status and the file it goes to.
pub struct StatusFile {
    path: Option<PathBuf>,
    status: Mutex<Status>,
}

impl StatusFile {
    /// Writes to `path`; with `None`, keeps the status in memory only.
    pub fn new(path: Option<&Path>) -> StatusFile {
        let status = Status {
            version: env!("CARGO_PKG_VERSION"),
            carlink: "starting",
            ..Status::default()
        };
        let file = StatusFile {
            path: path.map(Path::to_owned),
            status: Mutex::new(status),
        };
        file.write(&file.status.lock().unwrap());
        file
    }

    /// Changes the status and writes it when it changed.
    pub fn update(&self, change: impl FnOnce(&mut Status)) {
        let mut status = self.status.lock().unwrap();
        let before = status.clone();
        change(&mut status);
        if *status != before {
            self.write(&status);
        }
    }

    pub fn get(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn write(&self, status: &Status) {
        let Some(path) = &self.path else { return };
        let Ok(json) = serde_json::to_vec_pretty(status) else {
            return;
        };
        let tmp = path.with_extension("new");
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&tmp, json));
        if written.and_then(|_| std::fs::rename(&tmp, path)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Unix seconds now.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_on_change() {
        let dir = std::env::temp_dir().join(format!("carbridge-status-{}", std::process::id()));
        let path = dir.join("status.json");
        let file = StatusFile::new(Some(&path));
        let first: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(first["carlink"], "starting");
        file.update(|s| {
            s.carlink = "casting";
            s.carplay.screen = true;
        });
        let second: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(second["carlink"], "casting");
        assert_eq!(second["carplay"]["screen"], true);
        assert_eq!(second["head_unit"], serde_json::Value::Null);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn memory_only() {
        let file = StatusFile::new(None);
        file.update(|s| s.carlink = "scanning");
        assert_eq!(file.get().carlink, "scanning");
    }
}
