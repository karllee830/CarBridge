// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Bluetooth Classic through bluetoothd: the adapter's name and visibility, RFCOMM profiles whose connections
//! bluetoothd hands over, whether a device connects to us or we connect to it, and a pairing agent that accepts
//! pairings only while pairing is open.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::stream::{Stream, StreamExt};
use tokio::sync::mpsc;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, proxy, Connection, MatchRule, MessageStream};

const PROFILE_PATH: &str = "/org/carbridge/profile";
const AGENT_PATH: &str = "/org/carbridge/agent";

#[proxy(
    interface = "org.bluez.Adapter1",
    default_service = "org.bluez",
    gen_blocking = false
)]
trait Adapter {
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn class(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_alias(&self, alias: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_discoverable(&self, discoverable: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_discoverable_timeout(&self, seconds: u32) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_pairable(&self, pairable: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn set_pairable_timeout(&self, seconds: u32) -> zbus::Result<()>;
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.bluez.ProfileManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez",
    gen_blocking = false
)]
trait ProfileManager {
    fn register_profile(
        &self,
        profile: ObjectPath<'_>,
        uuid: &str,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_profile(&self, profile: ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez",
    gen_blocking = false
)]
trait AgentManager {
    fn register_agent(&self, agent: ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: ObjectPath<'_>) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.bluez.Device1",
    default_service = "org.bluez",
    gen_blocking = false
)]
trait Device {
    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_trusted(&self, trusted: bool) -> zbus::Result<()>;
    fn connect_profile(&self, uuid: &str) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
}

/// A device paired with the adapter.
#[derive(Clone, Debug)]
pub struct PairedDevice {
    /// The device's object path.
    pub path: String,
    pub address: String,
    /// The name it goes by: the one it was given here, else the one it gives itself.
    pub name: Option<String>,
    pub trusted: bool,
    pub connected: bool,
    /// The services bluetoothd found on it, lower case.
    pub uuids: Vec<String>,
}

impl PairedDevice {
    pub fn offers(&self, uuid: &str) -> bool {
        self.uuids.iter().any(|u| u.eq_ignore_ascii_case(uuid))
    }
}

/// A change bluetoothd reports for one of the adapter's devices, by object path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceChange {
    /// The device became paired.
    Paired(String),
    /// bluetoothd learned the device's services: their UUIDs, lower-case 128-bit text.
    Services(String, Vec<String>),
}

/// The changes to the adapter's devices, as they happen.
pub type DeviceChanges = Pin<Box<dyn Stream<Item = DeviceChange> + Send>>;

/// Which side of an RFCOMM profile we are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Devices connect to us on this RFCOMM channel, which bluetoothd publishes in the profile's SDP record.
    Server(u8),
    /// We connect to the profile of a device, with [`Classic::connect_profile`].
    Client,
}

/// An RFCOMM connection bluetoothd accepted for the profile.
#[derive(Debug)]
pub struct ProfileConnection {
    /// The device's object path.
    pub device: String,
    pub socket: OwnedFd,
}

struct Profile {
    connections: mpsc::Sender<ProfileConnection>,
}

#[interface(name = "org.bluez.Profile1")]
impl Profile {
    fn release(&self) {}

    async fn new_connection(
        &self,
        device: OwnedObjectPath,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        let connection = ProfileConnection {
            device: device.to_string(),
            socket: fd.into(),
        };
        self.connections
            .send(connection)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("not accepting".into()))
    }

    fn request_disconnection(&self, _device: OwnedObjectPath) {}
}

/// What the agent does with pairing requests.
#[derive(Debug)]
pub enum AgentEvent {
    /// A device asked to pair; `accepted` says whether pairing was open.
    Pairing { device: String, accepted: bool },
}

struct Agent {
    conn: Connection,
    open: Arc<AtomicBool>,
    events: mpsc::Sender<AgentEvent>,
}

impl Agent {
    async fn decide(&self, device: &OwnedObjectPath) -> zbus::fdo::Result<()> {
        let accepted = self.open.load(Ordering::Acquire);
        let _ = self.events.try_send(AgentEvent::Pairing {
            device: device.to_string(),
            accepted,
        });
        if !accepted {
            return Err(zbus::fdo::Error::AccessDenied("pairing is closed".into()));
        }
        if let Ok(proxy) = DeviceProxy::builder(&self.conn)
            .path(device.clone())?
            .build()
            .await
        {
            let _ = proxy.set_trusted(true).await;
        }
        Ok(())
    }

