// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! One connection to a head unit: discovery, BLE handoff, Wi-Fi, authentication, and the session with its cast,
//! until it ends.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use carbridge_carlink_ble::advert::Address;
use carbridge_carlink_ble::discovery::{uuid16_text, HeadUnit};
use carbridge_carlink_ble::exchange::{Band, ClientInfo, NetworkType, ServerInfo, UNKNOWN_MAC};
use carbridge_carlink_ble::gatt::{self, AttChannel, AttClient};
use carbridge_carlink_ble::{advert, hash, link as ble_link};
use carbridge_carlink_proto::auth::{AuthError, AuthKey, Handshake, PhoneInfo, PublicKey};
use carbridge_carlink_proto::certificate::HeadUnitIdentity;
use carbridge_carlink_session::display::{Display, Layout, Shape, WindowMode};
use carbridge_carlink_session::{auth, session, Error as SessionError};
use carbridge_carlink_wfd::source::{self, Cast, Choices, SinkEvent, Stalls, Streamer};
use carbridge_carlink_wfd::uibc::{Input, UibcReader};
use carbridge_carlink_wfd::Codec;
use carbridge_carlink_wifi::Network;
use carbridge_carplay::now_playing::NowPlaying;
use carbridge_carplay::picture::Picture;
use carbridge_carplay::receiver::AudioEvent;
use carbridge_platform::bluez::Bluez;
use carbridge_platform::dhcp::{DhcpClient, Lease};
use carbridge_platform::wifi::Station;
use carbridge_platform::{access_point, mgmt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::time::{sleep, timeout, Instant, MissedTickBehavior};

use crate::calibration::{Calibration, Measurement, Screen};
use crate::carplay::{cast_codec_of, CarPlay};
use crate::diagnostics::{
    describe_drops, describe_stalls, describe_tcp, describe_wifi, interface_counter, tcp_health,
    MicStats, WifiStation,
};
use crate::mfi::{self, MfiInstall, MfiStatus};
use crate::music::{self, Music};
use crate::options::{layout_name, Hevc, Options};
use crate::picture::{self, HeadUnitPicture, Mode, Source};
use crate::screens::StateScreen;
use crate::settings::{Effective, HeadUnitSettings, Request};
use crate::sound::{Format, Out, Sound};
use crate::status::{unix_now, PinPairing, StatusFile};
use crate::store::{self, Store};
use crate::video::{cast_codec, offer_hevc, Clip};
use crate::views::Views;
use crate::wifi::{self as network, network_name, NetworkChoice};
use carbridge_carlink_proto::frame::Message;
use carbridge_carlink_proto::header::{BodyFormat, Category, Kind};
use carbridge_carlink_proto::pb::{
    AppInvokeCategory, AudioType, DayNightMode, GetUCarConfigResponse,
};

/// Work modes the phone offers: normal cast, and the two window casts head units that take only those want.
const WORK_MODES: i32 = 1 | 256 | 512;
const MODEL: &str = "CarBridge";
/// How long the GATT exchange may take; the head unit drops the LE link after 30 seconds.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long DHCP may take after the station joined.
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the head unit has to open RTSP after the phone confirmed the authentication.
const RTSP_TIMEOUT: Duration = Duration::from_secs(10);
const FPS: u32 = 30;
/// Requests to the session waiting to go out, the phone's PCM among them.
const REQUEST_QUEUE: usize = 128;
/// Music frames waiting for the cast.
const MUSIC_QUEUE: usize = 64;
/// How long an announcement of the iPhone's sound may wait for room in the session's queue.
const ANNOUNCE_WAIT: Duration = Duration::from_millis(500);
/// How long the cast holds back what the head unit would show squeezed after its view changed shape, at most, while
/// the iPhone connects again to draw for it.
const RESHAPE_LIMIT: Duration = Duration::from_secs(10);
/// Views measured from touches waiting for the connection.
const MEASURED_QUEUE: usize = 8;
/// How often, and how many times, the iPhone is asked for a key frame after the head unit set its view up anew: it
/// takes about a second to do so.
const VIEW_REFRESH: Duration = Duration::from_secs(1);
const VIEW_REFRESHES: u32 = 3;
/// How often the detailed log reports the microphone and the sound dropped, and, every so many of these, the cast's
/// stalls, its TCP connections and the Wi-Fi link.
const DIAGNOSTICS_EVERY: Duration = Duration::from_secs(1);
const LINK_DIAGNOSTICS_EVERY: u64 = 5;
/// Where sound is dropped before it reaches the session: the session's queue of requests, and the cast's of music.
const SESSION_QUEUE: &str = "session queue";
const CAST_QUEUE: &str = "cast queue";
/// How long the next connection waits at most for the kernel to drop the last LE link, how often it looks, and how
/// long it waits when it cannot tell.
const LINK_DROP_WAIT: Duration = Duration::from_secs(5);
const LINK_DROP_POLL: Duration = Duration::from_millis(100);
const LINK_DROP_FALLBACK: Duration = Duration::from_secs(2);
/// How long the head unit may take to indicate its server info after the phone wrote its client info, before it is
/// taken for silent: one that cannot open the network asked for may say nothing until it drops the link.
const SERVER_INFO_TIMEOUT: Duration = Duration::from_secs(15);
/// The IPv4 type of service of what goes to the head unit: DSCP CS5, 802.1d priority 5, which Wi-Fi drivers send in
/// WMM's video access category, ahead of other traffic.
const TOS: u32 = 0xa0;

/// Why a connection ended or did not start.
#[derive(Debug)]
pub enum Error {
    /// A first pairing needs the PIN the head unit shows, and none was given.
    NeedPin,
    /// The head unit refused the PIN of a first pairing, most often because it has shown a new one since.
    PinRefused,
    /// The process is stopping.
    Shutdown,
    Step(&'static str, String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NeedPin => f.write_str("first pairing needs the head unit's PIN"),
            Error::PinRefused => f.write_str(
                "the head unit refused the PIN: it shows a new one every two minutes, send the one it shows now",
            ),
            Error::Shutdown => f.write_str("stopping"),
            Error::Step(step, err) => write!(f, "{step}: {err}"),
        }
    }
}

fn step<E: fmt::Display>(name: &'static str) -> impl FnOnce(E) -> Error {
    move |err| Error::Step(name, err.to_string())
}

/// What stays the same from one connection to the next.
pub struct Context {
    pub options: Options,
    pub store: Store,
    pub bluez: Bluez,
    pub phone_id: String,
    pub phone_key: AuthKey,
    /// SHA-256 of the adapter's address, as the head unit stores it.
    pub bluetooth_hash: String,
    pub shutdown: watch::Receiver<bool>,
    pub carplay: Arc<CarPlay>,
    pub status: Arc<StatusFile>,
    /// The ID the key of the head unit connected now is kept under, while one is.
    connected: Arc<Mutex<Option<String>>>,
    /// The head units whose settings the web interface changed last, among them the one connected then.
    settings_changed: watch::Receiver<Vec<String>>,
    /// The kind of network each head unit is asked for next, after one that did not work.
    networks: Mutex<NetworkChoice>,
    /// The head unit the last LE link went to.
    last_link: Mutex<Option<Address>>,
    /// The head units that refused the key paired with last, with that key: it is not tried on them again.
    refused_fallback: Mutex<Vec<(String, PublicKey)>>,
}

/// The key a connection to a head unit authenticates with, and why it is that one.
#[derive(Clone, Copy, Debug)]
struct CarKey {
    key: PublicKey,
    /// The key paired with last, tried on a head unit known by no key: it may be another head unit's.
    fallback: bool,
}

impl Context {
    pub async fn new(options: Options, shutdown: watch::Receiver<bool>) -> Result<Context, Error> {
        let store = Store::open(&options.state_dir).map_err(step("state"))?;
        let phone_id = store.phone_id().map_err(step("state"))?;
        let phone_key = store.phone_key().map_err(step("state"))?;
        let bluez = Bluez::connect(&options.adapter)
            .await
            .map_err(step("bluetoothd"))?;
        let address = bluez.address().await.map_err(step("bluetoothd"))?;
        let status = Arc::new(StatusFile::new(Some(&options.status_file)));
        publish_head_units(&store, &status);
        // Before any head unit connects, CarPlay follows the one connected last: the picture it asked for, its
        // settings and the shape of its window.
        let last = match store.last_connected() {
            Ok(last) => last,
            Err(err) => {
                log!("reading the head unit connected last: {err}");
                None
            }
        };
        let last_settings = match last.as_deref() {
            Some(id) => match store.car_settings(id) {
                Ok(settings) => settings,
                Err(err) => {
                    log!("reading the last head unit's settings: {err}");
                    HeadUnitSettings::default()
                }
            },
            None => HeadUnitSettings::default(),
        };
        if let Some(id) = &last {
            log!("head unit {id}, connected last, has settings {last_settings}");
        }
        let own = Effective::new(&options, &last_settings);
        let kept_picture = match store.last_connected_picture() {
            Ok(Some((id, picture))) => {
                log!("CarPlay picture of head unit {id}, connected last");
                Some(picture)
            }
            Ok(None) => None,
            Err(err) => {
                log!("reading the last head unit's picture: {err}");
                None
            }
        };
        let kept_views = match store.last_connected_views() {
            Ok(views) => views,
            Err(err) => {
                log!("reading the last head unit's views: {err}");
                Views::default()
            }
        };
        // Starting in the window, the picture has the shape of the window mode the head unit connected last cast in.
        let kept_window = match store.last_connected_window() {
            Ok(mode) => mode,
            Err(err) => {
                log!("reading the last head unit's window mode: {err}");
                None
            }
        };
        let window = kept_window
            .filter(|_| own.start == Layout::Window)
            .map(WindowMode::shape);
        let carplay = Arc::new(CarPlay::new(
            &options,
            status.clone(),
            last.as_deref(),
            &own,
            kept_picture.as_ref(),
            &kept_views,
            window,
        ));
        // Before any head unit connects, the tile is named after the one connected last.
        match store.last_connected_tile() {
            Ok(Some((id, identity))) => {
                log!("car tile of head unit {id}, connected last");
                carplay.name_car_tile(&identity);
            }
            Ok(None) => {}
            Err(err) => log!("reading the last head unit's tile: {err}"),
        }
        tokio::spawn(forget_on_request(
            store.clone(),
            options.clone(),
            status.clone(),
            carplay.clone(),
        ));
        let connected = Arc::new(Mutex::new(None));
        let (settings_tx, settings_changed) = watch::channel(Vec::new());
        tokio::spawn(settings_on_request(
            store.clone(),
            options.clone(),
            status.clone(),
            carplay.clone(),
            connected.clone(),
            settings_tx,
        ));
        tokio::spawn(mfi_on_request(
            options.clone(),
            status.clone(),
            carplay.clone(),
        ));
        tokio::spawn(
            carplay
                .clone()
                .keep_up(options.clone(), address.clone(), shutdown.clone()),
        );
        let address = Address::parse(&address)
            .ok_or_else(|| Error::Step("bluetoothd", format!("address {address}")))?;
        Ok(Context {
            bluetooth_hash: hash::address_hash(address),
            carplay,
            status,
            options,
            store,
            bluez,
            phone_id,
            phone_key,
            shutdown,
            connected,
            settings_changed,
            networks: Mutex::default(),
            last_link: Mutex::new(None),
            refused_fallback: Mutex::default(),
        })
    }

