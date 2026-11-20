// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The router's own access point, the network the iPhone joins for CarPlay: its settings in uci and its live state.

use std::process::Command;

/// What the iPhone needs to join the access point.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessPoint {
    pub ifname: String,
    pub ssid: String,
    /// The WPA passphrase; empty on an open network.
    pub passphrase: String,
    /// The uci `encryption` value, for example `psk2` or `sae-mixed`.
    pub encryption: String,
    pub channel: u8,
    pub bssid: [u8; 6],
}

impl std::fmt::Debug for AccessPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessPoint")
            .field("ifname", &self.ifname)
            .field("ssid", &self.ssid)
            .field("encryption", &self.encryption)
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// Reads the access point that runs on `ifname`.
pub fn read(ifname: &str) -> Result<AccessPoint, String> {
    let uci = run("uci", &["-q", "show", "wireless"])?;
    let (ssid, passphrase, encryption) =
        from_uci(&uci, ifname).ok_or_else(|| format!("no access point on {ifname} in uci"))?;
    let info = run("iw", &["dev", ifname, "info"])?;
    let channel = channel_from_iw(&info).ok_or_else(|| format!("{ifname} has no channel"))?;
    let address = std::fs::read_to_string(format!("/sys/class/net/{ifname}/address"))
        .map_err(|err| err.to_string())?;
    let bssid = parse_mac(address.trim()).ok_or_else(|| format!("bad address of {ifname}"))?;
    Ok(AccessPoint {
        ifname: ifname.to_owned(),
        ssid,
        passphrase,
        encryption,
        channel,
        bssid,
    })
}

/// The channel the access point on `ifname` runs on now.
pub fn channel(ifname: &str) -> Result<u8, String> {
    let info = run("iw", &["dev", ifname, "info"])?;
    channel_from_iw(&info).ok_or_else(|| format!("{ifname} has no channel"))
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("{program}: {err}"))?;
    if !output.status.success() {
        return Err(format!("{program} {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// SSID, key and encryption of the `wifi-iface` in AP mode whose `ifname` is `ifname`, from `uci show wireless`.
fn from_uci(text: &str, ifname: &str) -> Option<(String, String, String)> {
    let mut sections: Vec<(String, std::collections::HashMap<String, String>)> = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('\'').to_owned();
        let mut parts = key.splitn(3, '.');
        let (_, section, option) = (parts.next()?, parts.next()?, parts.next());
        let index = match sections.iter().position(|(name, _)| name == section) {
            Some(index) => index,
            None => {
                sections.push((section.to_owned(), Default::default()));
                sections.len() - 1
            }
        };
        if let Some(option) = option {
            sections[index].1.insert(option.to_owned(), value);
        }
    }
    sections
        .into_iter()
        .map(|(_, options)| options)
        .find_map(|options| {
            let matches = options.get("mode").map(String::as_str) == Some("ap")
                && options.get("ifname").map(String::as_str) == Some(ifname)
                && options.get("disabled").map(String::as_str) != Some("1");
            matches.then(|| {
                (
                    options.get("ssid").cloned().unwrap_or_default(),
                    options.get("key").cloned().unwrap_or_default(),
                    options
                        .get("encryption")
                        .cloned()
                        .unwrap_or_else(|| "none".into()),
                )
            })
        })
}

fn channel_from_iw(text: &str) -> Option<u8> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("channel "))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = text.split(':');
    for byte in &mut out {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UCI: &str = "wireless.radio0=wifi-device\nwireless.radio0.channel='149'\nwireless.ap0=wifi-iface\n\
        wireless.ap0.device='radio0'\nwireless.ap0.mode='ap'\nwireless.ap0.ifname='wlan0'\nwireless.ap0.ssid='My Car Wi-Fi'\n\
        wireless.ap0.encryption='psk2'\nwireless.ap0.key='secret123'\nwireless.default_radio1=wifi-iface\n\
        wireless.default_radio1.mode='ap'\nwireless.default_radio1.ssid='OpenWrt'\nwireless.default_radio1.disabled='1'\n";

    #[test]
    fn access_point_from_uci() {
        assert_eq!(
            from_uci(UCI, "wlan0"),
            Some(("My Car Wi-Fi".into(), "secret123".into(), "psk2".into()))
        );
        assert_eq!(from_uci(UCI, "wlan1"), None);
    }

    #[test]
    fn channel_and_address() {
        let iw = "Interface wlan0\n\tifindex 40\n\taddr 02:00:00:00:01:01\n\ttype AP\n\tchannel 149 (5745 MHz), width: 80 MHz\n";
        assert_eq!(channel_from_iw(iw), Some(149));
        assert_eq!(
            parse_mac("02:00:00:00:01:01"),
            Some([0x02, 0x00, 0x00, 0x00, 0x01, 0x01])
        );
        assert_eq!(parse_mac("02:00"), None);
    }
}