    async fn paired(&self, device: &OwnedObjectPath) -> bool {
        match DeviceProxy::builder(&self.conn).path(device.clone()) {
            Ok(builder) => match builder.build().await {
                Ok(proxy) => proxy.paired().await.unwrap_or(false),
                Err(_) => false,
            },
            Err(_) => false,
        }
    }
}

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    async fn request_pin_code(&self, device: OwnedObjectPath) -> zbus::fdo::Result<String> {
        self.decide(&device).await.map(|_| "0000".into())
    }

    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) {}

    async fn request_passkey(&self, device: OwnedObjectPath) -> zbus::fdo::Result<u32> {
        self.decide(&device).await.map(|_| 0)
    }

    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        _passkey: u32,
    ) -> zbus::fdo::Result<()> {
        self.decide(&device).await
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> zbus::fdo::Result<()> {
        self.decide(&device).await
    }

    /// Services of paired devices are always allowed; others only while pairing is open.
    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        _uuid: String,
    ) -> zbus::fdo::Result<()> {
        if self.paired(&device).await {
            return Ok(());
        }
        self.decide(&device).await
    }

    fn cancel(&self) {}
}

/// Bluetooth Classic on one adapter.
pub struct Classic {
    conn: Connection,
    adapter: AdapterProxy<'static>,
    adapter_path: String,
    open: Arc<AtomicBool>,
    profiles: Mutex<Vec<String>>,
}

impl Classic {
    pub async fn connect(adapter: &str) -> zbus::Result<Classic> {
        let conn = Connection::system().await?;
        let adapter_path = format!("/org/bluez/{adapter}");
        let adapter = AdapterProxy::builder(&conn)
            .path(adapter_path.clone())?
            .build()
            .await?;
        Ok(Classic {
            conn,
            adapter,
            adapter_path,
            open: Arc::new(AtomicBool::new(false)),
            profiles: Mutex::new(Vec::new()),
        })
    }

    pub async fn address(&self) -> zbus::Result<String> {
        self.adapter.address().await
    }

    /// The adapter's class of device.
    pub async fn class(&self) -> zbus::Result<u32> {
        self.adapter.class().await
    }

    /// The name other devices show for the adapter.
    pub async fn set_name(&self, name: &str) -> zbus::Result<()> {
        self.adapter.set_alias(name).await
    }

    /// Opens or closes pairing: the adapter becomes discoverable and pairable, and the agent accepts pairings, for
    /// `seconds` (0 keeps it open until closed).
    pub async fn set_pairing(&self, open: bool, seconds: u32) -> zbus::Result<()> {
        self.open.store(open, Ordering::Release);
        if open {
            self.adapter.set_pairable_timeout(seconds).await?;
            self.adapter.set_discoverable_timeout(seconds).await?;
        }
        self.adapter.set_pairable(open).await?;
        self.adapter.set_discoverable(open).await
    }

    /// Registers the pairing agent as the default one; returns its events.
    pub async fn register_agent(&self) -> zbus::Result<mpsc::Receiver<AgentEvent>> {
        let (events, rx) = mpsc::channel(16);
        let agent = Agent {
            conn: self.conn.clone(),
            open: self.open.clone(),
            events,
        };
        self.conn.object_server().at(AGENT_PATH, agent).await?;
        let manager = AgentManagerProxy::new(&self.conn).await?;
        let path = ObjectPath::try_from(AGENT_PATH)?;
        // No display and no keypad: pairing is Just Works, and the iPhone does not ask the user to compare a code
        // on a screen the router does not have.
        manager
            .register_agent(path.clone(), "NoInputNoOutput")
            .await?;
        manager.request_default_agent(path).await?;
        Ok(rx)
    }

    /// Registers an RFCOMM profile for `uuid`; bluetoothd hands over each of its connections.
    pub async fn register_profile(
        &self,
        uuid: &str,
        name: &str,
        role: Role,
    ) -> zbus::Result<mpsc::Receiver<ProfileConnection>> {
        let (connections, rx) = mpsc::channel(4);
        let path = format!("{PROFILE_PATH}/{}", uuid.replace('-', "_"));
        self.conn
            .object_server()
            .at(path.as_str(), Profile { connections })
            .await?;
        let mut options = HashMap::new();
        options.insert("Name", Value::from(name));
        match role {
            Role::Server(channel) => {
                options.insert("Role", Value::from("server"));
                options.insert("Channel", Value::from(u16::from(channel)));
            }
            Role::Client => {
                options.insert("Role", Value::from("client"));
            }
        }
        options.insert("RequireAuthentication", Value::from(true));
        options.insert("RequireAuthorization", Value::from(false));
        options.insert("AutoConnect", Value::from(true));
        ProfileManagerProxy::new(&self.conn)
            .await?
            .register_profile(ObjectPath::try_from(path.as_str())?, uuid, options)
            .await?;
        self.profiles.lock().unwrap().push(path);
        Ok(rx)
    }

    /// Connects to the profile `uuid` of a device; the connection arrives through the client profile registered
    /// for it.
    pub async fn connect_profile(&self, device: &str, uuid: &str) -> zbus::Result<()> {
        DeviceProxy::builder(&self.conn)
            .path(device.to_owned())?
            .build()
            .await?
            .connect_profile(uuid)
            .await
    }

    /// Forgets a device: its pairing and everything bluetoothd knows of it.
    pub async fn remove_device(&self, device: &str) -> zbus::Result<()> {
        self.adapter
            .remove_device(&ObjectPath::try_from(device)?)
            .await
    }