    fn stopping(&self) -> bool {
        *self.shutdown.borrow()
    }

    async fn wait_shutdown(&self) {
        let mut shutdown = self.shutdown.clone();
        let _ = shutdown.wait_for(|stop| *stop).await;
    }

    fn phone(&self, user_confirmed: bool) -> PhoneInfo {
        PhoneInfo {
            id: self.phone_id.clone(),
            model: MODEL.into(),
            work_modes: Some(WORK_MODES),
            product_name: Some(self.options.name.clone()),
            user_confirmed,
        }
    }

    /// The client info that asks the head unit for a network of `network`'s kind on `channel`.
    fn client_info(&self, network: NetworkType, channel: u32) -> ClientInfo {
        ClientInfo {
            id: self.phone_id.clone(),
            name: self.options.name.clone(),
            model: MODEL.into(),
            band: Band::Ghz5,
            mac: UNKNOWN_MAC.into(),
            network,
            channel,
            manufacturer: None,
            direct: None,
        }
    }

    /// Waits, for a few seconds at most, until the kernel has dropped the LE link of the last connection, so that the
    /// next one does not find it still up.
    pub async fn link_dropped(&self) {
        let Some(address) = *self.last_link.lock().unwrap() else {
            return;
        };
        let since = Instant::now();
        let deadline = since + LINK_DROP_WAIT;
        loop {
            match mgmt::connections(&self.options.adapter) {
                Ok(links) if !le_link_to(&links, address) => {
                    detail!(
                        "the LE link to {address} is down after {} ms",
                        since.elapsed().as_millis()
                    );
                    break;
                }
                Ok(_) if Instant::now() >= deadline => {
                    log!(
                        "the LE link to {address} is still up after {} s; connecting all the same",
                        LINK_DROP_WAIT.as_secs()
                    );
                    break;
                }
                Ok(_) => {}
                Err(err) => {
                    detail!("listing the Bluetooth connections: {err}");
                    sleep(LINK_DROP_FALLBACK).await;
                    break;
                }
            }
            if self.stopping() {
                break;
            }
            sleep(LINK_DROP_POLL).await;
        }
        *self.last_link.lock().unwrap() = None;
    }

    /// Keeps the name a head unit with a key of its own advertises, for the list of head units.
    fn keep_name(&self, id: &str, name: Option<&str>) {
        if let Some(name) = name {
            if let Err(err) = self.store.set_car_name(id, name) {
                log!("keeping the head unit's name: {err}");
            }
        }
    }

    /// Puts the head units paired with in the status.
    fn publish_head_units(&self) {
        publish_head_units(&self.store, &self.status);
    }

    fn read_pin(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.options.pin_file).ok()?;
        let pin = text.trim();
        (pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit())).then(|| pin.to_owned())
    }

    /// Puts how a first pairing with the head unit `id` by its PIN went in the status, as a new outcome: the result,
    /// and for a failed connection what failed and how ([`PinPairing`]).
    fn tell_pin_pairing(
        &self,
        id: &str,
        (result, reason, detail): (&'static str, Option<&'static str>, Option<String>),
    ) {
        log!("pairing with head unit {id} by its PIN: {result}");
        self.status.update(|s| {
            let attempt = s.pin_pairing.as_ref().map_or(0, |p| p.attempt) + 1;
            s.pin_pairing = Some(PinPairing {
                attempt,
                result,
                reason,
                detail,
                head_unit: store::file_name(id),
            });
        });
    }

    /// Whether the head unit `id` refused `key`, the key paired with last, before.
    fn refused_fallback(&self, id: &str, key: &PublicKey) -> bool {
        let id = store::file_name(id);
        self.refused_fallback
            .lock()
            .unwrap()
            .iter()
            .any(|(refused, refused_key)| *refused == id && refused_key == key)
    }

    fn clear_pin(&self) {
        let _ = std::fs::remove_file(&self.options.pin_file);
    }
}

/// Runs one connection to a head unit, from discovery to the end of its session.
pub async fn connect(ctx: &Context) -> Result<session::End, Error> {
    ctx.status.update(|s| s.carlink = "scanning");
    let unit = tokio::select! {
        found = discover(ctx) => found?,
        _ = ctx.wait_shutdown() => return Err(Error::Shutdown),
    };
    let id = head_unit_id(&unit);
    ctx.status.update(|s| s.head_unit = Some(id.clone()));
    // The key the head unit is known by: its own, or the one its alias names.
    let own_key = ctx
        .store
        .known_key(&id)
        .map_err(step("state"))?
        .map(|(key, _)| key);
    if own_key.is_some() {
        ctx.keep_name(&id, unit.name.as_deref());
    }
    // A head unit known by no key pairs with the PIN the web interface gave, if there is one; without, the key paired
    // with last is tried, for head units that give no car ID, unless the head unit refused that key before.
    let pin = if own_key.is_none() {
        ctx.read_pin()
    } else {
        None
    };
    let car_key = match (own_key, &pin) {
        (Some(key), _) => Some(CarKey {
            key,
            fallback: false,
        }),
        (None, Some(_)) => None,
        (None, None) => ctx
            .store
            .last_car_key()
            .map_err(step("state"))?
            .filter(|key| !ctx.refused_fallback(&id, key))
            .map(|key| CarKey {
                key,
                fallback: true,
            }),
    };
    if car_key.is_none() && pin.is_none() {
        return Err(Error::NeedPin);
    }
    log!(
        "head unit {id} ({}), RSSI {:?}, {}",
        unit.address,
        unit.rssi,
        if pin.is_some() {
            "first pairing"
        } else {
            "reconnection"
        }
    );

    ctx.status.update(|s| s.carlink = "connecting");
    let first_pairing = pin.is_some();
    // What the head unit is asked for: the kind of network that worked with it last, or Wi-Fi Direct, unless the last
    // connection asked for that in vain; on a channel away from the iPhone's access point.
    let network_id = match ctx.store.key_owner(&id) {
        Ok(Some(owner)) => owner,
        _ => store::file_name(&id),
    };
    let kept = ctx.store.car_network(&network_id).unwrap_or_else(|err| {
        log!("reading the head unit's network: {err}");
        None
    });
    let asked = ctx.networks.lock().unwrap().ask(&network_id, kept);
    let access_point = access_point::channel(&ctx.options.ap_iface);
    let channel = network::channel_away_from(access_point.as_ref().ok().copied());
    match &access_point {
        Ok(ap) => log!(
            "asking the head unit for {} on channel {channel}, away from the access point on channel {ap}",
            network_name(asked)
        ),
        Err(err) => log!(
            "asking the head unit for {} on any channel: the access point's is not known ({err})",
            network_name(asked)
        ),
    }
    let mut joined = None;
    let result = async {
        let info = exchange(ctx, &unit, asked, channel)
            .await
            .map_err(|(err, unanswered)| {
                if unanswered {
                    joined = Some(network::Outcome::NotAnswered);
                }
                err
            })?;
        let network = Network::from_server_info(&info).map_err(|err| {
            joined = Some(network::Outcome::NotAnswered);
            step("server info")(err)
        })?;
        log!(
            "the head unit opened {} {} on channel {} ({} MHz)",
            network_name(network.kind),
            network.ssid,
            network::channel_of(network.freq)
                .map_or_else(|| "unknown".to_owned(), |c| c.to_string()),
            network.freq
        );
        let station = Station::new(&ctx.options.iface);
        station
            .join(&network.wpa_supplicant_config())
            .await
            .map_err(step("wpa_supplicant"))?;
        let result = on_network(
            ctx,
            &station,
            &network,
            &id,
            unit.name.as_deref(),
            car_key,
            pin,
        )
        .await;
        joined = match &result {
            Err(Error::Step("join" | "DHCP", _)) => Some(network::Outcome::NotJoined),
            Err(Error::Shutdown) => None,
            _ => Some(network::Outcome::Joined(network.kind)),
        };
        if let Err(err) = station.leave().await {
            log!("leaving the network: {err}");
        }
        result
    }
    .await;
    if let Some(outcome) = joined {
        note_network(ctx, &id, &network_id, asked, outcome);
    }
    // A pairing that got as far as the head unit's key was told of then; one that did not is told of now.
    if first_pairing && !ctx.store.paired(&id).unwrap_or(false) {
        if let Err(err) = &result {
            ctx.tell_pin_pairing(&id, pin_failure(err));
        }
    }
    result
}

/// Takes how the network asked of the head unit `id` went, kept under `network_id` until it has a key: the kind
/// that worked is kept with its key, the other kind is asked for next after one that did not.
fn note_network(
    ctx: &Context,
    id: &str,
    network_id: &str,
    asked: NetworkType,
    outcome: network::Outcome,
) {
    let keep = ctx
        .networks
        .lock()
        .unwrap()
        .outcome(network_id, asked, outcome);
    match keep {
        Some(kind) => {
            if let Ok(Some(owner)) = ctx.store.key_owner(id) {
                if let Err(err) = ctx.store.set_car_network(&owner, kind) {
                    log!("keeping the head unit's network: {err}");
                }
            }
        }
        None => log!(
            "the head unit's {} did not work: it is asked for {} at the next connection",
            network_name(asked),
            network_name(network::other(asked))
        ),
    }
}

/// How a first pairing that failed with `err` went, for the status: the result, and for a failed connection what
/// failed and how.
fn pin_failure(err: &Error) -> (&'static str, Option<&'static str>, Option<String>) {
    match err {
        Error::PinRefused => ("refused", None, None),
        Error::Step(step, detail) => {
            let reason = match *step {
                "LE connect" | "GATT exchange" | "scan" => "bluetooth",
                "server info" | "wpa_supplicant" | "join" | "DHCP" => "wifi",
                "authentication" | "confirm" => "authentication",
                _ => "other",
            };
            ("failed", Some(reason), Some(format!("{step}: {detail}")))
        }
        err => ("failed", Some("other"), Some(err.to_string())),
    }
}

async fn discover(ctx: &Context) -> Result<HeadUnit, Error> {
    let tag = hash::phone_tag(&ctx.bluetooth_hash);
    let mut scan = ctx
        .bluez
        .scan_le(&[uuid16_text(advert::SERVICE_UUID16)])
        .await
        .map_err(step("scan"))?;
    let found = loop {
        let Some(report) = scan.next().await else {
            break Err(Error::Step("scan", "bluetoothd went away".into()));
        };
        let report = report.map_err(step("scan"))?;
        let Some(unit) = HeadUnit::from_report(
            &report.address,
            report.public,
            report.rssi,
            &report.uuids,
            report.paired,
            &report.service_data,
        ) else {
            continue;
        };
        if !unit.in_range() {
            continue;
        }
        if unit.advert.expected_phone.is_some() && tag.is_some_and(|tag| !unit.waits_for(tag)) {
            continue;
        }
        break Ok(unit);
    };
    let _ = scan.stop().await;
    found
}

/// Puts the head units paired with in the status.
fn publish_head_units(store: &Store, status: &StatusFile) {
    match store.head_units() {
        Ok(units) => status.update(|s| s.head_units = units),
        Err(err) => log!("listing the head units: {err}"),
    }
}

/// The lines of a request file of the web interface, once it appears; it is removed as it is read.
async fn requests(file: &std::path::Path, poll: &mut tokio::time::Interval) -> String {
    loop {
        poll.tick().await;
        if let Ok(text) = std::fs::read_to_string(file) {
            let _ = std::fs::remove_file(file);
            return text;
        }
    }
}

