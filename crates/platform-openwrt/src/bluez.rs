// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! LE scanning through bluetoothd's D-Bus API.
//!
//! The scan only listens: it never powers the controller on or off, and it leaves the adapter's other users alone.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::select;
use zbus::fdo::{ObjectManagerProxy, PropertiesProxy};
use zbus::message::Type as MessageType;
use zbus::names::InterfaceName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{proxy, Connection, MatchRule, Message, MessageStream};

const SERVICE: &str = "org.bluez";
const DEVICE: &str = "org.bluez.Device1";

#[proxy(
    interface = "org.bluez.Adapter1",
    default_service = "org.bluez",
    gen_blocking = false
)]
trait Adapter {
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
}

/// What bluetoothd currently knows of a device seen by the scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeReport {
    /// `AA:BB:CC:DD:EE:FF`.
    pub address: String,
    /// The advertisement came from a public address rather than a random one.
    pub public: bool,
    pub rssi: Option<i16>,
    /// Service UUIDs it advertises, lower-case 128-bit text. For a paired device, the UUIDs of its services instead.
    pub uuids: Vec<String>,
    /// The device is paired with the adapter.
    pub paired: bool,
    /// Service data of the advertisement and the scan response, by lower-case 128-bit UUID text.
    pub service_data: HashMap<String, Vec<u8>>,
}

/// A connection to bluetoothd for one adapter.
pub struct Bluez {
    conn: Connection,
    adapter: AdapterProxy<'static>,
    adapter_path: OwnedObjectPath,
}

impl Bluez {
    /// Connects to bluetoothd on the system bus, for the adapter named `hci0` and so on.
    pub async fn connect(adapter: &str) -> zbus::Result<Bluez> {
        let conn = Connection::system().await?;
        let adapter_path = OwnedObjectPath::try_from(format!("/org/bluez/{adapter}"))?;
        let proxy = AdapterProxy::builder(&conn)
            .path(adapter_path.clone())?
            .build()
            .await?;
        Ok(Bluez {
            conn,
            adapter: proxy,
            adapter_path,
        })
    }

    /// The adapter's Bluetooth address, `AA:BB:CC:DD:EE:FF`.
    pub async fn address(&self) -> zbus::Result<String> {
        self.adapter.address().await
    }

    /// Whether the adapter is powered; scanning needs it.
    pub async fn powered(&self) -> zbus::Result<bool> {
        self.adapter.powered().await
    }

    /// Starts an LE scan for devices advertising any of `uuids` (128-bit text), reporting every advertisement.
    pub async fn scan_le(&self, uuids: &[String]) -> zbus::Result<LeScan<'_>> {
        let rule =
            |interface: &'static str, member: &'static str| -> zbus::Result<MatchRule<'static>> {
                Ok(MatchRule::builder()
                    .msg_type(MessageType::Signal)
                    .sender(SERVICE)?
                    .interface(interface)?
                    .member(member)?
                    .path_namespace(self.adapter_path.to_string())?
                    .build())
            };
        let changed = MessageStream::for_match_rule(
            rule("org.freedesktop.DBus.Properties", "PropertiesChanged")?,
            &self.conn,
            Some(256),
        )
        .await?;
        let added = MessageStream::for_match_rule(
            MatchRule::builder()
                .msg_type(MessageType::Signal)
                .sender(SERVICE)?
                .interface("org.freedesktop.DBus.ObjectManager")?
                .member("InterfacesAdded")?
                .build(),
            &self.conn,
            Some(256),
        )
        .await?;

        let mut filter = HashMap::new();
        filter.insert("Transport", Value::from("le"));
        filter.insert("DuplicateData", Value::from(true));
        filter.insert("UUIDs", Value::from(uuids.to_vec()));
        self.adapter.set_discovery_filter(filter).await?;
        if let Err(err) = self.adapter.start_discovery().await {
            // bluetoothd keeps one discovery per D-Bus client: one that was left running belongs to this
            // connection, and stopping it lets a new one start with the filter just set.
            if !in_progress(&err) {
                return Err(err);
            }
            self.adapter.stop_discovery().await?;
            self.adapter.start_discovery().await?;
        }

        // Devices already found by a discovery in progress carry an RSSI.
        let objects = ObjectManagerProxy::builder(&self.conn)
            .destination(SERVICE)?
            .path("/")?
            .build()
            .await?;
        let mut pending = Vec::new();
        for (path, interfaces) in objects.get_managed_objects().await? {
            if !path.as_str().starts_with(self.adapter_path.as_str()) {
                continue;
            }
            if let Some(props) = interfaces
                .iter()
                .find(|(name, _)| name.as_str() == DEVICE)
                .map(|(_, p)| p)
            {
                if props.contains_key("RSSI") {
                    pending.extend(parse_device(props));
                }
            }
        }
        Ok(LeScan {
            bluez: self,
            changed,
            added,
            pending,
        })
    }

    async fn device(&self, path: &str) -> zbus::Result<Option<LeReport>> {
        let props = PropertiesProxy::builder(&self.conn)
            .destination(SERVICE)?
            .path(path)?
            .build()
            .await?;
        let all = props
            .get_all(InterfaceName::from_static_str_unchecked(DEVICE))
            .await?;
        Ok(parse_device(&all))
    }
}