    /// Ends every connection to a device.
    pub async fn disconnect(&self, device: &str) -> zbus::Result<()> {
        DeviceProxy::builder(&self.conn)
            .path(device.to_owned())?
            .build()
            .await?
            .disconnect()
            .await
    }

    /// The object path of the device with the address `AA:BB:CC:DD:EE:FF`, in any case.
    pub fn device_path(&self, address: &str) -> String {
        format!(
            "{}/dev_{}",
            self.adapter_path,
            address.to_uppercase().replace(':', "_")
        )
    }

    /// The paired devices, connected now or not.
    pub async fn paired(&self) -> zbus::Result<Vec<PairedDevice>> {
        let objects = ObjectManagerProxy::builder(&self.conn)
            .destination("org.bluez")?
            .path("/")?
            .build()
            .await?
            .get_managed_objects()
            .await?;
        let prefix = format!("{}/", self.adapter_path);
        let flag = |props: &HashMap<String, OwnedValue>, name: &str| {
            props
                .get(name)
                .is_some_and(|v| matches!(**v, Value::Bool(true)))
        };
        Ok(objects
            .into_iter()
            .filter(|(path, _)| path.as_str().starts_with(&prefix))
            .filter_map(|(path, interfaces)| {
                let props = interfaces
                    .iter()
                    .find(|(name, _)| name.as_str() == "org.bluez.Device1")
                    .map(|(_, props)| props)?;
                if !flag(props, "Paired") {
                    return None;
                }
                let address = match props.get("Address").map(|v| &**v) {
                    Some(Value::Str(s)) => s.to_string(),
                    _ => return None,
                };
                let uuids = uuid_list(props.get("UUIDs"));
                let name = ["Alias", "Name"].into_iter().find_map(|key| {
                    match props.get(key).map(|v| &**v) {
                        Some(Value::Str(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
                        _ => None,
                    }
                });
                Some(PairedDevice {
                    path: path.to_string(),
                    address,
                    name,
                    trusted: flag(props, "Trusted"),
                    connected: flag(props, "Connected"),
                    uuids,
                })
            })
            .collect())
    }

    /// The devices as they become paired, and as bluetoothd learns their services. Pairings bluetoothd settles
    /// without the agent, as Just Works ones, show here at once.
    pub async fn device_changes(&self) -> zbus::Result<DeviceChanges> {
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")?
            .member("PropertiesChanged")?
            .path_namespace(self.adapter_path.clone())?
            .arg(0, "org.bluez.Device1")?
            .build();
        let stream = MessageStream::for_match_rule(rule, &self.conn, None).await?;
        Ok(Box::pin(stream.flat_map(|message| {
            let mut changes = Vec::new();
            if let Ok(message) = message {
                let path = message.header().path().map(|p| p.to_string());
                let body = message.body();
                let changed =
                    body.deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>();
                if let (Some(path), Ok((_, changed, _))) = (path, changed) {
                    if changed
                        .get("Paired")
                        .is_some_and(|v| matches!(**v, Value::Bool(true)))
                    {
                        changes.push(DeviceChange::Paired(path.clone()));
                    }
                    if changed.contains_key("UUIDs") {
                        changes.push(DeviceChange::Services(
                            path,
                            uuid_list(changed.get("UUIDs")),
                        ));
                    }
                }
            }
            futures_util::stream::iter(changes)
        })))
    }

    /// Trusts a device: bluetoothd then lets it connect to our services without asking the agent.
    pub async fn trust(&self, device: &str) -> zbus::Result<()> {
        DeviceProxy::builder(&self.conn)
            .path(device.to_owned())?
            .build()
            .await?
            .set_trusted(true)
            .await
    }

    /// The address and name of a device, by its object path.
    pub async fn device(&self, path: &str) -> zbus::Result<(String, String)> {
        let proxy = DeviceProxy::builder(&self.conn)
            .path(path.to_owned())?
            .build()
            .await?;
        Ok((
            proxy.address().await?,
            proxy.name().await.unwrap_or_default(),
        ))
    }

    /// Withdraws the profiles and the agent and closes pairing.
    pub async fn close(&self) {
        let _ = self.set_pairing(false, 0).await;
        if let Ok(manager) = ProfileManagerProxy::new(&self.conn).await {
            let profiles = std::mem::take(&mut *self.profiles.lock().unwrap());
            for profile in profiles {
                if let Ok(path) = ObjectPath::try_from(profile.as_str()) {
                    let _ = manager.unregister_profile(path).await;
                }
            }
        }
        if let Ok(manager) = AgentManagerProxy::new(&self.conn).await {
            if let Ok(path) = ObjectPath::try_from(AGENT_PATH) {
                let _ = manager.unregister_agent(path).await;
            }
        }
    }
}

/// The UUIDs in a `UUIDs` property, lower-case.
fn uuid_list(value: Option<&OwnedValue>) -> Vec<String> {
    match value.map(|v| &**v) {
        Some(Value::Array(array)) => array
            .iter()
            .filter_map(|u| match u {
                Value::Str(s) => Some(s.to_lowercase()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}