/// Forgets the head units the web interface names in the options' forget file, one ID a line, as the file appears.
/// Forgetting the head unit connected last takes its name off CarPlay's car tile, and its picture, settings and views
/// off CarPlay's, for the iPhones that connect from now on.
async fn forget_on_request(
    store: Store,
    options: Options,
    status: Arc<StatusFile>,
    carplay: Arc<CarPlay>,
) {
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    loop {
        let text = requests(&options.forget_file, &mut poll).await;
        for id in text.lines().map(str::trim).filter(|id| !id.is_empty()) {
            let last = store.connected_last(id).unwrap_or(false);
            match store.forget_head_unit(id) {
                Ok(true) => {
                    log!("head unit {id} forgotten; it pairs again with its PIN");
                    if last {
                        carplay.reset_car_tile();
                        let defaults = Effective::new(&options, &HeadUnitSettings::default());
                        carplay.follow_head_unit(None, &defaults, &Views::default());
                        carplay.follow_picture(None);
                    }
                }
                Ok(false) => log!("no head unit {id} to forget"),
                Err(err) => log!("forgetting head unit {id}: {err}"),
            }
        }
        publish_head_units(&store, &status);
    }
}

/// Keeps the MFi identity's state in the status, looked at every few seconds, and installs the identities the web
/// interface uploads with a request in the options' MFi request file ([`mfi::take_request`]). An identity installed
/// while CarPlay cannot run has it start at once; one that replaces the identity CarPlay runs with is used from
/// CarBridge's next start.
async fn mfi_on_request(options: Options, status: Arc<StatusFile>, carplay: Arc<CarPlay>) {
    let upload = mfi::Upload::next_to(&options.mfi_request_file);
    // A request left from before CarBridge started is not the owner's now.
    upload.clear();
    let mut install: Option<MfiInstall> = None;
    let mut attempts = 0;
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    let mut ticks: u64 = 0;
    loop {
        poll.tick().await;
        let outcome = mfi::take_request(&upload, &options.mfi_dir);
        if let Some(outcome) = &outcome {
            attempts += 1;
            log!("{}", outcome.describe());
            let running = carplay.running();
            install = Some(outcome.status(attempts, running));
            if let mfi::Outcome::Installed { .. } = outcome {
                if running {
                    log!("CarPlay goes on with the identity it started with until CarBridge starts again");
                } else {
                    carplay.retry_now();
                }
            }
        }
        if outcome.is_some() || ticks % 5 == 0 {
            let inspection = carbridge_carplay::mfi::inspect(&options.mfi_dir);
            let now = MfiStatus::new(&options.mfi_dir, &inspection, install.clone());
            status.update(|s| s.carplay.mfi = now);
        }
        ticks += 1;
    }
}

/// Applies the head units' own settings the web interface asks for in the options' settings file, one request a line
/// ([`Request`]), as the file appears: keeps them and lists them, and clears the views measured of a head unit when a
/// request asks, for its windows to be calibrated again. The connection to a head unit they concern, if it is
/// connected now, follows them at once (`changed`). For the head unit CarPlay follows while none is connected, the
/// one connected last, CarPlay's picture, its shape and its offer of H.265 follow them; an iPhone given another
/// connects again. Other head units take them at their next connection.
async fn settings_on_request(
    store: Store,
    options: Options,
    status: Arc<StatusFile>,
    carplay: Arc<CarPlay>,
    connected: Arc<Mutex<Option<String>>>,
    changed: watch::Sender<Vec<String>>,
) {
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    loop {
        let text = requests(&options.settings_file, &mut poll).await;
        let mut now = Vec::new();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let request = match Request::parse(line) {
                Ok(request) => request,
                Err(err) => {
                    log!("head unit settings {line:?} refused: {err}");
                    continue;
                }
            };
            let id = store::file_name(&request.id);
            match store.paired(&id) {
                Ok(true) => {}
                Ok(false) => {
                    log!("no head unit {id} to change the settings of");
                    continue;
                }
                Err(err) => {
                    log!("finding head unit {id}: {err}");
                    continue;
                }
            }
            let before = match store.car_settings(&id) {
                Ok(settings) => settings,
                Err(err) => {
                    log!("reading head unit {id}'s settings: {err}");
                    HeadUnitSettings::default()
                }
            };
            let after = request.applied_to(before);
            if let Err(err) = store.set_car_settings(&id, &after) {
                log!("keeping head unit {id}'s settings: {err}");
                continue;
            }
            log!("head unit {id} settings: {after}");
            let cleared = request.clears_views()
                && match store.car_views(&id) {
                    Ok(views) if views.is_empty() => false,
                    Ok(_) => match store.set_car_views(&id, &Views::default()) {
                        Ok(()) => {
                            log!("head unit {id}'s views cleared: its windows are calibrated again when it next shows CarPlay there");
                            true
                        }
                        Err(err) => {
                            log!("clearing head unit {id}'s views: {err}");
                            false
                        }
                    },
                    Err(err) => {
                        log!("reading head unit {id}'s views: {err}");
                        false
                    }
                };
            let (old, new) = (
                Effective::new(&options, &before),
                Effective::new(&options, &after),
            );
            if old == new && !cleared {
                continue;
            }
            if connected.lock().unwrap().as_deref() == Some(id.as_str()) {
                now.push(id);
            } else if carplay.follows(&id) {
                if cleared {
                    carplay.follow_views(&Views::default(), None, false);
                }
                if old.picture != new.picture || old.start != new.start {
                    let window = match store.car_window(&id) {
                        Ok(mode) => mode,
                        Err(err) => {
                            log!("reading head unit {id}'s window mode: {err}");
                            None
                        }
                    };
                    let window = window
                        .filter(|_| new.start == Layout::Window)
                        .map(WindowMode::shape);
                    carplay.follow_settings(&new, Some(window));
                }
                if old.hevc != new.hevc {
                    carplay.renew_hevc(offer_hevc(new.hevc, None));
                }
            } else {
                log!("head unit {id} takes its settings at its next connection");
            }
        }
        publish_head_units(&store, &status);
        if !now.is_empty() {
            changed.send_replace(now);
        }
    }
}

/// Marks the head unit connected now, until it is dropped.
struct Connected(Arc<Mutex<Option<String>>>);