/// A scan in progress. Call [`LeScan::stop`] to end it; bluetoothd also ends it when the D-Bus connection closes.
pub struct LeScan<'a> {
    bluez: &'a Bluez,
    changed: MessageStream,
    added: MessageStream,
    pending: Vec<LeReport>,
}

impl LeScan<'_> {
    /// The next report: a device appeared, or its advertisement changed. `None` when bluetoothd went away.
    pub async fn next(&mut self) -> Option<zbus::Result<LeReport>> {
        loop {
            if let Some(report) = self.pending.pop() {
                return Some(Ok(report));
            }
            let message = select! {
                m = self.changed.next() => m?,
                m = self.added.next() => m?,
            };
            match self.handle(message).await {
                Ok(Some(report)) => return Some(Ok(report)),
                Ok(None) => {}
                Err(err) => return Some(Err(err)),
            }
        }
    }

    async fn handle(&self, message: zbus::Result<Message>) -> zbus::Result<Option<LeReport>> {
        let message = message?;
        let header = message.header();
        let member = header.member().map(|m| m.as_str()).unwrap_or_default();
        let body = message.body();
        if member == "InterfacesAdded" {
            let (path, interfaces): (
                OwnedObjectPath,
                HashMap<String, HashMap<String, OwnedValue>>,
            ) = body.deserialize()?;
            if !path.as_str().starts_with(self.bluez.adapter_path.as_str()) {
                return Ok(None);
            }
            return Ok(interfaces.get(DEVICE).and_then(parse_device));
        }
        let (interface, _, _): (String, HashMap<String, OwnedValue>, Vec<String>) =
            body.deserialize()?;
        if interface != DEVICE {
            return Ok(None);
        }
        let Some(path) = header.path() else {
            return Ok(None);
        };
        match self.bluez.device(path.as_str()).await {
            // The device was removed after the signal was sent.
            Err(err) if gone(&err) => Ok(None),
            result => result,
        }
    }

    /// Stops the scan.
    pub async fn stop(self) -> zbus::Result<()> {
        self.bluez.adapter.stop_discovery().await
    }
}

/// bluetoothd refused to start something already started for this client.
fn in_progress(err: &zbus::Error) -> bool {
    matches!(err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.Error.InProgress")
}

/// The object asked about no longer exists on the bus.
fn gone(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::FDO(err) => matches!(
            **err,
            zbus::fdo::Error::UnknownObject(_)
                | zbus::fdo::Error::UnknownInterface(_)
                | zbus::fdo::Error::UnknownMethod(_)
        ),
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.DBus.Error.UnknownMethod"
        ),
        _ => false,
    }
}

