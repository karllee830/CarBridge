// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! A Wi-Fi station run by the system's wpa_supplicant, which OpenWrt starts as a ubus service.

use std::io;
use std::path::PathBuf;

use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::ubus;

/// Directory of the station configurations, which hold keys: on tmpfs, readable by root only.
const CONFIG_DIR: &str = "/var/run/carbridge";
/// Where wpa_supplicant puts the station's control socket.
const CTRL_DIR: &str = "/var/run/wpa_supplicant";

/// A station on one interface. The interface is not managed by netifd's wireless configuration.
pub struct Station {
    iface: String,
}

impl Station {
    pub fn new(iface: &str) -> Station {
        Station {
            iface: iface.to_owned(),
        }
    }

    pub fn iface(&self) -> &str {
        &self.iface
    }

    fn config_path(&self) -> PathBuf {
        PathBuf::from(CONFIG_DIR).join(format!("wpa-{}.conf", self.iface))
    }

    /// Starts joining the network that `config` describes (wpa_supplicant's configuration format), leaving any
    /// network the station had.
    pub async fn join(&self, config: &str) -> Result<(), ubus::Error> {
        self.leave().await?;
        write_private(&self.config_path(), config)
            .await
            .map_err(ubus::Error::Io)?;
        let args = json!({
            "iface": self.iface,
            "driver": "nl80211",
            "ctrl": CTRL_DIR,
            "config": self.config_path(),
        });
        ubus::call("wpa_supplicant", "config_add", &args).await?;
        Ok(())
    }

    /// wpa_supplicant's state for the interface (`SCANNING`, `ASSOCIATING`, `COMPLETED`…), or `None` when it does not
    /// run the interface.
    pub async fn state(&self) -> Result<Option<String>, ubus::Error> {
        match ubus::call(
            "wpa_supplicant",
            "iface_status",
            &json!({ "name": self.iface }),
        )
        .await
        {
            Ok(status) => Ok(status["state"].as_str().map(str::to_owned)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Whether the station is associated and authenticated.
    pub async fn connected(&self) -> Result<bool, ubus::Error> {
        Ok(self.state().await?.as_deref() == Some("COMPLETED"))
    }

    /// Turns power saving off on the interface, with `iw`.
    pub async fn power_save_off(&self) -> io::Result<()> {
        let output = tokio::process::Command::new("iw")
            .args(["dev", &self.iface, "set", "power_save", "off"])
            .output()
            .await?;
        if output.status.success() {
            Ok(())
        } else {
            let message = String::from_utf8_lossy(&output.stderr);
            Err(io::Error::other(format!(
                "iw: {}",
                message.trim().trim_start_matches("command failed: ")
            )))
        }
    }

    /// What `iw` reports of the access point the station is associated with (`iw dev IFACE station dump`): its
    /// signal, bitrates and counters, as `iw` prints them; empty while it is not associated, and with drivers that
    /// dump no stations.
    pub async fn station_dump(&self) -> io::Result<String> {
        self.iw(&["station", "dump"]).await
    }

    /// What `iw` reports of the station's link (`iw dev IFACE link`): the access point, its signal and bitrates.
    pub async fn link(&self) -> io::Result<String> {
        self.iw(&["link"]).await
    }

    async fn iw(&self, command: &[&str]) -> io::Result<String> {
        let output = tokio::process::Command::new("iw")
            .args(["dev", &self.iface])
            .args(command)
            .output()
            .await?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let message = String::from_utf8_lossy(&output.stderr);
            Err(io::Error::other(format!(
                "iw: {}",
                message.trim().trim_start_matches("command failed: ")
            )))
        }
    }

    /// Leaves the network and hands the interface back.
    pub async fn leave(&self) -> Result<(), ubus::Error> {
        ubus::call(
            "wpa_supplicant",
            "config_remove",
            &json!({ "iface": self.iface }),
        )
        .await?;
        match tokio::fs::remove_file(self.config_path()).await {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(ubus::Error::Io(err)),
            _ => Ok(()),
        }
    }
}

async fn write_private(path: &PathBuf, content: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(content.as_bytes()).await?;
    file.sync_all().await
}