impl Connected {
    fn new(connected: &Arc<Mutex<Option<String>>>, id: Option<&str>) -> Connected {
        *connected.lock().unwrap() = id.map(str::to_owned);
        Connected(connected.clone())
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

/// The settings of the head unit `owner`, none of its own when they cannot be read.
fn own_settings(ctx: &Context, owner: Option<&str>) -> HeadUnitSettings {
    let Some(owner) = owner else {
        return HeadUnitSettings::default();
    };
    match ctx.store.car_settings(owner) {
        Ok(settings) => settings,
        Err(err) => {
            log!("reading the head unit's settings: {err}");
            HeadUnitSettings::default()
        }
    }
}

/// The views measured of the head unit `owner`, none when they cannot be read.
fn own_views(ctx: &Context, owner: Option<&str>) -> Views {
    let Some(owner) = owner else {
        return Views::default();
    };
    match ctx.store.car_views(owner) {
        Ok(views) => views,
        Err(err) => {
            log!("reading the head unit's views: {err}");
            Views::default()
        }
    }
}

/// The name under which a head unit's key is kept: its car ID, or its address when it does not disclose one.
fn head_unit_id(unit: &HeadUnit) -> String {
    if unit.advert.car_id.0 == [2, 0, 0, 0, 0, 0] || unit.advert.car_id.0 == [0; 6] {
        unit.address.to_string()
    } else {
        unit.advert.car_id.to_string()
    }
}

/// Asks the head unit for a network of the kind `asked` on `channel` over the GATT exchange, and returns the one it
/// opened. Fails with whether the head unit left the request unanswered: it refused the client info, said nothing for
/// [`SERVER_INFO_TIMEOUT`] after it, dropped the link, or answered with what is no server info.
async fn exchange(
    ctx: &Context,
    unit: &HeadUnit,
    asked: NetworkType,
    channel: u32,
) -> Result<ServerInfo, (Error, bool)> {
    let failed =
        |unanswered: bool| move |err: String| (Error::Step("GATT exchange", err), unanswered);
    let link = ble_link::connect(unit.address, unit.public)
        .await
        .map_err(|err| (Error::Step("LE connect", err.to_string()), false))?;
    *ctx.last_link.lock().unwrap() = Some(unit.address);
    let mut att = AttClient::new(LoggedAtt(link));
    let deadline = Instant::now() + EXCHANGE_TIMEOUT;
    let server_info = match tokio::time::timeout_at(
        deadline,
        att.send_client_info(&ctx.client_info(asked, channel)),
    )
    .await
    {
        Err(_) => return Err(failed(false)("timed out".into())),
        Ok(Ok(handle)) => handle,
        Ok(Err(err @ gatt::Error::Att { .. })) => {
            return Err(failed(true)(format!(
                "the head unit refused the client info: {err}"
            )))
        }
        Ok(Err(err)) => return Err(failed(false)(err.to_string())),
    };
    let answer_by = deadline.min(Instant::now() + SERVER_INFO_TIMEOUT);
    let indicated =
        match tokio::time::timeout_at(answer_by, att.wait_for_indication(server_info)).await {
            Err(_) => {
                return Err(failed(true)(format!(
                    "the head unit did not answer the client info within {} s",
                    SERVER_INFO_TIMEOUT.as_secs()
                )))
            }
            Ok(Err(gatt::Error::Closed)) => {
                return Err(failed(true)(
                    "the head unit dropped the link without answering the client info".into(),
                ))
            }
            Ok(Err(err)) => return Err(failed(false)(err.to_string())),
            Ok(Ok(value)) => value,
        };
    ServerInfo::parse(&indicated).map_err(|err| failed(true)(format!("server info: {err}")))
}

/// The ATT channel to the head unit, its PDUs noted in the detailed log by opcode, handle and length; their values
/// carry the head unit's network key, which is left out.
struct LoggedAtt<C>(C);

impl<C: AttChannel> AttChannel for LoggedAtt<C> {
    async fn send(&mut self, pdu: &[u8]) -> std::io::Result<()> {
        detail!("ATT sent {}", describe_pdu(pdu));
        self.0.send(pdu).await
    }

    async fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.0.recv(buf).await?;
        if len == 0 {
            detail!("ATT link closed");
        } else {
            detail!("ATT received {}", describe_pdu(&buf[..len]));
        }
        Ok(len)
    }
}

/// An ATT PDU for the log: its opcode, the handle or the request it concerns, and its length.
fn describe_pdu(pdu: &[u8]) -> String {
    let Some(&opcode) = pdu.first() else {
        return "an empty PDU".into();
    };
    let head = pdu
        .get(1..3)
        .map(|b| format!(" {:#06x}", u16::from_le_bytes([b[0], b[1]])))
        .unwrap_or_default();
    format!("opcode {opcode:#04x}{head}, {} bytes", pdu.len())
}

/// Whether `links` hold an LE link to `address`.
fn le_link_to(links: &[mgmt::Connection], address: Address) -> bool {
    links
        .iter()
        .any(|link| link.le && link.address == address.0)
}

async fn on_network(
    ctx: &Context,
    station: &Station,
    network: &Network,
    id: &str,
    name: Option<&str>,
    car_key: Option<CarKey>,
    pin: Option<String>,
) -> Result<session::End, Error> {
    let deadline = Instant::now() + network.join_timeout();
    while !station.connected().await.map_err(step("wpa_supplicant"))? {
        if Instant::now() > deadline {
            return Err(Error::Step("join", "timed out".into()));
        }
        if ctx.stopping() {
            return Err(Error::Shutdown);
        }
        sleep(Duration::from_millis(200)).await;
    }
    // A dozing station has the head unit hold back what it sends, touches among it, until the station wakes.
    if let Err(err) = station.power_save_off().await {
        log!("power saving stays on {}: {err}", station.iface());
    }
    let dhcp = DhcpClient::start(&ctx.options.iface)
        .await
        .map_err(step("DHCP"))?;
    let result = match dhcp.wait_for_lease(DHCP_TIMEOUT).await {
        Ok(Some(lease)) => {
            log!("address {}/{}", lease.address, lease.prefix);
            authenticated(ctx, network, lease, id, name, car_key, pin).await
        }
        Ok(None) => Err(Error::Step("DHCP", "no lease".into())),
        Err(err) => Err(Error::Step("DHCP", err.to_string())),
    };
    if let Err(err) = dhcp.stop().await {
        log!("stopping DHCP: {err}");
    }
    result
}

async fn authenticate(
    ctx: &Context,
    head_unit: Ipv4Addr,
    id: &str,
    name: Option<&str>,
    car_key: Option<CarKey>,
    pin: Option<&str>,
) -> Result<auth::Accepted, Error> {
    let mut user_confirmed = false;
    loop {
        let handshake = match (car_key, pin) {
            (Some(CarKey { key, .. }), _) => {
                Handshake::reconnection(&ctx.phone_key, key, &ctx.phone(user_confirmed))
            }
            (None, Some(pin)) => {
                Handshake::first_pairing(&ctx.phone_key, pin, &ctx.phone(user_confirmed))
            }
            (None, None) => return Err(Error::NeedPin),
        };
        match auth::authenticate(auth::auth_address(head_unit), handshake).await {
            Ok(accepted) => {
                if car_key.is_none() {
                    ctx.store
                        .set_car_key(id, &accepted.session.car_auth_key)
                        .map_err(step("state"))?;
                    ctx.clear_pin();
                    ctx.keep_name(id, name);
                    ctx.publish_head_units();
                    ctx.tell_pin_pairing(id, ("paired", None, None));
                }
                // A head unit known by no key that took the key paired with last goes by that key from now on,
                // whatever head unit pairs next.
                if car_key.is_some_and(|k| k.fallback) {
                    match ctx.store.key_owner(id) {
                        Ok(Some(owner)) if owner != store::file_name(id) => {
                            match ctx.store.set_alias(id, &owner) {
                                Ok(()) => log!("head unit {id} goes by the key of {owner}"),
                                Err(err) => log!("keeping head unit {id}'s alias: {err}"),
                            }
                        }
                        Ok(_) => {}
                        Err(err) => log!("finding the owner of head unit {id}'s key: {err}"),
                    }
                }
                return Ok(accepted);
            }
            // Another phone was the last one: this device stands in for the user and takes over.
            Err(SessionError::Auth(AuthError::OtherPhoneLast)) if !user_confirmed => {
                user_confirmed = true
            }
            Err(SessionError::Auth(
                err @ (AuthError::UnknownPhone
                | AuthError::Rejected
                | AuthError::WrongPin
                | AuthError::BadResponse(_)),
            )) => {
                match car_key {
                    // The key paired with last is another head unit's: it is kept, and not tried here again.
                    Some(CarKey {
                        key,
                        fallback: true,
                    }) => {
                        log!("head unit {id} does not take the key paired with last; it pairs with its PIN");
                        ctx.refused_fallback
                            .lock()
                            .unwrap()
                            .push((store::file_name(id), key));
                    }
                    // The head unit no longer knows the phone: its own key goes, or its alias, leaving the key it
                    // named to the head unit that owns it.
                    Some(CarKey { key, .. }) => {
                        if ctx.store.car_key(id).ok().flatten() == Some(key) {
                            let _ = ctx.store.forget_car_key(id, &key);
                        } else {
                            let _ = ctx.store.forget_alias(id);
                        }
                        ctx.publish_head_units();
                    }
                    None => {}
                }
                if pin.is_some() {
                    ctx.clear_pin();
                    if car_key.is_none() && matches!(err, AuthError::Rejected | AuthError::WrongPin)
                    {
                        return Err(Error::PinRefused);
                    }
                }
                return Err(Error::Step("authentication", err.to_string()));
            }
            Err(err) => return Err(Error::Step("authentication", err.to_string())),
        }
    }
}

async fn authenticated(
    ctx: &Context,
    network: &Network,
    lease: Lease,
    id: &str,
    name: Option<&str>,
    car_key: Option<CarKey>,
    pin: Option<String>,
) -> Result<session::End, Error> {
    let accepted = authenticate(ctx, network.head_unit, id, name, car_key, pin.as_deref()).await?;
    let owner = key_owner(ctx, id);
    // What the connection goes by: the head unit's own settings, else the options'.
    let mut settings_changed = ctx.settings_changed.clone();
    settings_changed.borrow_and_update();
    let _connected = Connected::new(&ctx.connected, owner.as_deref());
    let settings = own_settings(ctx, owner.as_deref());
    let mut own = Effective::new(&ctx.options, &settings);
    log!("head unit settings: {settings}");
    let views = own_views(ctx, owner.as_deref());
    log!("head unit views: {views}");
    ctx.carplay.follow_head_unit(owner.as_deref(), &own, &views);
    ctx.status.update(|s| {
        s.carlink = "connected";
        s.carlink_error = None;
    });
    log!(
        "authenticated: work mode {:?}, screen {:?}, day or night mode {:?}",
        accepted.session.work_mode,
        accepted.session.screen_info,
        accepted.session.day_or_night_mode
    );
    let day_night = DayNight::new(ctx.carplay.clone(), accepted.session.day_or_night_mode);
    // The picture this head unit asked for last time is the iPhone's until its configuration comes; until then the
    // cast's M4 claims it, or what the authentication reply gives.
    let kept = owner.as_deref().and_then(|owner| kept_picture(ctx, owner));
    if let Some(kept) = &kept {
        ctx.carplay.follow_picture(Some((kept, Source::Kept)));
    }
    let screen = accepted
        .session
        .screen_info
        .as_ref()
        .and_then(HeadUnitPicture::from_screen_info);
    let (head_unit_picture, known_picture) = watch::channel(
        kept.map(|kept| (kept, Source::Kept))
            .or(screen.map(|screen| (screen, Source::ScreenInfo))),
    );
    // How the cast shows: as the settings start it, in the window mode the head unit picked, on a screen of the shape
    // its picture best known has.
    let window_mode = accepted
        .session
        .work_mode
        .and_then(WindowMode::from_work_mode);
    let mut display = Display {
        layout: own.start,
        window_mode,
        screen: head_unit_picture
            .borrow()
            .map_or(Shape::Landscape, |(unit, _)| {
                Shape::of(unit.width, unit.height)
            }),
    };
    log!("cast {display}");
    if let (Some(owner), Some(mode)) = (owner.as_deref(), window_mode) {
        keep_window(ctx, owner, mode);
    }
    // A window whose view is not known is calibrated before CarPlay shows in it, and CarPlay's picture takes its shape
    // and density once it is.
    let mode = Mode::of(window_shape(&display));
    let calibration = Calibration::new(
        views,
        mode,
        shown_in(&display, *ctx.carplay.watch_picture().borrow()),
    );
    if calibration.calibrating() {
        log!("{}", calibrating(mode));
    } else {
        ctx.carplay.follow_shape(window_shape(&display));
    }
    let (screen, screen_rx) = watch::channel(calibration.screen());
    let calibration = Arc::new(Mutex::new(calibration));
    let (view, view_rx) = watch::channel(View {
        shape: display.shape(),
        wait: false,
        changes: 0,
    });
    let local = lease.address;
    let listeners = session::Listeners::bind_with_tos(local, session::Ports::STANDARD, Some(TOS))
        .await
        .map_err(step("listen"))?;
    let listen = |port| session::listen(SocketAddr::from((local, port)), Some(TOS));
    let rtsp = listen(source::RTSP_PORT).map_err(step("listen"))?;
    let uibc = listen(source::UIBC_PORT).map_err(step("listen"))?;
    let session_keys = accepted.confirm(local).await.map_err(step("confirm"))?;
    let key = session_keys.key;

    // The head unit sizes its window to the picture CarPlay is shown at, and follows it when it changes.
    let mut shown = ctx.carplay.watch_picture();
    // What the detailed log reports of the connection while it lasts.
    let watching = Watching::default();
    let options = session::Options {
        bluetooth_hash: Some(ctx.bluetooth_hash.clone()),
        picture: picture_size(*shown.borrow_and_update()),
        display,
        media_drops: Some(watching.drops.clone()),
        media_socket: Some(watching.media_socket.clone()),
        ..session::Options::default()
    };
    let (event_tx, mut events) = mpsc::channel(64);
    let (requests, request_rx) = mpsc::channel(REQUEST_QUEUE);
    let mut run = tokio::spawn(session::run(
        listeners,
        key.clone(),
        options,
        event_tx,
        request_rx,
    ));
    let (cast_music, music_rx) = mpsc::channel(MUSIC_QUEUE);
    let (hevc, hevc_rx) = watch::channel(own.hevc);
    let mut audio = ctx.carplay.subscribe_audio();
    let mut sound = Sound::new();
    sound.set_nav(own.nav_audio);
    // What plays on the iPhone, once the control channel is up: what is known then goes first, then each change.
    let mut now_playing = ctx.carplay.watch_now_playing();
    let mut music = Music::new(music::window_app(window_mode));
    let mut control_up = false;
    let mut cast = tokio::spawn(cast(
        rtsp,
        local,
        ctx.options.clone(),
        CastFollows {
            head_unit_picture: known_picture,
            view: view_rx,
            picture: own.picture,
            hevc: hevc_rx,
            screen: screen_rx,
            stalls: watching.stalls.clone(),
            rtp_socket: watching.rtp_socket.clone(),
        },
        ctx.carplay.clone(),
        music_rx,
        ctx.status.clone(),
    ));
    let (measured_tx, mut measured) = mpsc::channel(MEASURED_QUEUE);
    let input = tokio::spawn(read_input(
        uibc,
        key,
        ctx.carplay.clone(),
        calibration.clone(),
        measured_tx,
    ));
    let mic = watching.mic.clone();
    let drops = watching.drops.clone();
    let diagnostics = tokio::spawn(diagnostics(watching, ctx.options.iface.clone()));

    let end = loop {
        tokio::select! {
            ended = &mut run => break ended.map_err(step("session"))?.map_err(step("session"))?,
            result = &mut cast => {
                match result {
                    Ok(Ok(())) => log!("cast ended"),
                    Ok(Err(err)) => log!("cast: {err}"),
                    Err(err) => log!("cast: {err}"),
                }
                let _ = requests.send(session::Request::Stop).await;
                break (&mut run).await.map_err(step("session"))?.map_err(step("session"))?;
            }
            Some(event) = events.recv() => match event {
                session::Event::Key(key) => ctx.carplay.key(i64::from(key.action), i64::from(key.keycode)),
                session::Event::DayNight(mode) => day_night.set(mode),
                session::Event::Certificate(content) => name_car_tile(ctx, owner.as_deref(), &content),
                session::Event::Config(config) => follow_config(ctx, owner.as_deref(), &config, &head_unit_picture),
                session::Event::AppState(state) => log!("{}", session::describe_app_state(&state)),
                session::Event::ControlConnected => {
                    log_event(&session::Event::ControlConnected);
                    control_up = true;
                    let out = sound.control_up();
                    route_audio(out, &requests, &cast_music, &drops).await;
                    let playing_now = now_playing.borrow_and_update().clone();
                    tell_music(&mut music, playing_now.as_ref(), &requests).await;
                }
                session::Event::InvokeApp(invoke) => {
                    log!("InvokeApp({invoke:?})");
                    // The music info names the app the window casts, for the capsule to show it.
                    let leaves = matches!(
                        AppInvokeCategory::try_from(invoke.invoke_state),
                        Ok(AppInvokeCategory::InvokeStop | AppInvokeCategory::InvokePause)
                    );
                    if WindowMode::from_app(invoke.id).is_some() && !leaves && music.set_app(invoke.id) && control_up {
                        let playing_now = now_playing.borrow().clone();
                        tell_music(&mut music, playing_now.as_ref(), &requests).await;
                    }
                }
                session::Event::Display(shown_now) => {
                    log!("the head unit shows the cast {shown_now}");
                    if let (Some(owner), Some(mode)) = (owner.as_deref(), shown_now.window_mode) {
                        keep_window(ctx, owner, mode);
                    }
                    display = shown_now;
                    let mode = Mode::of(window_shape(&display));
                    let picture = shown_in(&display, *shown.borrow());
                    let (was, now) = {
                        let mut calibration = calibration.lock().unwrap();
                        let was = calibration.calibrating();
                        (was, calibration.display(mode, picture))
                    };
                    // The cast learns of the new view first, while it can still tell whether the iPhone shows; on the
                    // calibration screen it does not.
                    view.send_modify(|view| {
                        if view.shape != display.shape() {
                            view.shape = display.shape();
                            view.wait = ctx.carplay.active() && !now;
                        }
                        view.changes += 1;
                    });
                    if now {
                        log!("{}", calibrating(mode));
                        screen.send_replace(Screen::Calibrate);
                    } else {
                        if was {
                            log!("the {mode} needs no calibration: the calibration screen gives way");
                            screen.send_replace(Screen::CarPlay);
                        }
                        ctx.carplay.follow_shape(window_shape(&display));
                    }
                }
                session::Event::Media(message)
                    if message.category == Category::AUDIO && message.command == AudioType::AudioMicrophone as u16 =>
                {
                    if let Some((stream_type, frames)) = sound.microphone(&message.body) {
                        let detailed = crate::logfile::detailed();
                        if detailed {
                            mic.lock().unwrap().received(&message.body, std::time::Instant::now());
                        }
                        for frame in frames {
                            let outcome = ctx.carplay.microphone(stream_type, &frame);
                            if detailed {
                                mic.lock().unwrap().sent(outcome);
                            }
                        }
                    }
                }
                event => log_event(&event),
            },
            Some(event) = audio.recv() => {
                if let AudioEvent::MicrophoneStarted { sample_rate, channels, frame_bytes, .. } = &event {
                    let format = Format { sample_rate: *sample_rate, channels: *channels };
                    mic.lock().unwrap().iphone_asked(format, *frame_bytes);
                }
                let out = sound.take(event);
                note_microphone(&out, &mic);
                route_audio(out, &requests, &cast_music, &drops).await;
            }
            Ok(()) = now_playing.changed(), if control_up => {
                let playing_now = now_playing.borrow_and_update().clone();
                tell_music(&mut music, playing_now.as_ref(), &requests).await;
            }
            Some(Measurement { mode, measured, calibrated }) = measured.recv() => {
                let views = *calibration.lock().unwrap().views();
                if let Some(owner) = owner.as_deref() {
                    if let Err(err) = ctx.store.set_car_views(owner, &views) {
                        log!("keeping the head unit's views: {err}");
                    }
                }
                let current = calibration.lock().unwrap().mode() == mode;
                if calibrated && current {
                    // CarPlay takes the window's shape and the density of its view at once: one new session at most.
                    log!("the head unit's {mode} is calibrated: view {measured}");
                    ctx.carplay.follow_views(&views, Some(window_shape(&display)), true);
                    screen.send_replace(Screen::Calibrated);
                } else {
                    log!("the head unit's {mode} view is {measured}");
                    ctx.carplay.follow_views(&views, None, false);
                }
                ctx.publish_head_units();
            }
            Ok(()) = shown.changed() => {
                let picture = picture_size(*shown.borrow_and_update());
                calibration.lock().unwrap().picture(picture.map(|size| display.picture(size)));
                let _ = requests.send(session::Request::Picture(picture)).await;
            }
            // The head unit's settings changed: its picture follows at once, its H.265 mode through the cast, and
            // how CarPlay starts at its next connection. Views cleared are measured again: its windows are calibrated
            // when it next switches to them.
            Ok(()) = settings_changed.changed() => {
                let ids = settings_changed.borrow_and_update().clone();
                if owner.as_deref().is_some_and(|owner| ids.iter().any(|id| id == owner)) {
                    let views = own_views(ctx, owner.as_deref());
                    let cleared = {
                        let mut calibration = calibration.lock().unwrap();
                        let changed = *calibration.views() != views;
                        calibration.set_views(views);
                        changed
                    };
                    if cleared {
                        log!("head unit views: {views}");
                        ctx.carplay.follow_views(&views, None, false);
                    }
                    let new = Effective::new(&ctx.options, &own_settings(ctx, owner.as_deref()));
                    if new.start != own.start {
                        log!("CarPlay starts {} on this head unit from its next connection", layout_name(new.start));
                    }
                    if new.picture != own.picture {
                        ctx.carplay.follow_settings(&new, None);
                    }
                    hevc.send_if_modified(|mode| std::mem::replace(mode, new.hevc) != new.hevc);
                    sound.set_nav(new.nav_audio);
                    own = new;
                }
            }
            _ = ctx.wait_shutdown() => {
                let _ = requests.send(session::Request::Stop).await;
                break (&mut run).await.map_err(step("session"))?.map_err(step("session"))?;
            }
        }
    };
    ctx.carplay.unsubscribe_audio();
    cast.abort();
    input.abort();
    diagnostics.abort();
    Ok(end)
}

/// Tells the head unit what plays on the iPhone, when that is news to it. The message waits for room in the
/// session's queue for a while; one that does not go out has the next carry the cover again.
async fn tell_music(
    music: &mut Music,
    now: Option<&NowPlaying>,
    requests: &mpsc::Sender<session::Request>,
) {
    let Some(info) = music.info(now, std::time::Instant::now()) else {
        return;
    };
    // Logged when a cover goes or nothing plays any more; CarPlay logs the other changes as they come.
    let news = info.cover_art_bitmap.is_some() || info.id == Some(-1);
    let message = music::message(&info);
    let sent = timeout(
        ANNOUNCE_WAIT,
        requests.send(session::Request::Send(message)),
    )
    .await;
    if sent.is_err() {
        music.forget();
        log!("music info dropped: the session's queue stayed full");
    } else if news {
        log!("{}", music::describe(&info));
    }
}

/// What the log says when the head unit's `mode` is calibrated.
fn calibrating(mode: Mode) -> String {
    format!("no view of the head unit's {mode} is known for CarPlay's picture: the calibration screen shows until a touch measures it")
}

/// The picture CarPlay is shown at, as the head unit shows it in `display`: turned to the shape of its view.
fn shown_in(display: &Display, picture: Option<Picture>) -> Option<session::PictureSize> {
    picture_size(picture).map(|size| display.picture(size))
}

/// The shape CarPlay's picture is turned to: the window's while the head unit shows the cast in it, none in full
/// screen, where the picture is as the head unit's screen is.
fn window_shape(display: &Display) -> Option<Shape> {
    (display.layout == Layout::Window).then(|| display.shape())
}

/// Keeps the window mode the head unit `owner` casts in, for the shape of CarPlay's picture at the next start.
fn keep_window(ctx: &Context, owner: &str, mode: WindowMode) {
    if let Err(err) = ctx.store.set_car_window(owner, mode) {
        log!("keeping the head unit's window mode: {err}");
    }
}

/// The size of CarPlay's picture, for the head unit's window.
fn picture_size(picture: Option<Picture>) -> Option<session::PictureSize> {
    picture.map(|picture| session::PictureSize {
        width: picture.width,
        height: picture.height,
    })
}

/// Sends the iPhone's sound where the head unit plays it: music into the cast, everything else as PCM on the media
/// channel, announced on the control channel with the call or Siri it belongs to. Sound that finds the queue full is
/// dropped; an announcement waits for room, for a while.
async fn route_audio(
    out: Vec<Out>,
    requests: &mpsc::Sender<session::Request>,
    music: &mpsc::Sender<Vec<u8>>,
    drops: &session::MediaDrops,
) {
    for out in out {
        match out {
            Out::Music(adts) => {
                if music.try_send(adts).is_err() {
                    drops.add(CAST_QUEUE, AudioType::AudioCastMusic as u16);
                }
            }
            Out::Pcm {
                audio_type,
                samples,
            } => {
                let message = Message::new(
                    Category::AUDIO,
                    audio_type as u16,
                    Kind::Send,
                    BodyFormat::Plain,
                    samples,
                );
                if requests.try_send(session::Request::Media(message)).is_err() {
                    drops.add(SESSION_QUEUE, audio_type as u16);
                }
            }
            control => {
                if let Some(message) = control.control() {
                    let sent = timeout(
                        ANNOUNCE_WAIT,
                        requests.send(session::Request::Send(message)),
                    )
                    .await;
                    if sent.is_err() {
                        log!("audio announcement dropped: the session's queue stayed full");
                    }
                }
            }
        }
    }
}

/// Follows the microphone in what goes to the head unit, for its statistics: it records while it is told to.
fn note_microphone(out: &[Out], mic: &Mutex<MicStats>) {
    for out in out {
        if let Out::Microphone { on, format } = out {
            let last = mic
                .lock()
                .unwrap()
                .head_unit_asked(*on, *format, std::time::Instant::now());
            if let Some(last) = last {
                detail!("{last}");
            }
        }
    }
}

/// What the detailed log reports of a connection to a head unit: what its parts count and the sockets they show.
#[derive(Clone)]
struct Watching {
    mic: Arc<Mutex<MicStats>>,
    drops: Arc<session::MediaDrops>,
    stalls: Arc<Stalls>,
    rtp_socket: Arc<session::SocketWatch>,
    media_socket: Arc<session::SocketWatch>,
}

impl Default for Watching {
    fn default() -> Watching {
        Watching {
            mic: Arc::new(Mutex::new(MicStats::new(std::time::Instant::now()))),
            drops: Arc::default(),
            stalls: Arc::default(),
            rtp_socket: Arc::default(),
            media_socket: Arc::default(),
        }
    }
}

/// Reports a connection to the detailed log while it is on: every second the microphone while it records and the
/// sound dropped; every few seconds the cast's stalled writes, the TCP state of the cast and of the media channel while
/// they are connected, and the Wi-Fi link of `iface`. What is counted while it is off is dropped.
async fn diagnostics(watching: Watching, iface: String) {
    let station = Station::new(&iface);
    let mut tick = tokio::time::interval(DIAGNOSTICS_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ticks: u64 = 0;
    let mut tcp_before: [Option<crate::diagnostics::TcpHealth>; 2] = [None, None];
    let mut wifi_before: Option<WifiStation> = None;
    loop {
        tick.tick().await;
        ticks += 1;
        let link_tick = ticks % LINK_DIAGNOSTICS_EVERY == 0;
        let mic = watching
            .mic
            .lock()
            .unwrap()
            .report(std::time::Instant::now());
        let drops = watching.drops.take();
        let stalls = link_tick.then(|| watching.stalls.take());
        if !crate::logfile::detailed() {
            tcp_before = [None, None];
            wifi_before = None;
            continue;
        }
        if let Some(mic) = mic {
            detail!("{mic}");
        }
        if let Some(line) = describe_drops(&drops, "1 s") {
            detail!("{line}");
        }
        if !link_tick {
            continue;
        }
        if let Some(line) = stalls.and_then(|stalls| describe_stalls(stalls, "5 s")) {
            detail!("{line}");
        }
        for ((name, socket), before) in [
            ("cast", &watching.rtp_socket),
            ("media", &watching.media_socket),
        ]
        .into_iter()
        .zip(tcp_before.iter_mut())
        {
            match socket.with(tcp_health) {
                Some(Ok(health)) => {
                    detail!("{}", describe_tcp(name, before.as_ref(), &health));
                    *before = Some(health);
                }
                Some(Err(err)) => {
                    detail!("tcp {name}: {err}");
                    *before = None;
                }
                None => *before = None,
            }
        }
        // Drivers that dump no stations still report the link, with less in it.
        let mut report = station.station_dump().await;
        if report
            .as_ref()
            .is_ok_and(|dump| WifiStation::parse(dump).is_none())
        {
            report = station.link().await;
        }
        match report {
            Ok(report) => match WifiStation::parse(&report) {
                Some(mut now) => {
                    if now.tx_packets.is_none() {
                        now.tx_packets = interface_counter(&iface, "tx_packets");
                    }
                    now.tx_errors = interface_counter(&iface, "tx_errors");
                    now.tx_dropped = interface_counter(&iface, "tx_dropped");
                    detail!("{}", describe_wifi(&iface, wifi_before.as_ref(), &now));
                    wifi_before = Some(now);
                }
                None => {
                    detail!("wifi {iface}: not associated");
                    wifi_before = None;
                }
            },
            Err(err) => detail!("wifi {iface}: {err}"),
        }
    }
}

/// The ID the tile and the picture of the head unit `id`, just authenticated, are kept under: that of the key it
/// authenticated with ([`Store::key_owner`]). Also keeps it as the head unit connected with last.
fn key_owner(ctx: &Context, id: &str) -> Option<String> {
    let owner = match ctx.store.key_owner(id) {
        Ok(owner) => owner,
        Err(err) => {
            log!("finding the head unit's key: {err}");
            return None;
        }
    };
    if let Some(owner) = &owner {
        if let Err(err) = ctx.store.set_last_connected(owner) {
            log!("keeping the head unit connected last: {err}");
        }
    }
    owner
}

/// Names CarPlay's car tile after the head unit, from its certificate data, and keeps it under `tile_id`; data that
/// cannot be read leaves the tile as it is.
fn name_car_tile(ctx: &Context, tile_id: Option<&str>, content: &[u8]) {
    let Some(identity) = HeadUnitIdentity::parse(content) else {
        log!(
            "head unit certificate of {} bytes cannot be read: {:?}; the car tile stays as it is",
            content.len(),
            String::from_utf8_lossy(&content[..content.len().min(48)])
        );
        return;
    };
    log!(
        "head unit certificate: short name {:?}, product name {:?}, vendor {:?}",
        identity.short_name,
        identity.product_name,
        identity.vendor_id
    );
    if let Some(id) = tile_id {
        if let Err(err) = ctx.store.set_car_tile(id, &identity) {
            log!("keeping the head unit's tile: {err}");
        }
    }
    ctx.carplay.name_car_tile(&identity);
}

/// The picture the head unit `owner` asked for last time, if it is kept.
fn kept_picture(ctx: &Context, owner: &str) -> Option<HeadUnitPicture> {
    match ctx.store.car_picture(owner) {
        Ok(picture) => picture,
        Err(err) => {
            log!("reading the head unit's picture: {err}");
            None
        }
    }
}

/// Follows the picture the head unit's configuration asks for: the cast's M4 claims it if it is still to come, the
/// iPhones render it, and it is kept under `owner` for the head unit's next connection and CarBridge's next start.
fn follow_config(
    ctx: &Context,
    owner: Option<&str>,
    config: &GetUCarConfigResponse,
    known: &watch::Sender<Option<(HeadUnitPicture, Source)>>,
) {
    log!(
        "head unit screen {}x{}, picture {}x{}, {} dpi (real {:?}), {} fps",
        config.screen_width,
        config.screen_height,
        config.video_display_width,
        config.video_display_height,
        config.dpi,
        config.real_dpi,
        config.fps
    );
    let Some(picture) = HeadUnitPicture::from_config(config) else {
        log!(
            "the head unit's configuration names no picture size; CarPlay's picture stays as it is"
        );
        return;
    };
    known.send_replace(Some((picture, Source::Config)));
    if let Some(owner) = owner {
        if let Err(err) = ctx.store.set_car_picture(owner, &picture) {
            log!("keeping the head unit's picture: {err}");
        }
    }
    ctx.carplay.follow_picture(Some((&picture, Source::Config)));
    ctx.publish_head_units();
}

fn log_event(event: &session::Event) {
    match event {
        session::Event::Certificate(_) | session::Event::Sensor(_) | session::Event::Media(_) => {}
        session::Event::Control(message) => log!(
            "control message {} kind {:?}",
            message.command,
            message.kind
        ),
        other => log!("{other:?}"),
    }
}

/// The head unit's day or night mode, passed on to CarPlay while the connection lasts and taken back when it ends.
struct DayNight {
    carplay: Arc<CarPlay>,
}

impl DayNight {
    /// Starts with the mode the head unit gave in its authentication response, if it gave one.
    fn new(carplay: Arc<CarPlay>, mode: Option<i32>) -> DayNight {
        let day_night = DayNight { carplay };
        if let Some(mode) = mode {
            day_night.set(mode);
        }
        day_night
    }

    /// Takes a mode the head unit gave, a `DayNightMode`; one that is neither day nor night leaves CarPlay's as it is.
    fn set(&self, mode: i32) {
        match night_of(mode) {
            Some(night) => {
                log!(
                    "head unit in {} mode ({mode})",
                    if night { "night" } else { "day" }
                );
                self.carplay.set_night_mode(Some(night));
            }
            None => log!("head unit day or night mode {mode} unknown; CarPlay's stays as it is"),
        }
    }
}

impl Drop for DayNight {
    fn drop(&mut self) {
        self.carplay.set_night_mode(None);
    }
}

/// Whether the head unit's `DayNightMode` is night, or `None` when it is neither day nor night.
fn night_of(mode: i32) -> Option<bool> {
    match DayNightMode::try_from(mode) {
        Ok(DayNightMode::DayNightDay) => Some(false),
        Ok(DayNightMode::DayNightNight) => Some(true),
        _ => None,
    }
}

/// Offers iPhones H.265 or not as the cast of the moment takes it in the head unit's H.265 mode, and as no cast does
/// once it ends.
struct HevcOffer {
    carplay: Arc<CarPlay>,
    hevc: Hevc,
}

impl HevcOffer {
    fn cast(&self, codec: Codec) {
        self.carplay.set_hevc(offer_hevc(self.hevc, Some(codec)));
    }
}

impl Drop for HevcOffer {
    fn drop(&mut self) {
        self.carplay.set_hevc(offer_hevc(self.hevc, None));
    }
}

/// Casts to the head unit in one codec, chosen at M4 for the head unit's H.265 mode: the waiting and calibration clips
/// and the iPhone's picture alike. M4 claims the timing and level of the picture the head unit's settings choose for
/// its picture best known then. When its H.265 mode changes to one that casts in the other codec, the cast ends, for
/// the head unit to connect again and be cast to in it; the iPhone connects again when it was offered otherwise.
///
/// While a window is calibrated, the calibration screen goes out and the iPhone's pictures do not; once it is, the
/// calibration done screen goes out until a key frame of a session that renders the picture wanted now, for at most
/// [`RESHAPE_LIMIT`].
async fn cast(
    rtsp: TcpListener,
    local: Ipv4Addr,
    options: Options,
    follows: CastFollows,
    carplay: Arc<CarPlay>,
    mut music: mpsc::Receiver<Vec<u8>>,
    status: Arc<StatusFile>,
) -> Result<(), String> {
    let (stream, _) = timeout(RTSP_TIMEOUT, rtsp.accept())
        .await
        .map_err(|_| "the head unit did not open RTSP".to_owned())?
        .map_err(|err| err.to_string())?;
    let CastFollows {
        head_unit_picture,
        mut view,
        picture: settings,
        hevc: mut hevc_mode,
        mut screen,
        stalls,
        rtp_socket,
    } = follows;
    let hevc = *hevc_mode.borrow_and_update();
    let mut offer = HevcOffer {
        carplay: carplay.clone(),
        hevc,
    };
    let mut session_left_out = false;
    let known = *head_unit_picture.borrow();
    // M4 claims the picture's timing and an H.264 level for it, which its density does not change.
    let mode = picture::video_mode(
        &picture::choose(
            &settings,
            known.as_ref().map(|(picture, source)| (picture, *source)),
            &Views::default(),
            picture::Mode::FullScreen,
        )
        .picture,
    );
    let choices = Choices {
        tos: Some(TOS),
        video_format: mode.format(),
        head_unit_dpi: known
            .as_ref()
            .map(|(picture, _)| picture.dpi)
            .filter(|dpi| *dpi > 0)
            .unwrap_or(320),
        ..Choices::new(local)
    };
    let cast = source::negotiate(stream, &choices, |offered| {
        let choice = cast_codec(hevc, offered, carplay.session_codec());
        offer.cast(choice.codec);
        session_left_out = choice.session_left_out;
        choice.codec
    })
    .await
    .map_err(|err| err.to_string())?;
    let codec = cast.codec;
    let offers_hevc = cast.offers_hevc();
    let offered = if offers_hevc {
        "offers H.265"
    } else {
        "does not offer H.265"
    };
    log!("casting {codec} in {mode}; the head unit {offered}");
    if session_left_out {
        log!("CarPlay runs in H.265, which this head unit does not take: the waiting screen stays until the iPhone connects to CarPlay again, in H.264");
    }
    status.update(|s| s.carlink = "casting");
    let (sink_tx, mut sink) = mpsc::channel(8);
    let rtsp_task = tokio::spawn(Cast::serve_rtsp(cast.rtsp, sink_tx));
    let mut clips = Clips::open(&options, codec).await?;
    // What goes out, and since when the calibration done screen does; while CarPlay shows nothing, the screen that
    // tells what it is doing.
    let mut shows = *screen.borrow_and_update();
    let mut state = StateScreen::choose(&status.get().carplay, unix_now());
    log!("while CarPlay shows nothing, the head unit is shown {state}");
    let mut states = tokio::time::interval(Duration::from_secs(1));
    states.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut calibrated_since = (shows == Screen::Calibrated).then(Instant::now);
    let mut reshape = Reshape::new(view.borrow_and_update().shape);
    let mut streamer = Streamer::new(cast.rtp, codec, false).count_stalls(stalls);
    // Declared after the streamer, so that the connection is no longer shown when the streamer closes it.
    let _rtp_watched = session::SocketWatch::watch(&rtp_socket, streamer.socket());
    let mut iphone = carplay.subscribe();
    let started = Instant::now();
    // 90 kHz ticks since the cast started: one clock for the clip and the iPhone alike.
    let now = || (started.elapsed().as_micros() * 9 / 100) as u64;
    // The iPhone's frames go out only from a key frame on, after the clip or a lost frame; until one comes the clip
    // keeps the head unit's picture going, and the iPhone is asked for one every second.
    let mut need_key = true;
    let mut ask_key = tokio::time::interval(Duration::from_secs(1));
    // While CarPlay shows, the ticks are not taken; once a key frame is wanted again, the iPhone is asked once, not
    // once for every second gone by.
    ask_key.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut active = carplay.watch_active();
    // When to ask the iPhone for a picture for the head unit's new view next, and how many times more.
    let mut refresh: Option<(Instant, u32)> = None;
    let result = loop {
        let deadline = reshape.deadline();
        let calibrated_deadline = calibrated_since.map(|since| since + RESHAPE_LIMIT);
        tokio::select! {
            frame = clips.get(shows, state, reshape.shape).next(),
                if clip_goes_out(shows, carplay.active(), need_key, &reshape) => {
                if !carplay.active() {
                    need_key = true;
                }
                let (data, key) = (frame.data.to_vec(), frame.key);
                if let Err(err) = streamer.video(&data, now(), key).await {
                    break Err(err.to_string());
                }
            }
            // The head unit's view takes another shape: the clip of that shape from a key frame on, and the iPhone's
            // pictures once it draws for it.
            Ok(()) = view.changed() => {
                let View { shape, wait, .. } = *view.borrow_and_update();
                if shape != reshape.shape {
                    if reshape.reshape(shape, wait, std::time::Instant::now()) {
                        log!("the head unit's view is {shape} now: nothing goes out until CarPlay draws for it");
                    } else {
                        log!("the head unit's view is {shape} now");
                    }
                    clips.get(shows, state, shape).key_frame();
                    need_key = true;
                }
                // The head unit shows its new view from the next picture on, which an iPhone whose screen stands
                // still does not send by itself.
                refresh = Some((Instant::now() + VIEW_REFRESH, VIEW_REFRESHES));
            }
            _ = states.tick() => {
                let now = StateScreen::choose(&status.get().carplay, unix_now());
                if now != state {
                    log!("while CarPlay shows nothing, the head unit is shown {now}");
                    state = now;
                    clips.get(shows, state, reshape.shape).key_frame();
                }
            }
            Ok(()) = screen.changed() => {
                let now = *screen.borrow_and_update();
                if now != shows {
                    log!("the cast shows {now}");
                }
                shows = now;
                calibrated_since = (shows == Screen::Calibrated).then(Instant::now);
                clips.get(shows, state, reshape.shape).key_frame();
                need_key = true;
            }
            () = sleep_until_some(calibrated_deadline) => {
                log!(
                    "CarPlay did not show in the calibrated window within {} s; the cast goes on",
                    RESHAPE_LIMIT.as_secs()
                );
                shows = Screen::CarPlay;
                calibrated_since = None;
                clips.get(shows, state, reshape.shape).key_frame();
                need_key = true;
            }
            () = sleep_until_some(refresh.map(|(at, _)| at)) => {
                if carplay.active() && reshape.takes(carplay.session_shape())
                    && takes_session(shows, carplay.session_picture(), carplay.next_picture()) {
                    carplay.request_key_frame();
                }
                refresh = refresh.and_then(|(at, left)| (left > 1).then(|| (at + VIEW_REFRESH, left - 1)));
            }
            () = sleep_until_some(deadline) => {
                reshape.give_up();
                log!(
                    "CarPlay did not draw for the head unit's {} view within {} s; the cast goes on",
                    reshape.shape,
                    RESHAPE_LIMIT.as_secs()
                );
                clips.get(shows, state, reshape.shape).key_frame();
                need_key = true;
            }
            // The clip takes over, from a key frame, as soon as the iPhone's screen stops.
            Ok(()) = active.changed() => {
                if !*active.borrow_and_update() && !need_key {
                    log!("CarPlay picture gone; the waiting clip from a key frame on");
                    need_key = true;
                    clips.get(shows, state, reshape.shape).key_frame();
                }
            }
            _ = ask_key.tick(), if carplay.active() && need_key && carplay.session_codec() == Some(codec)
                && reshape.takes(carplay.session_shape())
                && takes_session(shows, carplay.session_picture(), carplay.next_picture()) => {
                carplay.request_key_frame();
            }
            Ok(()) = hevc_mode.changed() => {
                let hevc = *hevc_mode.borrow_and_update();
                let wanted = cast_codec(hevc, offers_hevc, None).codec;
                offer.hevc = hevc;
                carplay.renew_hevc(offer_hevc(hevc, Some(wanted)));
                if wanted != codec {
                    log!("the head unit's H.265 setting casts {wanted}: the cast ends, for the head unit to connect again in it");
                    break Ok(());
                }
            }
            Some(adts) = music.recv() => {
                if let Err(err) = streamer.audio(&adts, now()).await {
                    break Err(err.to_string());
                }
            }
            Some(frame) = iphone.recv() => {
                let (shape, session) = (frame.shape, frame.picture);
                let frame = frame.video;
                // A session in another codec is not cast; the clip goes on.
                if cast_codec_of(frame.codec) != codec {
                    continue;
                }
                if carplay.take_lost() {
                    need_key = true;
                }
                if !takes_session(shows, session, carplay.next_picture())
                    || !reshape.takes(shape)
                    || (need_key && !frame.key)
                {
                    continue;
                }
                if let Some(since) = calibrated_since.take() {
                    log!("CarPlay shows in the calibrated window after {:.1} s", since.elapsed().as_secs_f64());
                    shows = Screen::CarPlay;
                }
                if let Some(waited) = reshape.drawn(std::time::Instant::now()) {
                    log!("CarPlay drew for the head unit's {} view after {:.1} s", reshape.shape, waited.as_secs_f64());
                }
                if need_key {
                    log!("CarPlay picture from a key frame on");
                }
                need_key = false;
                if let Err(err) = streamer.video(&frame.data, now(), frame.key).await {
                    break Err(err.to_string());
                }
            }
            event = sink.recv() => match event {
                Some(SinkEvent::IdrRequest) => {
                    clips.get(shows, state, reshape.shape).key_frame();
                    carplay.request_key_frame();
                }
                Some(SinkEvent::Teardown) => break Ok(()),
                Some(SinkEvent::Closed) | None => break Err("RTSP closed".to_owned()),
            },
        }
    };
    carplay.unsubscribe();
    rtsp_task.abort();
    drop(offer);
    result
}

/// What a cast follows of the head unit: its picture best known and how its settings choose the picture, for M4; the
/// shape of its view; its H.265 mode; and what it shows while a window is calibrated.
struct CastFollows {
    head_unit_picture: watch::Receiver<Option<(HeadUnitPicture, Source)>>,
    view: watch::Receiver<View>,
    picture: picture::Settings,
    hevc: watch::Receiver<Hevc>,
    screen: watch::Receiver<Screen>,
    /// Where the cast counts its writes that wait for room, and shows its RTP connection, for the detailed log.
    stalls: Arc<Stalls>,
    rtp_socket: Arc<session::SocketWatch>,
}

/// Whether a clip goes out while the cast shows `shows`: the calibration clips always; else the waiting clip while
/// the iPhone's screen does not show (`active`) or a key frame of it is awaited, unless a reshape holds it back.
fn clip_goes_out(shows: Screen, active: bool, need_key: bool, reshape: &Reshape) -> bool {
    match shows {
        Screen::CarPlay => (!active || need_key) && reshape.clip_goes_out(),
        Screen::Calibrate | Screen::Calibrated => true,
    }
}

/// Whether the pictures of the iPhone's session, which renders `session`, go out while the cast shows `shows`: always
/// for CarPlay, never on the calibration screen, and once a window is calibrated only those of a session that renders
/// `next`, the picture wanted now.
fn takes_session(shows: Screen, session: Option<Picture>, next: Option<Picture>) -> bool {
    match shows {
        Screen::CarPlay => true,
        Screen::Calibrate => false,
        Screen::Calibrated => session.is_some() && session == next,
    }
}

/// The head unit's view: its shape, whether the iPhone's screen showed when it took it, and how many times the head
/// unit set its view up anew, as it does for every switch between full screen and the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct View {
    shape: Shape,
    wait: bool,
    changes: u64,
}

/// Clips of one codec for a view of either shape.
struct ShapedClips {
    landscape: Clip,
    /// `None` when there is no portrait clip: the landscape one shows in a portrait view as well.
    portrait: Option<Clip>,
}

impl ShapedClips {
    /// The `what` clips at `landscape` and `portrait`; the landscape one is needed.
    async fn open(
        landscape: &std::path::Path,
        portrait: &std::path::Path,
        codec: Codec,
        what: &str,
    ) -> Result<ShapedClips, String> {
        let open = |path: &std::path::Path| {
            let path = path.to_owned();
            async move {
                Clip::open(&path, codec, FPS)
                    .await
                    .map_err(|err| format!("{}: {err}", path.display()))
            }
        };
        let landscape = open(landscape).await?;
        let portrait = match open(portrait).await {
            Ok(clip) => Some(clip),
            Err(err) => {
                log!("no portrait {what} clip ({err}); the landscape one shows in a portrait view");
                None
            }
        };
        Ok(ShapedClips {
            landscape,
            portrait,
        })
    }