/// Reads the properties of `org.bluez.Device1`.
pub fn parse_device(props: &HashMap<String, OwnedValue>) -> Option<LeReport> {
    let address = string(props.get("Address")?)?;
    let public = props
        .get("AddressType")
        .and_then(string)
        .is_none_or(|t| t == "public");
    let rssi = props.get("RSSI").and_then(|v| match inner(v) {
        Value::I16(rssi) => Some(*rssi),
        _ => None,
    });
    let paired = matches!(
        props.get("Paired").map(|v| inner(v)),
        Some(Value::Bool(true))
    );
    let uuids = props
        .get("UUIDs")
        .map(|v| match inner(v) {
            Value::Array(array) => array
                .iter()
                .filter_map(|u| string_value(u).map(|s| s.to_lowercase()))
                .collect(),
            _ => Vec::new(),
        })
        .unwrap_or_default();
    let mut service_data = HashMap::new();
    if let Some(Value::Dict(dict)) = props.get("ServiceData").map(|v| inner(v)) {
        for (key, value) in dict.iter() {
            if let (Some(uuid), Some(bytes)) = (string_value(key), bytes(value)) {
                service_data.insert(uuid.to_lowercase(), bytes);
            }
        }
    }
    Some(LeReport {
        address,
        public,
        rssi,
        uuids,
        paired,
        service_data,
    })
}

/// The value inside any number of variant wrappers.
fn inner<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(boxed) => inner(boxed),
        other => other,
    }
}

fn string(value: &OwnedValue) -> Option<String> {
    string_value(value)
}

fn string_value(value: &Value<'_>) -> Option<String> {
    match inner(value) {
        Value::Str(s) => Some(s.as_str().to_owned()),
        _ => None,
    }
}

fn bytes(value: &Value<'_>) -> Option<Vec<u8>> {
    match inner(value) {
        Value::Array(array) => array
            .iter()
            .map(|b| match b {
                Value::U8(b) => Some(*b),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Dict, Signature};

    fn owned(value: Value<'_>) -> OwnedValue {
        value.try_to_owned().unwrap()
    }

    fn device_props() -> HashMap<String, OwnedValue> {
        let mut data = Dict::new(&Signature::Str, &Signature::Variant);
        data.add(
            "00000001-0000-1000-8000-00805F9B34FB",
            Value::new(vec![1u8, 7, 0x5A]),
        )
        .unwrap();
        data.add(
            "00000002-0000-1000-8000-00805f9b34fb",
            Value::new(vec![0u8; 16]),
        )
        .unwrap();
        let mut props = HashMap::new();
        props.insert("Address".into(), owned(Value::from("11:22:33:44:55:66")));
        props.insert("AddressType".into(), owned(Value::from("public")));
        props.insert("RSSI".into(), owned(Value::from(-52i16)));
        props.insert(
            "UUIDs".into(),
            owned(Value::from(vec!["0000FCFB-0000-1000-8000-00805f9b34fb"])),
        );
        props.insert("ServiceData".into(), owned(Value::Dict(data)));
        props
    }

    #[test]
    fn device_properties() {
        let report = parse_device(&device_props()).unwrap();
        assert_eq!(report.address, "11:22:33:44:55:66");
        assert!(report.public);
        assert_eq!(report.rssi, Some(-52));
        assert_eq!(report.uuids, ["0000fcfb-0000-1000-8000-00805f9b34fb"]);
        assert_eq!(
            report.service_data["00000001-0000-1000-8000-00805f9b34fb"],
            [1, 7, 0x5A]
        );
        assert_eq!(
            report.service_data["00000002-0000-1000-8000-00805f9b34fb"],
            [0; 16]
        );
    }

    #[test]
    fn random_address_without_rssi() {
        let mut props = device_props();
        props.insert("AddressType".into(), owned(Value::from("random")));
        props.remove("RSSI");
        let report = parse_device(&props).unwrap();
        assert!(!report.public);
        assert_eq!(report.rssi, None);
    }

    #[test]
    fn needs_an_address() {
        let mut props = device_props();
        props.remove("Address");
        assert_eq!(parse_device(&props), None);
    }

    #[test]
    fn empty_service_data() {
        let mut props = device_props();
        props.insert(
            "ServiceData".into(),
            owned(Value::Array(Array::new(&Signature::U8))),
        );
        assert!(parse_device(&props).unwrap().service_data.is_empty());
    }

    #[test]
    fn a_removed_device_is_gone() {
        let removed = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject("removed".into())));
        assert!(gone(&removed));
        let other = zbus::Error::FDO(Box::new(zbus::fdo::Error::Failed("other".into())));
        assert!(!gone(&other));
        assert!(!in_progress(&other));
    }
}