    /// The clip for a view of `shape`.
    fn get(&mut self, shape: Shape) -> &mut Clip {
        match (shape, &mut self.portrait) {
            (Shape::Portrait, Some(portrait)) => portrait,
            _ => &mut self.landscape,
        }
    }
}

/// The clips of one codec the cast shows while CarPlay does not: the waiting clip, those of the other screens that
/// tell what CarPlay is doing, and the two of the calibration of a window; the waiting clip stands in for any that is
/// missing.
struct Clips {
    waiting: ShapedClips,
    states: Vec<(StateScreen, ShapedClips)>,
    calibrate: Option<ShapedClips>,
    calibrated: Option<ShapedClips>,
}

impl Clips {
    async fn open(options: &Options, codec: Codec) -> Result<Clips, String> {
        let waiting = ShapedClips::open(
            options.waiting_clip(codec, Shape::Landscape),
            options.waiting_clip(codec, Shape::Portrait),
            codec,
            "waiting",
        )
        .await?;
        let calibration = |done: bool| async move {
            let what = if done {
                "calibration done"
            } else {
                "calibration"
            };
            let clips = ShapedClips::open(
                &options.calibration_clip(done, codec, Shape::Landscape),
                &options.calibration_clip(done, codec, Shape::Portrait),
                codec,
                what,
            )
            .await;
            match clips {
                Ok(clips) => Some(clips),
                Err(err) => {
                    log!("no {what} clip ({err}); the waiting clip shows instead");
                    None
                }
            }
        };
        let mut states = Vec::new();
        let mut missing = Vec::new();
        for screen in StateScreen::NAMED {
            let Some(name) = screen.clip_name() else {
                continue;
            };
            let clips = ShapedClips::open(
                &options.screen_clip(name, codec, Shape::Landscape),
                &options.screen_clip(name, codec, Shape::Portrait),
                codec,
                name,
            )
            .await;
            match clips {
                Ok(clips) => states.push((screen, clips)),
                Err(_) => missing.push(name),
            }
        }
        if !missing.is_empty() {
            log!(
                "no {} clip in {}; the waiting clip shows instead",
                missing.join(", "),
                options.screen_dir.display()
            );
        }
        Ok(Clips {
            waiting,
            states,
            calibrate: calibration(false).await,
            calibrated: calibration(true).await,
        })
    }

    /// The clip for a view of `shape` while the cast shows `shows`, and `state` while CarPlay shows nothing.
    fn get(&mut self, shows: Screen, state: StateScreen, shape: Shape) -> &mut Clip {
        let clips = match shows {
            Screen::Calibrate => self.calibrate.as_mut(),
            Screen::Calibrated => self.calibrated.as_mut(),
            Screen::CarPlay => self
                .states
                .iter_mut()
                .find(|(screen, _)| *screen == state)
                .map(|(_, clips)| clips),
        };
        match clips {
            Some(clips) => clips.get(shape),
            None => self.waiting.get(shape),
        }
    }
}

/// Holds back what the head unit would show squeezed once its view took another shape. Until the iPhone connected
/// again draws for the new shape, its pictures of the old one go out to no one, and neither does the waiting clip,
/// so the head unit shows nothing rather than a picture of the wrong shape; for at most [`RESHAPE_LIMIT`]. After
/// that, the clip goes out again, and so do the iPhone's pictures of either shape, until the view's shape changes
/// once more.
struct Reshape {
    /// The shape of the head unit's view.
    shape: Shape,
    /// Since when the cast waits for the iPhone to draw for it.
    waiting: Option<std::time::Instant>,
    /// Whether pictures of the other shape are held back.
    strict: bool,
}

impl Reshape {
    fn new(shape: Shape) -> Reshape {
        Reshape {
            shape,
            waiting: None,
            strict: true,
        }
    }

    /// The view takes `shape` at `now`; `wait` when the iPhone's screen showed, which it then draws again. Returns
    /// whether the cast waits for it.
    fn reshape(&mut self, shape: Shape, wait: bool, now: std::time::Instant) -> bool {
        if shape == self.shape {
            return self.waiting.is_some();
        }
        self.shape = shape;
        self.strict = true;
        self.waiting = wait.then_some(now);
        wait
    }

    /// When the wait ends at the latest.
    fn deadline(&self) -> Option<Instant> {
        self.waiting
            .map(|since| Instant::from_std(since) + RESHAPE_LIMIT)
    }

    /// The wait ran out: everything goes out again.
    fn give_up(&mut self) {
        self.waiting = None;
        self.strict = false;
    }

    /// Whether the waiting clip goes out.
    fn clip_goes_out(&self) -> bool {
        self.waiting.is_none()
    }

    /// Whether the iPhone's pictures go out when they are of `shape`, `None` for a shape not known.
    fn takes(&self, shape: Option<Shape>) -> bool {
        !self.strict || shape.is_none_or(|shape| shape == self.shape)
    }

    /// A picture of the view's shape goes out at `now`: the wait ends, and how long it took, if the cast waited.
    fn drawn(&mut self, now: std::time::Instant) -> Option<Duration> {
        self.waiting
            .take()
            .map(|since| now.saturating_duration_since(since))
    }
}

/// Waits until `deadline`, or for ever without one.
async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Passes the head unit's touches and keys to the iPhone. Each touch goes through the calibration first, which keeps
/// the view it was made on, holds it back while a window is calibrated and passes the views it measures on.
async fn read_input(
    uibc: TcpListener,
    key: carbridge_carlink_proto::record::SessionKey,
    carplay: Arc<CarPlay>,
    calibration: Arc<Mutex<Calibration>>,
    measured: mpsc::Sender<Measurement>,
) {
    while let Ok((stream, _)) = uibc.accept().await {
        let mut reader = UibcReader::new(stream, key.clone());
        while let Ok(Some(input)) = reader.next().await {
            match input {
                Input::Touch {
                    action,
                    width,
                    height,
                    pointers,
                } => {
                    let touched = calibration.lock().unwrap().touch(action, width, height);
                    if let Some(measurement) = touched.measured {
                        let _ = measured.try_send(measurement);
                    }
                    if touched.forward {
                        let pointers: Vec<_> = pointers.iter().map(|p| (p.id, p.x, p.y)).collect();
                        carplay.touch(action, &pointers);
                    }
                }
                Input::Key {
                    action, keycode, ..
                } => carplay.key(action, keycode),
                Input::Other(value) => log!("input {value}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_failures_say_what_failed() {
        assert_eq!(pin_failure(&Error::PinRefused), ("refused", None, None));
        let failed = |step| pin_failure(&Error::Step(step, "timed out".into()));
        assert_eq!(failed("GATT exchange").1, Some("bluetooth"));
        assert_eq!(failed("LE connect").1, Some("bluetooth"));
        assert_eq!(failed("wpa_supplicant").1, Some("wifi"));
        assert_eq!(failed("DHCP").1, Some("wifi"));
        assert_eq!(failed("authentication").1, Some("authentication"));
        assert_eq!(
            failed("session"),
            ("failed", Some("other"), Some("session: timed out".into()))
        );
    }

    #[test]
    fn a_reshape_holds_back_the_old_shape_until_the_iphone_draws_the_new_one() {
        let start = std::time::Instant::now();
        let later = |seconds: f64| start + Duration::from_secs_f64(seconds);
        let mut reshape = Reshape::new(Shape::Landscape);
        assert!(reshape.clip_goes_out());
        assert!(reshape.takes(Some(Shape::Landscape)) && reshape.takes(None));
        assert!(!reshape.takes(Some(Shape::Portrait)));
        assert_eq!(reshape.deadline(), None);
        // The same shape again changes nothing.
        assert!(!reshape.reshape(Shape::Landscape, true, start));
        assert!(reshape.clip_goes_out());
        // Portrait while the iPhone showed: neither the clip nor the old shape goes out.
        assert!(reshape.reshape(Shape::Portrait, true, start));
        assert!(!reshape.clip_goes_out());
        assert!(!reshape.takes(Some(Shape::Landscape)));
        assert!(reshape.takes(Some(Shape::Portrait)));
        assert!(reshape.deadline().is_some());
        // The first picture of the new shape ends the wait.
        assert_eq!(reshape.drawn(later(5.5)), Some(Duration::from_millis(5500)));
        assert_eq!(reshape.drawn(later(6.0)), None);
        assert!(reshape.clip_goes_out());
        assert_eq!(reshape.deadline(), None);
        assert!(!reshape.takes(Some(Shape::Landscape)));
    }

    #[test]
    fn a_reshape_gives_up_after_its_limit() {
        let start = std::time::Instant::now();
        let mut reshape = Reshape::new(Shape::Portrait);
        reshape.reshape(Shape::Landscape, true, start);
        let deadline = reshape.deadline().unwrap();
        assert_eq!(deadline - Instant::from_std(start), RESHAPE_LIMIT);
        reshape.give_up();
        assert!(reshape.clip_goes_out());
        // After the limit, pictures of either shape go out, until the next change of shape.
        assert!(reshape.takes(Some(Shape::Portrait)) && reshape.takes(Some(Shape::Landscape)));
        reshape.reshape(Shape::Portrait, false, start);
        assert!(!reshape.takes(Some(Shape::Landscape)));
        // Without the iPhone's screen, nothing to wait for: the clip of the new shape goes out at once.
        assert!(reshape.clip_goes_out());
        assert_eq!(reshape.deadline(), None);
    }

    #[test]
    fn what_goes_out_while_a_window_is_calibrated() {
        let reshape = Reshape::new(Shape::Landscape);
        // CarPlay: the clip only while the iPhone's screen does not show or a key frame is awaited.
        assert!(clip_goes_out(Screen::CarPlay, false, false, &reshape));
        assert!(clip_goes_out(Screen::CarPlay, true, true, &reshape));
        assert!(!clip_goes_out(Screen::CarPlay, true, false, &reshape));
        // The calibration screens go out whatever the iPhone does, also while a reshape waits.
        let mut waiting = Reshape::new(Shape::Landscape);
        waiting.reshape(Shape::Portrait, true, std::time::Instant::now());
        for shows in [Screen::Calibrate, Screen::Calibrated] {
            assert!(clip_goes_out(shows, true, false, &reshape));
            assert!(clip_goes_out(shows, true, false, &waiting));
        }
        assert!(!clip_goes_out(Screen::CarPlay, false, true, &waiting));
        // The iPhone's pictures: none on the calibration screen; after it, only from the new session.
        let old = Picture {
            dpi: Some(206),
            ..Picture::new(2458, 1484, 30)
        };
        let new = Picture {
            dpi: Some(296),
            ..old
        };
        assert!(takes_session(Screen::CarPlay, Some(old), Some(new)));
        assert!(!takes_session(Screen::Calibrate, Some(new), Some(new)));
        assert!(!takes_session(Screen::Calibrated, Some(old), Some(new)));
        assert!(!takes_session(Screen::Calibrated, None, Some(new)));
        assert!(takes_session(Screen::Calibrated, Some(new), Some(new)));
    }

    #[test]
    fn the_picture_as_the_view_shows_it() {
        let simple = Display {
            layout: Layout::Window,
            window_mode: Some(WindowMode::Simple),
            screen: Shape::Landscape,
        };
        let picture = Picture::new(2458, 1484, 30);
        assert_eq!(
            shown_in(&simple, Some(picture)),
            Some(session::PictureSize {
                width: 1484,
                height: 2458
            })
        );
        let full = Display {
            layout: Layout::FullScreen,
            ..simple
        };
        assert_eq!(
            shown_in(&full, Some(picture)),
            Some(session::PictureSize {
                width: 2458,
                height: 1484
            })
        );
        assert_eq!(shown_in(&full, None), None);
    }

    #[test]
    fn the_picture_turns_only_in_the_window() {
        let landscape = Display {
            layout: Layout::FullScreen,
            window_mode: Some(WindowMode::Simple),
            screen: Shape::Landscape,
        };
        assert_eq!(window_shape(&landscape), None);
        let simple = Display {
            layout: Layout::Window,
            ..landscape
        };
        assert_eq!(window_shape(&simple), Some(Shape::Portrait));
        let mirror = Display {
            window_mode: Some(WindowMode::Mirror),
            ..simple
        };
        assert_eq!(window_shape(&mirror), Some(Shape::Landscape));
    }

    #[test]
    fn day_and_night_of_the_head_unit() {
        assert_eq!(night_of(1), Some(false));
        assert_eq!(night_of(2), Some(true));
        assert_eq!(night_of(0), None);
        assert_eq!(night_of(7), None);
    }
}
