// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! CarPlay: the iPhone pairs and starts wireless CarPlay over Bluetooth, then casts its screen to the AirPlay
//! receiver over the router's access point. The receiver runs for the life of the program, across connections to
//! head units; the cast of the moment takes its frames.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use carbridge_carlink_proto::certificate::HeadUnitIdentity;
use carbridge_carlink_session::display::Shape;
use carbridge_carlink_wfd::Codec;
use carbridge_carplay::input::{Key, TouchMapper};
use carbridge_carplay::now_playing::NowPlaying;
use carbridge_carplay::picture::Picture;
use carbridge_carplay::receiver::{
    AudioEvent, MicrophoneError, MicrophoneSent, Receiver, ReceiverConfig, ReceiverEvent,
};
use carbridge_carplay::tile::{CarTile, Choice};
use carbridge_carplay::video::{VideoCodec, VideoFrame};
use carbridge_carplay::wireless::{self, Accessory, Network, SessionWatch, Stall, Step};
use carbridge_platform::access_point;
use carbridge_platform::classic::{
    AgentEvent, Classic, DeviceChange, DeviceChanges, ProfileConnection, Role,
};
use carbridge_platform::{mgmt, rfcomm};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch, Notify};

use crate::diagnostics::{MicOutcome, Throttle};
use crate::options::Options;
use crate::picture::{self, HeadUnitPicture, Settings, Source};
use crate::settings::Effective;
use crate::sound::Replay;
use crate::status::{unix_now, PairedIphone, PairingError, StatusFile, Unavailable};
use crate::video::offer_hevc;
use crate::views::Views;

/// Frames waiting for the cast; a slow cast loses frames and waits for the next key frame.
const QUEUE: usize = 32;
/// Audio events waiting for the connection.
const AUDIO_QUEUE: usize = 256;
/// The accessory's iAP2 service, by which iPhones recognise a CarPlay car over Bluetooth.
const IAP2_ACCESSORY_UUID: &str = "00000000-deca-fade-deca-deafdecacaff";
/// The iPhone's iAP2 service, which the accessory connects to.
const IAP2_DEVICE_UUID: &str = "00000000-deca-fade-deca-deafdecacafe";
/// The service by which a car offers CarPlay over Bluetooth, next to the iAP2 one.
const CARPLAY_UUID: &str = "ec884348-cd41-40a2-9727-575d50bf1fd3";
/// The RFCOMM channels iPhones open the two services on, away from those of bluetoothd's own profiles.
const IAP2_CHANNEL: u8 = 22;
const CARPLAY_CHANNEL: u8 = 23;
/// How long to wait before connecting to an iPhone's iAP2 service again; doubled after each failure up to the
/// longest wait.
const IAP2_RETRY: Duration = Duration::from_secs(5);
const IAP2_RETRY_LONGEST: Duration = Duration::from_secs(60);
/// How long a newly paired iPhone is called no later than, when bluetoothd has not learned its iAP2 service by then.
const IAP2_AFTER_PAIRING: Duration = Duration::from_secs(5);
/// How soon to call again when bluetoothd is still busy with the iPhone.
const IAP2_BUSY_RETRY: Duration = Duration::from_secs(1);
/// How long Bluetooth is kept after the iPhone asked to give it up, waiting for iAP2 to be up inside the session.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(45);
/// How long an iPhone may hold Bluetooth with no iAP2 link and no session before it is disconnected, to call it
/// afresh.
const IDLE_LINK_TIMEOUT: Duration = Duration::from_secs(15);
/// The class of device's service classes: Audio.
const SERVICE_AUDIO: u8 = 0x20;
/// How long each step of opening iAP2 directly may take: the SDP lookup, then the connection.
const DIRECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Class of device: major class audio/video, minor class car audio.
const CLASS_MAJOR_AUDIO_VIDEO: u8 = 0x04;
const CLASS_MINOR_CAR_AUDIO: u8 = 0x08 << 2;
/// Whether an iPhone whose car tile is no longer the head unit's has its session ended, to connect again and read the
/// new one.
const RENEW_CAR_TILE: bool = true;
/// Whether an iPhone whose picture is no longer the one the head unit asks for has its session ended, to connect again
/// and render the new one.
const RENEW_PICTURE: bool = true;
/// How often starting CarPlay is tried again while the receiver or Bluetooth cannot start.
const START_RETRY: Duration = Duration::from_secs(5);
/// How long to wait before opening pairing again when it could not be opened.
const PAIRING_RETRY: Duration = Duration::from_secs(1);

/// A frame of the iPhone's screen, and the picture of the session it comes from, with its shape.
pub struct ScreenFrame {
    pub video: VideoFrame,
    pub shape: Option<Shape>,
    pub picture: Option<Picture>,
}

#[derive(Default)]
struct Feed {
    sender: Mutex<Option<mpsc::Sender<ScreenFrame>>>,
    /// The iPhone's sound, for the connection of the moment.
    audio: Mutex<AudioFeed>,
    /// Whether the iPhone's screen is being received.
    active: watch::Sender<bool>,
    /// A frame was lost: the cast needs a key frame before it can go on.
    lost: AtomicBool,
    /// The steps of an iPhone's move from Bluetooth to Wi-Fi.
    handoff: Mutex<Option<mpsc::UnboundedSender<Handoff>>>,
    /// The pictures CarPlay is rendered at.
    pictures: Mutex<Pictures>,
    /// The picture CarPlay is shown at ([`Pictures::shown`]).
    shown: watch::Sender<Option<Picture>>,
    /// What plays on the iPhone, kept across connections to head units until the iPhone's session ends.
    now_playing: watch::Sender<Option<NowPlaying>>,
}

/// The picture iPhones that connect render and the one the iPhone connected now renders, `None` without the
/// receiver or the iPhone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pictures {
    next: Option<Picture>,
    session: Option<Picture>,
}

impl Pictures {
    /// The picture CarPlay is shown at: the one the iPhone connected now renders, or else the one it renders when
    /// it connects.
    fn shown(&self) -> Option<Picture> {
        self.session.or(self.next)
    }
}

/// The iPhone's sound for the connection of the moment, and the streams that go on, which a connection that begins
/// later is told of first.
#[derive(Default)]
struct AudioFeed {
    sender: Option<mpsc::Sender<AudioEvent>>,
    replay: Replay,
}

/// The steps of an iPhone's move from Bluetooth to Wi-Fi: Bluetooth is let go once the session asks for it and
/// iAP2 is up inside the session, or when iAP2 is late. A link over Bluetooth whose session does not come is started
/// afresh.
enum Handoff {
    /// The session asks for Bluetooth to be given up (`disableBluetooth`); the iPhone's Bluetooth address.
    Requested(String),
    /// iAP2 is up on the tunnel inside the session.
    TunnelReady,
    TunnelEnded,
    /// A step of the bring-up over the iAP2 link of the iPhone at a device path.
    Link(String, Step),
    SessionStarted,
    /// A session ended, or one that was starting gave up.
    SessionEnded,
    AuthenticationFailed,
}

impl Feed {
    /// Changes the pictures CarPlay is rendered at, and with them the one it is shown at.
    fn set_pictures(&self, change: impl FnOnce(&mut Pictures)) {
        let mut pictures = self.pictures.lock().unwrap();
        change(&mut pictures);
        let shown = pictures.shown();
        self.shown.send_if_modified(|current| {
            let changed = *current != shown;
            *current = shown;
            changed
        });
    }

    /// Whether the iPhone connected now renders a picture of another shape than iPhones that connect are given: it
    /// is to connect again for the head unit's new window.
    fn reshaping(&self) -> bool {
        let pictures = self.pictures.lock().unwrap();
        match (pictures.session, pictures.next) {
            (Some(session), Some(next)) => session.is_portrait() != next.is_portrait(),
            _ => false,
        }
    }

    /// Tells the Bluetooth side a step, once it is serving.
    fn step(&self, step: Handoff) {
        if let Some(handoff) = self.handoff.lock().unwrap().as_ref() {
            let _ = handoff.send(step);
        }
    }
}

pub struct CarPlay {
    /// The receiver, once it started.
    receiver: OnceLock<Arc<Receiver>>,
    feed: Arc<Feed>,
    /// The receiver's address, on the access point the iPhone joins, once it started.
    address: OnceLock<IpAddr>,
    touch: Mutex<TouchMapper>,
    status: Arc<StatusFile>,
    /// Head unit makers' icons for the car tile.
    oem_icon_dir: PathBuf,
    /// What the picture follows.
    wanted: Mutex<Wanted>,
    /// When a failure to give the iPhone the microphone was logged last.
    microphone_errors: Mutex<Throttle>,
    /// What the receiver is to give the iPhones, kept for a receiver that starts later: whether H.265 is offered,
    /// the car tile, and the head unit's day or night mode.
    hevc: AtomicBool,
    tile: Mutex<CarTile>,
    night: Mutex<Option<bool>>,
    /// Wakes the start of CarPlay up to try again at once, as when an MFi identity was installed.
    retry: Notify,
    /// When a key from the head unit was logged last, while the detailed log is off.
    keys_logged: Mutex<Throttle>,
}

/// How often a key from the head unit is logged at most while the detailed log is off.
const KEYS_EVERY: Duration = Duration::from_secs(10);

/// How often a failure to give the iPhone the car's microphone is logged at most.
const MICROPHONE_ERRORS_EVERY: Duration = Duration::from_secs(10);

/// What CarPlay's picture follows: the head unit connected now, or else the one connected last; the picture its
/// settings choose for its picture; the views measured of it; and the shape of its window when it shows the cast
/// there, which also gives the mode whose view calibrates the picture's density.
#[derive(Clone, Debug)]
struct Wanted {
    /// The ID the head unit's key is kept under, if it is known.
    head_unit_id: Option<String>,
    /// How the picture is chosen for the head unit.
    settings: Settings,
    head_unit: Option<(HeadUnitPicture, Source)>,
    views: Views,
    window: Option<Shape>,
}

impl Wanted {
    /// The picture, with the density of the head unit's mode, and why it is the one.
    fn choice(&self) -> picture::Choice {
        let choice = picture::choose(
            &self.settings,
            self.head_unit
                .as_ref()
                .map(|(unit, source)| (unit, *source)),
            &self.views,
            picture::Mode::of(self.window),
        );
        match self.window {
            Some(shape) if picture_shape(&choice.picture) != shape => picture::Choice {
                picture: choice.picture.turned(shape == Shape::Portrait),
                reason: format!(
                    "{}, turned {shape} for the head unit's window",
                    choice.reason
                ),
            },
            _ => choice,
        }
    }
}

/// The shape of a picture.
fn picture_shape(picture: &Picture) -> Shape {
    Shape::of(picture.width, picture.height)
}

impl CarPlay {
    /// CarPlay, without its receiver, which [`CarPlay::keep_up`] starts; without it, the cast shows the clip. Until a
    /// head unit connects, CarPlay follows `head_unit_id`, the head unit connected last, and `own`, what it goes by:
    /// iPhones render the picture its settings choose for `kept`, the picture it asked for, with the density its
    /// `views` give, turned to `window`, the shape of its window when the cast starts there, and are offered H.265 by
    /// its mode.
    pub fn new(
        options: &Options,
        status: Arc<StatusFile>,
        head_unit_id: Option<&str>,
        own: &Effective,
        kept: Option<&HeadUnitPicture>,
        views: &Views,
        window: Option<Shape>,
    ) -> CarPlay {
        let wanted = Wanted {
            head_unit_id: head_unit_id.map(str::to_owned),
            settings: own.picture,
            head_unit: kept.map(|kept| (*kept, Source::Kept)),
            views: *views,
            window,
        };
        let initial = wanted.choice();
        log!("CarPlay picture {initial}");
        CarPlay {
            receiver: OnceLock::new(),
            feed: Arc::new(Feed::default()),
            address: OnceLock::new(),
            touch: Mutex::new(TouchMapper::new(
                initial.picture.width,
                initial.picture.height,
            )),
            status,
            oem_icon_dir: options.oem_icon_dir.clone(),
            wanted: Mutex::new(wanted),
            microphone_errors: Mutex::new(Throttle::new(MICROPHONE_ERRORS_EVERY)),
            hevc: AtomicBool::new(offer_hevc(own.hevc, None)),
            tile: Mutex::new(CarTile::default()),
            night: Mutex::new(None),
            retry: Notify::new(),
            keys_logged: Mutex::new(Throttle::new(KEYS_EVERY)),
        }
    }

    /// The receiver, once it started.
    fn receiver(&self) -> Option<&Arc<Receiver>> {
        self.receiver.get()
    }

    /// Starts the receiver, then Bluetooth for iPhones, trying again every few seconds while either cannot start,
    /// until both run or CarBridge stops. Why CarPlay cannot run goes in the status meanwhile; the log tells the first
    /// failure of each kind and when CarPlay can run again. `bluetooth_address` is the adapter's.
    pub async fn keep_up(
        self: Arc<Self>,
        options: Options,
        bluetooth_address: String,
        mut shutdown: watch::Receiver<bool>,
    ) {
        if !options.carplay {
            self.status.update(|s| {
                s.carplay.unavailable = Some(Unavailable {
                    reason: "off",
                    detail: None,
                })
            });
            return;
        }
        let mut failing: Option<&'static str> = None;
        loop {
            let started = self
                .try_start(&options, &bluetooth_address, &shutdown)
                .await;
            let unavailable = started.err();
            match (&unavailable, failing) {
                (Some(now), before) if before != Some(now.reason) => {
                    log!(
                        "CarPlay cannot run: {}; trying again every {} s",
                        now.detail.as_deref().unwrap_or(now.reason),
                        START_RETRY.as_secs()
                    );
                }
                (None, Some(_)) => log!("CarPlay can run again"),
                _ => {}
            }
            failing = unavailable.as_ref().map(|u| u.reason);
            self.status.update(|s| s.carplay.unavailable = unavailable);
            if failing.is_none() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(START_RETRY) => {}
                _ = self.retry.notified() => {}
                _ = shutdown.wait_for(|stop| *stop) => return,
            }
        }
    }

    /// Has [`CarPlay::keep_up`] try again at once, if it is waiting to.
    pub fn retry_now(&self) {
        self.retry.notify_one();
    }

    /// Whether the receiver runs.
    pub fn running(&self) -> bool {
        self.receiver().is_some()
    }

    /// Starts what does not run yet of the receiver and Bluetooth for iPhones; why CarPlay cannot run when either does
    /// not start.
    async fn try_start(
        &self,
        options: &Options,
        bluetooth_address: &str,
        shutdown: &watch::Receiver<bool>,
    ) -> Result<(), Unavailable> {
        let unavailable = |reason, detail: String| Unavailable {
            reason,
            detail: Some(detail),
        };
        let receiver = match self.receiver() {
            Some(receiver) => receiver.clone(),
            None => {
                let local = options
                    .carplay_address
                    .or_else(|| interface_address(&options.lan_iface))
                    .ok_or_else(|| {
                        unavailable(
                            "network",
                            format!("no IPv4 address on {}", options.lan_iface),
                        )
                    })?;
                let identity = carbridge_carplay::mfi::inspect(&options.mfi_dir);
                if let Some(problem) = identity.problem {
                    return Err(unavailable(
                        "mfi",
                        format!("MFi identity in {}: {problem}", options.mfi_dir.display()),
                    ));
                }
                let receiver = self
                    .start_receiver(options, local, bluetooth_address)
                    .map_err(|err| unavailable("receiver", format!("receiver: {err}")))?;
                let _ = self.address.set(local);
                let _ = self.receiver.set(receiver.clone());
                receiver
            }
        };
        if self.feed.handoff.lock().unwrap().is_some() {
            return Ok(());
        }
        let bluetooth = setup_bluetooth(options, &self.status)
            .await
            .map_err(|err| unavailable("bluetooth", format!("Bluetooth: {err}")))?;
        self.serve_bluetooth(
            options,
            bluetooth_address,
            receiver,
            bluetooth,
            shutdown.clone(),
        );
        Ok(())
    }

    /// Starts the receiver on `local`, with what CarPlay gives iPhones now.
    fn start_receiver(
        &self,
        options: &Options,
        local: IpAddr,
        bluetooth_address: &str,
    ) -> std::io::Result<Arc<Receiver>> {
        let choice = self.wanted.lock().unwrap().choice();
        let receiver = start_receiver(
            options,
            local,
            bluetooth_address,
            &choice,
            self.hevc.load(Ordering::Acquire),
            self.tile.lock().unwrap().clone(),
            self.feed.clone(),
            self.status.clone(),
        )?;
        let receiver = Arc::new(receiver);
        if let Some(night) = *self.night.lock().unwrap() {
            receiver.set_night_mode(Some(night));
        }
        self.feed
            .set_pictures(|pictures| pictures.next = Some(choice.picture));
        Ok(receiver)
    }

    /// Makes the router visible to iPhones over Bluetooth and starts wireless CarPlay with each iPhone that connects.
    fn serve_bluetooth(
        &self,
        options: &Options,
        bluetooth_address: &str,
        receiver: Arc<Receiver>,
        bluetooth: Bluetooth,
        shutdown: watch::Receiver<bool>,
    ) {
        let Some(&address) = self.address.get() else {
            return;
        };
        let accessory = Accessory {
            name: options.name.clone(),
            model: "CarBridge".into(),
            manufacturer: "CarBridge".into(),
            serial_number: bluetooth_address.replace(':', ""),
            firmware_version: env!("CARGO_PKG_VERSION").into(),
            hardware_version: "1.0".into(),
            bluetooth_address: bluetooth_address.to_owned(),
        };
        let (handoff_tx, handoff) = mpsc::unbounded_channel();
        *self.feed.handoff.lock().unwrap() = Some(handoff_tx.clone());
        // Once the session runs over Wi-Fi, the iPhone opens iAP2 again inside it and finishes setting up there.
        let tunnels = Arc::new(AtomicBool::new(false));
        {
            let receiver_ref = Arc::downgrade(&receiver);
            let (accessory, ap_iface, stop, handoff_tx) = (
                accessory.clone(),
                options.ap_iface.clone(),
                tunnels.clone(),
                handoff_tx.clone(),
            );
            receiver.set_iap_tunnel(move |stream| {
                let Some(receiver) = receiver_ref.upgrade() else {
                    return;
                };
                let network = match network(&ap_iface, address) {
                    Ok(network) => network,
                    Err(err) => {
                        log!("iAP2 over CarPlay: {err}");
                        return;
                    }
                };
                let (accessory, stop, handoff) =
                    (accessory.clone(), stop.clone(), handoff_tx.clone());
                std::thread::spawn(move || {
                    let log = |line: String| log!("{line}");
                    let ready = || {
                        let _ = handoff.send(Handoff::TunnelReady);
                    };
                    match wireless::run_tunnel(
                        stream, &accessory, &network, &receiver, &stop, &log, &ready,
                    ) {
                        Ok(result) => log!("iAP2 over CarPlay ended: {result}"),
                        Err(err) => log!("iAP2 over CarPlay: {err}"),
                    }
                    let _ = handoff.send(Handoff::TunnelEnded);
                });
            });
        }
        let serve = Serve {
            tunnels,
            steps: handoff_tx,
            handoff,
            ap_iface: options.ap_iface.clone(),
            address,
            pair_file: options.pair_file.clone(),
            pairing_window: options.pairing_window,
            forget_iphone_file: options.forget_iphone_file.clone(),
            status: self.status.clone(),
        };
        tokio::spawn(serve_bluetooth(
            bluetooth, receiver, accessory, serve, shutdown,
        ));
    }

    /// Whether the iPhone's screen is being received.
    pub fn active(&self) -> bool {
        *self.feed.active.borrow()
    }

    /// The shape of the picture the iPhone connected now renders, if one is.
    pub fn session_shape(&self) -> Option<Shape> {
        self.feed
            .pictures
            .lock()
            .unwrap()
            .session
            .as_ref()
            .map(picture_shape)
    }

    /// The picture the iPhone connected now renders, if one is.
    pub fn session_picture(&self) -> Option<Picture> {
        self.feed.pictures.lock().unwrap().session
    }

    /// The picture iPhones that connect render, `None` without the receiver.
    pub fn next_picture(&self) -> Option<Picture> {
        self.feed.pictures.lock().unwrap().next
    }

    /// Follows the picture CarPlay is shown at, which the head unit sizes its window to: the one the iPhone connected
    /// now renders, or else the one iPhones that connect render; `None` without the receiver.
    pub fn watch_picture(&self) -> watch::Receiver<Option<Picture>> {
        self.feed.shown.subscribe()
    }

    /// Follows what plays on the iPhone: the song known now, then each change, `None` while nothing is known.
    pub fn watch_now_playing(&self) -> watch::Receiver<Option<NowPlaying>> {
        self.feed.now_playing.subscribe()
    }

    /// Follows whether the iPhone's screen is being received, for a cast to take over from it when it stops.
    pub fn watch_active(&self) -> watch::Receiver<bool> {
        self.feed.active.subscribe()
    }

    /// Takes the frames from now on, for one cast.
    pub fn subscribe(&self) -> mpsc::Receiver<ScreenFrame> {
        let (tx, rx) = mpsc::channel(QUEUE);
        *self.feed.sender.lock().unwrap() = Some(tx);
        self.feed.lost.store(false, Ordering::Release);
        rx
    }

    pub fn unsubscribe(&self) {
        *self.feed.sender.lock().unwrap() = None;
    }

    /// Takes the iPhone's sound from now on, for one connection to a head unit; the starts of the streams and the
    /// microphone that go on already come first.
    pub fn subscribe_audio(&self) -> mpsc::Receiver<AudioEvent> {
        let (tx, rx) = mpsc::channel(AUDIO_QUEUE);
        let mut audio = self.feed.audio.lock().unwrap();
        for event in audio.replay.started() {
            let _ = tx.try_send(event.clone());
        }
        audio.sender = Some(tx);
        rx
    }

    pub fn unsubscribe_audio(&self) {
        self.feed.audio.lock().unwrap().sender = None;
    }

    /// Whether a frame was lost since the last call.
    pub fn take_lost(&self) -> bool {
        self.feed.lost.swap(false, Ordering::AcqRel)
    }

    /// Offers H.265 to the iPhones that connect from now on, or only H.264.
    pub fn set_hevc(&self, on: bool) {
        self.hevc.store(on, Ordering::Release);
        if let Some(receiver) = self.receiver() {
            receiver.set_hevc(on);
        }
    }

    /// Offers H.265 to the iPhones that connect from now on, or only H.264, and ends the session of an iPhone that
    /// was offered otherwise, to connect again with this offer.
    pub fn renew_hevc(&self, on: bool) {
        self.hevc.store(on, Ordering::Release);
        let Some(receiver) = self.receiver() else {
            return;
        };
        receiver.set_hevc(on);
        let codecs = if on { "H.265 and H.264" } else { "H.264 only" };
        if receiver.end_session_if(|given| given.hevc != on) {
            log!("CarPlay offers {codecs}; ending the iPhone's session for it to connect with that offer");
        } else {
            log!("CarPlay offers {codecs} to the iPhones that connect from now on");
        }
    }

    /// Follows the head unit `id`, connecting now, or none, `own`, what it goes by, and `views`, the views measured of
    /// it: the picture is chosen by its settings and views from the next [`CarPlay::follow_picture`] or
    /// [`CarPlay::follow_shape`] on, and the iPhones that connect from now on are offered H.265 by its mode, as while
    /// no cast runs.
    pub fn follow_head_unit(&self, id: Option<&str>, own: &Effective, views: &Views) {
        {
            let mut wanted = self.wanted.lock().unwrap();
            wanted.head_unit_id = id.map(str::to_owned);
            wanted.settings = own.picture;
            wanted.views = *views;
        }
        self.set_hevc(offer_hevc(own.hevc, None));
    }

    /// Whether `id` is the head unit CarPlay follows: the one connected now, or else the one connected last.
    pub fn follows(&self, id: &str) -> bool {
        self.wanted.lock().unwrap().head_unit_id.as_deref() == Some(id)
    }

    /// Takes `own`, the new settings of the head unit CarPlay follows, for its picture, and with `Some(window)` the
    /// shape of its window as well. As with [`CarPlay::follow_picture`], an iPhone that renders another picture
    /// connects again. Its H.265 mode goes to iPhones through [`CarPlay::renew_hevc`].
    pub fn follow_settings(&self, own: &Effective, window: Option<Option<Shape>>) {
        self.follow(|wanted| {
            wanted.settings = own.picture;
            if let Some(window) = window {
                wanted.window = window;
            }
        });
    }

    /// Takes `views`, the views measured of the head unit CarPlay follows, for the picture's density, and with
    /// `Some(window)` the shape of its window as well. With `renew`, as with [`CarPlay::follow_picture`], an iPhone
    /// that renders another picture connects again; without, it keeps its picture until it connects again.
    pub fn follow_views(&self, views: &Views, window: Option<Option<Shape>>, renew: bool) {
        self.follow_with(
            |wanted| {
                wanted.views = *views;
                if let Some(window) = window {
                    wanted.window = window;
                }
            },
            renew,
        );
    }

    /// Passes the head unit's day or night mode, night when true, to CarPlay: to the iPhone's session at once, and
    /// to each session that starts while the head unit stays connected. `None` once the head unit is gone.
    pub fn set_night_mode(&self, night: Option<bool>) {
        *self.night.lock().unwrap() = night;
        if let Some(receiver) = self.receiver() {
            receiver.set_night_mode(night);
        }
    }

    /// Names the car tile on the CarPlay home screen after the head unit, from what its certificate says, and gives
    /// it the maker's icon if it has one in place. The tile is kept for the iPhones that connect from now on, also
    /// after the head unit's connection ends, until another head unit names it. An iPhone connected or connecting
    /// with a tile that looks otherwise has its session ended, to connect again and read this one.
    pub fn name_car_tile(&self, head_unit: &HeadUnitIdentity) {
        let choice = Choice::for_head_unit(
            head_unit.short_name.as_deref(),
            head_unit.product_name.as_deref(),
            head_unit.vendor_id.as_deref(),
            &self.oem_icon_dir,
        );
        *self.tile.lock().unwrap() = choice.tile.clone();
        let Some(receiver) = self.receiver() else {
            log!("CarPlay: {choice}, once it runs");
            return;
        };
        receiver.set_car_tile(choice.tile.clone());
        let mut stale = false;
        let ended = receiver.end_session_if(|shown| {
            stale = !shown.tile.looks_like(&choice.tile);
            stale && RENEW_CAR_TILE
        });
        if ended {
            log!("CarPlay: {choice}; ending the iPhone's session for it to read the new tile");
        } else if stale {
            log!("CarPlay: {choice}; the iPhone connected now keeps its tile until it connects again");
        } else {
            log!("CarPlay: {choice}");
        }
    }

    /// Gives the iPhones that connect from now on the car tile of no head unit.
    pub fn reset_car_tile(&self) {
        *self.tile.lock().unwrap() = CarTile::default();
        if let Some(receiver) = self.receiver() {
            if receiver.set_car_tile(CarTile::default()) {
                log!(
                    "CarPlay: car tile \"{}\" for the iPhones that connect from now on",
                    CarTile::default().label
                );
            }
        }
    }

    /// Gives the iPhones that connect from now on the picture the settings choose for `head_unit`'s picture, or for no
    /// head unit, turned to the shape [`CarPlay::follow_shape`] gave. It is kept also after the head unit's connection
    /// ends, until another head unit asks for one. An iPhone connected or connecting with another picture has its
    /// session ended, to connect again and render this one.
    pub fn follow_picture(&self, head_unit: Option<(&HeadUnitPicture, Source)>) {
        self.follow(|wanted| wanted.head_unit = head_unit.map(|(unit, source)| (*unit, source)));
    }

    /// Turns the picture to `window`, the shape of the head unit's window while it shows the cast there, or leaves it
    /// as the head unit's screen is with `None`, in full screen, and gives it the density set for that mode. As with
    /// [`CarPlay::follow_picture`], an iPhone that renders a picture of another shape or density connects again; one
    /// whose picture stays, as between full screen and the landscape window at the same density, goes on.
    pub fn follow_shape(&self, window: Option<Shape>) {
        self.follow(|wanted| wanted.window = window);
    }

    fn follow(&self, change: impl FnOnce(&mut Wanted)) {
        self.follow_with(change, true);
    }

    /// Changes what the picture follows; with `renew`, an iPhone connected or connecting with another picture has its
    /// session ended.
    fn follow_with(&self, change: impl FnOnce(&mut Wanted), renew: bool) {
        let choice = {
            let mut wanted = self.wanted.lock().unwrap();
            change(&mut wanted);
            wanted.choice()
        };
        // A receiver that starts later takes the picture wanted then.
        let Some(receiver) = self.receiver() else {
            return;
        };
        let changed = receiver.set_picture(choice.picture, &choice.reason);
        self.feed
            .set_pictures(|pictures| pictures.next = Some(choice.picture));
        let mut stale = false;
        let ended = if renew {
            receiver.end_session_if(|given| {
                stale = given.picture != choice.picture;
                stale && RENEW_PICTURE
            })
        } else {
            stale = receiver
                .session_picture()
                .is_some_and(|given| given != choice.picture);
            false
        };
        if ended {
            log!("CarPlay picture {choice}; ending the iPhone's session for it to render it");
        } else if stale {
            log!("CarPlay picture {choice}; the iPhone connected now keeps its picture until it connects again");
        } else if changed {
            log!("CarPlay picture {choice} for the iPhones that connect from now on");
        } else {
            log!("CarPlay picture {choice}, as before");
        }
    }

    /// The codec the iPhone's session of the moment is cast in, if one is connected.
    pub fn session_codec(&self) -> Option<Codec> {
        self.receiver()?.session_codec().map(cast_codec_of)
    }

    /// Passes a touch from the head unit to the iPhone: an Android motion event action and every pointer on the
    /// screen as (id, x, y) in picture pixels.
    pub fn touch(&self, action: i64, pointers: &[(i64, i64, i64)]) {
        let Some(receiver) = self.receiver() else {
            return;
        };
        // While the iPhone is to draw for the head unit's new window, the head unit shows nothing of it.
        if self.feed.reshaping() {
            return;
        }
        let mut touch = self.touch.lock().unwrap();
        // The head unit gives the touches in the pixels of the picture the iPhone renders.
        if let Some(picture) = receiver.session_picture() {
            if touch.size() != (picture.width, picture.height) {
                touch.set_size(picture.width, picture.height);
            }
        }
        let Some(contacts) = touch.event(action, pointers) else {
            return;
        };
        drop(touch);
        if let Err(err) = receiver.touch(&contacts) {
            log!("CarPlay touch: {err}");
        }
    }

    /// Passes a key from the head unit to the iPhone: an Android or Carlink key code, pressed when `action` is
    /// down (0) or press (2). Each key is logged with the CarPlay key it became: in the detailed log while it is on,
    /// else in the log at most once every [`KEYS_EVERY`], with how many came since.
    pub fn key(&self, action: i64, keycode: i64) {
        let key = Key::from_keycode(keycode);
        let pressed = matches!(action, 0 | 2);
        let line = describe_key(action, keycode, key, pressed, self.receiver().is_some());
        if crate::logfile::detailed() {
            crate::logfile::detail(&line);
        } else {
            let held = self.keys_logged.lock().unwrap().pass(Instant::now());
            match held {
                Some(0) => log!("{line}"),
                Some(held) => log!("{line} ({held} more keys held back)"),
                None => {}
            }
        }
        let Some(receiver) = self.receiver() else {
            return;
        };
        let Some(key) = key.filter(|_| pressed) else {
            return;
        };
        if let Err(err) = receiver.key(key) {
            log!("CarPlay key {key:?}: {err}");
        }
    }

    /// Gives the iPhone one frame of the car's microphone, and says what became of it. A failure is logged at most
    /// once every [`MICROPHONE_ERRORS_EVERY`], with how many came since the last logged.
    pub fn microphone(&self, stream_type: u16, frame: &[u8]) -> MicOutcome {
        let Some(receiver) = self.receiver() else {
            return MicOutcome::NoStream;
        };
        let err = match receiver.microphone(stream_type, frame) {
            Ok(MicrophoneSent::Opus) => return MicOutcome::Opus,
            Ok(MicrophoneSent::Pcm) => return MicOutcome::Pcm,
            Ok(MicrophoneSent::NoStream) => return MicOutcome::NoStream,
            Err(err) => err,
        };
        let held = self.microphone_errors.lock().unwrap().pass(Instant::now());
        match held {
            Some(0) => log!("CarPlay microphone: {err}"),
            Some(held) => log!("CarPlay microphone: {err} ({held} more failures held back)"),
            None => {}
        }
        match err {
            MicrophoneError::Encode => MicOutcome::EncodeFailed,
            MicrophoneError::Send(_) => MicOutcome::SendFailed,
        }
    }

    pub fn request_key_frame(&self) {
        if let Some(receiver) = self.receiver() {
            receiver.request_key_frame();
        }
    }

    pub fn stop(&self) {
        if let Some(receiver) = self.receiver() {
            receiver.stop();
        }
    }
}

struct Bluetooth {
    classic: Classic,
    /// iAP2 connections an iPhone opened to us.
    incoming: mpsc::Receiver<ProfileConnection>,
    /// iAP2 connections we opened to an iPhone.
    outgoing: mpsc::Receiver<ProfileConnection>,
    /// Connections an iPhone opened to the CarPlay service.
    carplay: mpsc::Receiver<ProfileConnection>,
    agent: mpsc::Receiver<AgentEvent>,
    devices: DeviceChanges,
}

/// Sets Bluetooth up for iPhones: the name, the agent, the iAP2 and CarPlay profiles and pairing. When a step fails,
/// what the earlier steps registered with bluetoothd is withdrawn again, for the next try to register it afresh.
async fn setup_bluetooth(options: &Options, status: &StatusFile) -> zbus::Result<Bluetooth> {
    let classic = Classic::connect(&options.adapter).await?;
    let parts = register_bluetooth(&classic, options, status).await;
    match parts {
        Ok((agent, devices, incoming, outgoing, carplay)) => Ok(Bluetooth {
            classic,
            incoming,
            outgoing,
            carplay,
            agent,
            devices,
        }),
        Err(err) => {
            classic.close().await;
            Err(err)
        }
    }
}

async fn register_bluetooth(
    classic: &Classic,
    options: &Options,
    status: &StatusFile,
) -> zbus::Result<(
    mpsc::Receiver<AgentEvent>,
    DeviceChanges,
    mpsc::Receiver<ProfileConnection>,
    mpsc::Receiver<ProfileConnection>,
    mpsc::Receiver<ProfileConnection>,
)> {
    classic.set_name(&options.name).await?;
    let agent = classic.register_agent().await?;
    let devices = classic.device_changes().await?;
    let incoming = classic
        .register_profile(
            IAP2_ACCESSORY_UUID,
            "Wireless iAP",
            Role::Server(IAP2_CHANNEL),
        )
        .await?;
    let outgoing = classic
        .register_profile(IAP2_DEVICE_UUID, "Wireless iAP", Role::Client)
        .await?;
    let carplay = classic
        .register_profile(CARPLAY_UUID, "CarPlay", Role::Server(CARPLAY_CHANNEL))
        .await?;
    if options.pairing_window > 0 {
        classic.set_pairing(true, options.pairing_window).await?;
        status.update(|s| s.carplay.pairing_until = unix_now() + u64::from(options.pairing_window));
        log!(
            "iPhones can pair with \"{}\" for {} s",
            options.name,
            options.pairing_window
        );
    } else {
        // Hidden and not pairable until asked: a head unit that finds us over Bluetooth tries to pair for calls.
        classic.set_pairing(false, 0).await?;
    }
    // CarPlay lists only Bluetooth devices that say they are car audio.
    if let Err(err) = mgmt::set_device_class(
        &options.adapter,
        CLASS_MAJOR_AUDIO_VIDEO,
        CLASS_MINOR_CAR_AUDIO,
    ) {
        log!("setting the Bluetooth class of device: {err}");
    }
    // Car audio that offers audio; without the service class iOS takes the accessory for less.
    if let Err(err) = mgmt::add_uuid(&options.adapter, CARPLAY_UUID, SERVICE_AUDIO) {
        log!("setting the Bluetooth audio service class: {err}");
    }
    match classic.class().await {
        Ok(class) => log!("Bluetooth class of device {class:#08x}"),
        Err(err) => log!("Bluetooth class of device unknown: {err}"),
    }
    Ok((agent, devices, incoming, outgoing, carplay))
}

/// Opens the iAP2 service of the iPhone at `address` over RFCOMM, on the channel found for it before or found now
/// over SDP. A channel that no longer answers is looked up again.
fn open_directly(address: &str, channels: &Mutex<HashMap<String, u8>>) -> std::io::Result<OwnedFd> {
    let cached = channels.lock().unwrap().get(address).copied();
    if let Some(channel) = cached {
        match rfcomm::connect(address, channel, DIRECT_TIMEOUT) {
            Ok(socket) => return Ok(socket),
            Err(err) => {
                log!("iAP2 on RFCOMM channel {channel} of {address}: {err}; looking it up again");
                channels.lock().unwrap().remove(address);
            }
        }
    }
    let channel = rfcomm::find_channel(address, IAP2_DEVICE_UUID, DIRECT_TIMEOUT)?;
    if cached != Some(channel) {
        log!("iAP2 of {address} on RFCOMM channel {channel}");
    }
    channels.lock().unwrap().insert(address.to_owned(), channel);
    rfcomm::connect(address, channel, DIRECT_TIMEOUT)
}

/// When to connect to an iPhone's iAP2 service next.
struct Opening {
    next: Instant,
    wait: Duration,
    /// Newly paired: called as soon as bluetoothd learns its iAP2 service.
    paired: bool,
}

impl Opening {
    fn new() -> Opening {
        Opening {
            next: Instant::now(),
            wait: IAP2_RETRY,
            paired: false,
        }
    }

    /// Once bluetoothd has learned the iPhone's services, which it looks up after the pairing.
    fn after_pairing() -> Opening {
        Opening {
            next: Instant::now() + IAP2_AFTER_PAIRING,
            wait: IAP2_RETRY,
            paired: true,
        }
    }

    /// bluetoothd is busy with the iPhone: soon again, with no longer wait after.
    fn busy(&mut self) {
        self.next = Instant::now() + IAP2_BUSY_RETRY;
    }

    fn failed(&mut self) {
        self.paired = false;
        self.next = Instant::now() + self.wait;
        self.wait = (self.wait * 2).min(IAP2_RETRY_LONGEST);
    }
}

/// What serving the iPhones' Bluetooth connections needs.
struct Serve {
    /// Stops the iAP2 runs over CarPlay.
    tunnels: Arc<AtomicBool>,
    /// Where the iAP2 links over Bluetooth tell their steps.
    steps: mpsc::UnboundedSender<Handoff>,
    handoff: mpsc::UnboundedReceiver<Handoff>,
    /// The access point the iPhone joins, and the receiver's address on it.
    ap_iface: String,
    address: IpAddr,
    /// A file whose appearance opens pairing for `pairing_window` seconds.
    pair_file: PathBuf,
    pairing_window: u32,
    /// A file whose appearance unpairs the iPhones it names, one Bluetooth address a line.
    forget_iphone_file: PathBuf,
    status: Arc<StatusFile>,
}

async fn serve_bluetooth(
    mut bluetooth: Bluetooth,
    receiver: Arc<Receiver>,
    accessory: Accessory,
    serve: Serve,
    mut shutdown: watch::Receiver<bool>,
) {
    let Serve {
        tunnels,
        steps,
        mut handoff,
        ap_iface,
        address,
        pair_file,
        pairing_window,
        forget_iphone_file,
        status,
    } = serve;
    // iPhones with an iAP2 link, each with what stops it, and when we last tried to open one to each, by device path.
    let linked: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let mut attempts: HashMap<String, Opening> = HashMap::new();
    // iAP2 connections opened without bluetoothd, the iPhones they are being opened to, and the RFCOMM channel of
    // each iPhone's iAP2 service, by address.
    let (direct_tx, mut direct) = mpsc::channel::<ProfileConnection>(4);
    let opening: Arc<Mutex<HashSet<String>>> = Arc::default();
    let channels: Arc<Mutex<HashMap<String, u8>>> = Arc::default();
    // Since when each iPhone holds Bluetooth with no iAP2 link and no session, by device path.
    let mut idle: HashMap<String, Instant> = HashMap::new();
    // The iPhone that asked to give up Bluetooth, and until when we wait for iAP2 inside its session.
    let mut handoff_due: Option<(String, Instant)> = None;
    // The iPhone kept on Bluetooth because iAP2 inside its session was late; let go once it comes.
    let mut handoff_kept: Option<String> = None;
    let mut tunnel_ready = false;
    // The iAP2 links over Bluetooth, followed for a CarPlay session that does not come, by device path.
    let mut watches: HashMap<String, SessionWatch> = HashMap::new();
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = poll.tick() => {
                if let Some((address, _)) = handoff_due.take_if(|(_, due)| Instant::now() >= *due) {
                    let seconds = HANDOFF_TIMEOUT.as_secs();
                    if status.get().carplay.screen {
                        // CarPlay shows already: iAP2 stays on Bluetooth until it comes up inside the session.
                        log!("no iAP2 inside the CarPlay session {seconds} s after the iPhone asked; it stays on Bluetooth");
                        handoff_kept = Some(address);
                    } else {
                        log!("no iAP2 inside the CarPlay session {seconds} s after the iPhone asked");
                        let_go(&bluetooth.classic, &linked, &address).await;
                    }
                }
                if status.get().carplay.iphone.is_none() {
                    handoff_kept = None;
                }
                let mut paired = bluetooth.classic.paired().await.unwrap_or_default();
                if let Ok(text) = std::fs::read_to_string(&forget_iphone_file) {
                    let _ = std::fs::remove_file(&forget_iphone_file);
                    for address in text.lines().map(str::trim).filter(|a| !a.is_empty()) {
                        let Some(device) = paired.iter().find(|d| d.address.eq_ignore_ascii_case(address)) else {
                            log!("no iPhone {address} to unpair");
                            continue;
                        };
                        let path = device.path.clone();
                        if let Some(stop) = linked.lock().unwrap().get(&path) {
                            stop.store(true, Ordering::Release);
                        }
                        attempts.remove(&path);
                        idle.remove(&path);
                        watches.remove(&path);
                        match bluetooth.classic.remove_device(&path).await {
                            Ok(()) => log!("iPhone {address} unpaired; it pairs again from its Bluetooth settings"),
                            Err(err) => log!("unpairing iPhone {address}: {err}"),
                        }
                        paired.retain(|d| d.path != path);
                    }
                }
                let mut iphones: Vec<PairedIphone> = paired
                    .iter()
                    .map(|d| PairedIphone {
                        address: d.address.clone(),
                        name: d.name.clone(),
                        connected: d.connected,
                    })
                    .collect();
                iphones.sort_by(|a, b| a.address.cmp(&b.address));
                status.update(|s| s.carplay.iphones = iphones);
                watches.retain(|path, _| linked.lock().unwrap().contains_key(path));
                let (now, running) = (Instant::now(), status.get().carplay.iphone.is_some());
                let stalled: Vec<(String, Stall)> = watches
                    .iter()
                    .filter_map(|(path, watch)| Some((path.clone(), watch.stalled(now, running)?)))
                    .collect();
                for (path, stall) in stalled {
                    watches.remove(&path);
                    attempts.remove(&path);
                    let address = paired.iter().find(|d| d.path == path).map_or(path.as_str(), |d| d.address.as_str());
                    restart_link(&bluetooth.classic, &linked, &path, address, stall).await;
                }
                for device in paired.iter().filter(|device| !device.trusted) {
                    // Just Works pairings are settled without the agent. Trusted, the iPhone's connections to our
                    // services need no authorisation; it is called shortly.
                    match bluetooth.classic.trust(&device.path).await {
                        Ok(()) => log!("{} paired", device.address),
                        Err(err) => log!("trusting {}: {err}", device.address),
                    }
                    attempts.insert(device.path.clone(), Opening::after_pairing());
                }
                // The accessory calls the paired iPhones, nearby or not, and opens iAP2; none while a session runs.
                let calling = status.get().carplay.iphone.is_none();
                idle.retain(|path, _| paired.iter().any(|d| &d.path == path && d.connected));
                for device in paired.iter().filter(|_| calling) {
                    // A link the iPhone holds with nothing on it: disconnected, it is called afresh.
                    if !device.connected
                        || linked.lock().unwrap().contains_key(&device.path)
                        || opening.lock().unwrap().contains(&device.address)
                    {
                        idle.remove(&device.path);
                        continue;
                    }
                    let since = *idle.entry(device.path.clone()).or_insert_with(Instant::now);
                    if since.elapsed() >= IDLE_LINK_TIMEOUT {
                        log!("{} held Bluetooth {} s without iAP2; disconnecting it", device.address, IDLE_LINK_TIMEOUT.as_secs());
                        idle.remove(&device.path);
                        attempts.remove(&device.path);
                        if let Err(err) = bluetooth.classic.disconnect(&device.path).await {
                            log!("disconnecting {}: {err}", device.address);
                        }
                    }
                }
                for device in paired.into_iter().filter(|_| calling) {
                    if linked.lock().unwrap().contains_key(&device.path)
                        || attempts.get(&device.path).is_some_and(|o| Instant::now() < o.next)
                    {
                        continue;
                    }
                    if !device.offers(IAP2_DEVICE_UUID) {
                        // bluetoothd does not know the iPhone's services, as when its discovery after the pairing was
                        // cut short: the iAP2 service is looked up and opened directly.
                        attempts.entry(device.path.clone()).or_insert_with(Opening::new).failed();
                        opening.lock().unwrap().insert(device.address.clone());
                        let (direct, opening, channels) = (direct_tx.clone(), opening.clone(), channels.clone());
                        tokio::task::spawn_blocking(move || {
                            match open_directly(&device.address, &channels) {
                                Ok(socket) => {
                                    let _ = direct.blocking_send(ProfileConnection { device: device.path, socket });
                                }
                                Err(err) => log!("opening iAP2 to {} directly: {err}", device.address),
                            }
                            opening.lock().unwrap().remove(&device.address);
                        });
                        continue;
                    }
                    let device = device.path;
                    let Err(err) = bluetooth.classic.connect_profile(&device, IAP2_DEVICE_UUID).await else {
                        continue;
                    };
                    log!("opening iAP2 to {device}: {err}");
                    if err.to_string().contains("key-missing") {
                        // The iPhone forgot the pairing; ours goes too, for it to pair afresh.
                        attempts.remove(&device);
                        match bluetooth.classic.remove_device(&device).await {
                            Ok(()) => log!("{device} forgot the pairing; forgotten here too"),
                            Err(err) => log!("forgetting {device}: {err}"),
                        }
                        continue;
                    }
                    let opening = attempts.entry(device).or_insert_with(Opening::new);
                    if err.to_string().contains("busy") || err.to_string().contains("InProgress") {
                        opening.busy();
                    } else {
                        opening.failed();
                    }
                }
                if status.get().carplay.pairing_until != 0 && status.get().carplay.pairing_until <= unix_now() {
                    status.update(|s| s.carplay.pairing_until = 0);
                }
                if std::fs::remove_file(&pair_file).is_ok() {
                    // A request with no window of its own opens pairing for five minutes.
                    let seconds = if pairing_window == 0 { 300 } else { pairing_window };
                    let mut opened = bluetooth.classic.set_pairing(true, seconds).await;
                    if let Err(err) = &opened {
                        log!("opening pairing: {err}; trying again in {} s", PAIRING_RETRY.as_secs());
                        tokio::time::sleep(PAIRING_RETRY).await;
                        opened = bluetooth.classic.set_pairing(true, seconds).await;
                    }
                    match opened {
                        Ok(()) => {
                            status.update(|s| {
                                s.carplay.pairing_until = unix_now() + u64::from(seconds);
                                s.carplay.pairing_error = None;
                            });
                            log!("iPhones can pair for {seconds} s");
                        }
                        Err(err) => {
                            log!("opening pairing: {err}");
                            status.update(|s| {
                                let attempt = s.carplay.pairing_error.as_ref().map_or(0, |e| e.attempt) + 1;
                                s.carplay.pairing_error = Some(PairingError { attempt, detail: err.to_string() });
                            });
                        }
                    }
                }
            }
            Some(connection) = async {
                tokio::select! {
                    c = bluetooth.incoming.recv() => c,
                    c = bluetooth.outgoing.recv() => c,
                    c = bluetooth.carplay.recv() => c,
                    c = direct.recv() => c,
                }
            } => {
                let path = connection.device.clone();
                attempts.remove(&path);
                let stop = Arc::new(AtomicBool::new(false));
                if linked.lock().unwrap().insert(path.clone(), stop.clone()).is_some() {
                    log!("{path} has an iAP2 link already");
                    continue;
                }
                let (device, name) = bluetooth.classic.device(&connection.device).await.unwrap_or_default();
                log!("iPhone {name} ({device}) connected over Bluetooth");
                let network = match network(&ap_iface, address) {
                    Ok(network) => network,
                    Err(err) => {
                        log!("{err}");
                        linked.lock().unwrap().remove(&path);
                        continue;
                    }
                };
                watches.insert(path.clone(), SessionWatch::default());
                let (receiver, accessory, steps) = (receiver.clone(), accessory.clone(), steps.clone());
                let linked = linked.clone();
                tokio::task::spawn_blocking(move || {
                    let log = |line: String| log!("{line}");
                    let step = |step| {
                        let _ = steps.send(Handoff::Link(path.clone(), step));
                    };
                    match wireless::run(connection.socket, &accessory, &network, &receiver, &stop, &log, &step) {
                        Ok(result) => log!("iAP2 link ended: {result}"),
                        Err(err) => log!("iAP2: {err}"),
                    }
                    linked.lock().unwrap().remove(&path);
                });
            }
            Some(step) = handoff.recv() => match step {
                Handoff::Requested(address) if tunnel_ready => {
                    let_go(&bluetooth.classic, &linked, &address).await;
                }
                Handoff::Requested(address) => {
                    log!("waiting for iAP2 inside the CarPlay session before letting go of Bluetooth");
                    handoff_due = Some((address, Instant::now() + HANDOFF_TIMEOUT));
                }
                Handoff::TunnelReady => {
                    tunnel_ready = true;
                    let waiting = handoff_due.take().map(|(address, _)| address).or(handoff_kept.take());
                    if let Some(address) = waiting {
                        let_go(&bluetooth.classic, &linked, &address).await;
                    }
                }
                Handoff::TunnelEnded => tunnel_ready = false,
                Handoff::Link(path, step) => {
                    if let Some(watch) = watches.get_mut(&path) {
                        watch.step(step, Instant::now());
                    }
                }
                Handoff::SessionStarted => watches.values_mut().for_each(SessionWatch::session_started),
                Handoff::SessionEnded => watches.values_mut().for_each(|w| w.session_ended(Instant::now())),
                Handoff::AuthenticationFailed => {
                    watches.values_mut().for_each(|w| w.authentication_failed(Instant::now()));
                }
            },
            Some(change) = bluetooth.devices.next() => match change {
                DeviceChange::Paired(path) => {
                    // Trusted at once, before the iPhone opens its first connections to us, which then need no
                    // authorisation.
                    match bluetooth.classic.trust(&path).await {
                        Ok(()) => log!("{path} paired"),
                        Err(err) => log!("trusting {path}: {err}"),
                    }
                    attempts.insert(path, Opening::after_pairing());
                }
                DeviceChange::Services(path, uuids) => {
                    let iap2 = uuids.iter().any(|u| u.eq_ignore_ascii_case(IAP2_DEVICE_UUID));
                    if let Some(opening) = attempts.get_mut(&path).filter(|o| o.paired && iap2) {
                        opening.paired = false;
                        opening.next = Instant::now();
                        poll.reset_immediately();
                    }
                }
            },
            Some(event) = bluetooth.agent.recv() => match event {
                AgentEvent::Pairing { device, accepted } => {
                    log!("pairing request from {device}: {}", if accepted { "accepted" } else { "refused, pairing is closed" });
                    if accepted {
                        attempts.insert(device, Opening::after_pairing());
                    }
                }
            },
            // The guard wait_for returns is not Send; it must not live in the select.
            _ = async { let _ = shutdown.wait_for(|stop| *stop).await; } => break,
        }
    }
    for stop in linked.lock().unwrap().values() {
        stop.store(true, Ordering::Release);
    }
    tunnels.store(true, Ordering::Release);
    bluetooth.classic.close().await;
}

/// Ends iAP2 over Bluetooth to an iPhone whose session runs over Wi-Fi, and the Bluetooth connection with it.
async fn let_go(
    classic: &Classic,
    linked: &Mutex<HashMap<String, Arc<AtomicBool>>>,
    address: &str,
) {
    let device = classic.device_path(address);
    if let Some(stop) = linked.lock().unwrap().get(&device) {
        stop.store(true, Ordering::Release);
    }
    match classic.disconnect(&device).await {
        Ok(()) => log!("CarPlay runs over Wi-Fi; Bluetooth to {address} let go"),
        Err(err) => log!("letting go of Bluetooth to {address}: {err}"),
    }
}

/// Ends the iAP2 link over Bluetooth to an iPhone whose CarPlay session did not come, and the Bluetooth connection
/// with it, for the iPhone to be called afresh and start over from the Wi-Fi configuration.
async fn restart_link(
    classic: &Classic,
    linked: &Mutex<HashMap<String, Arc<AtomicBool>>>,
    device: &str,
    address: &str,
    stall: Stall,
) {
    let why = match stall {
        Stall::AuthenticationFailed => format!(
            "AirPlay authentication failed and the iPhone did not try again within {} s",
            wireless::AUTHENTICATION_GRACE.as_secs()
        ),
        Stall::NoSession => format!(
            "no CarPlay session {} s after the iPhone was asked to start one",
            wireless::SESSION_TIMEOUT.as_secs()
        ),
    };
    log!("{address}: {why}; ending its iAP2 link and Bluetooth connection to call it afresh");
    if let Some(stop) = linked.lock().unwrap().get(device) {
        stop.store(true, Ordering::Release);
    }
    if let Err(err) = classic.disconnect(device).await {
        log!("disconnecting {address}: {err}");
    }
}

/// The access point the iPhone joins, as iAP2 hands it over, with the receiver's address on it.
fn network(ap_iface: &str, address: IpAddr) -> Result<Network, String> {
    let ap =
        access_point::read(ap_iface).map_err(|err| format!("access point {ap_iface}: {err}"))?;
    Ok(Network {
        security: wireless::security(&ap.encryption),
        ssid: ap.ssid,
        passphrase: ap.passphrase,
        channel: ap.channel,
        bssid: Some(ap.bssid),
        address,
    })
}

/// A line for the log of a key from the head unit: its code and action, and the CarPlay key it is, if it is one that
/// goes to the iPhone.
fn describe_key(
    action: i64,
    keycode: i64,
    key: Option<Key>,
    pressed: bool,
    running: bool,
) -> String {
    let became = match key {
        None => "no CarPlay key".to_owned(),
        Some(key) if !running => format!("CarPlay key {key:?}, not sent: CarPlay does not run"),
        Some(key) if pressed => format!("CarPlay key {key:?}"),
        Some(key) => format!("CarPlay key {key:?} released, nothing sent"),
    };
    format!("key from the head unit: code {keycode}, action {action}: {became}")
}

#[allow(clippy::too_many_arguments)]
fn start_receiver(
    options: &Options,
    local: IpAddr,
    bluetooth_address: &str,
    picture: &picture::Choice,
    hevc: bool,
    tile: CarTile,
    feed: Arc<Feed>,
    status: Arc<StatusFile>,
) -> std::io::Result<Receiver> {
    let config = ReceiverConfig {
        local,
        port: options.carplay_port,
        name: options.name.clone(),
        device_id: bluetooth_address.to_owned(),
        bluetooth_address: bluetooth_address.to_owned(),
        picture: picture.picture,
        picture_reason: picture.reason.clone(),
        hevc,
        tile,
        state_dir: options.carplay_state_dir.clone(),
        mfi_key: options.mfi_dir.join("identity.pk8"),
        mfi_certificate: options.mfi_dir.join("certificate.p7b"),
    };
    let started_status = status.clone();
    let events = move |event: ReceiverEvent| match event {
        ReceiverEvent::Video(frame) => {
            if let Some(sender) = feed.sender.lock().unwrap().as_ref() {
                let picture = feed.pictures.lock().unwrap().session;
                let frame = ScreenFrame {
                    video: frame,
                    shape: picture.as_ref().map(picture_shape),
                    picture,
                };
                if sender.try_send(frame).is_err() {
                    feed.lost.store(true, Ordering::Release);
                }
            }
        }
        ReceiverEvent::ScreenActive(active) => {
            feed.active.send_replace(active);
            status.update(|s| s.carplay.screen = active);
            log!(
                "CarPlay screen {}",
                if active { "started" } else { "stopped" }
            );
        }
        ReceiverEvent::SessionActive {
            peer,
            codec,
            picture,
        } => {
            log!("CarPlay session with {peer:?} in {}", cast_codec_of(codec));
            feed.set_pictures(|pictures| pictures.session = Some(picture));
            status.update(|s| s.carplay.iphone = peer.map(|p| p.to_string()));
            feed.step(Handoff::SessionStarted);
        }
        ReceiverEvent::SessionEnded => {
            feed.active.send_replace(false);
            feed.set_pictures(|pictures| pictures.session = None);
            status.update(|s| {
                s.carplay.iphone = None;
                s.carplay.screen = false;
            });
            log!("CarPlay session ended");
            feed.step(Handoff::SessionEnded);
        }
        ReceiverEvent::AuthenticationFailed { peer } => {
            match peer {
                Some(peer) => log!("CarPlay: iPhone {peer} authentication failed"),
                None => log!("CarPlay: iPhone authentication failed"),
            }
            feed.step(Handoff::AuthenticationFailed);
        }
        ReceiverEvent::Audio(event) => {
            let mut audio = feed.audio.lock().unwrap();
            audio.replay.note(&event);
            if let Some(sender) = audio.sender.as_ref() {
                // A lagging connection loses sound rather than holding the receiver up.
                let _ = sender.try_send(event);
            }
        }
        ReceiverEvent::BluetoothHandoff { device_id } => feed.step(Handoff::Requested(device_id)),
        ReceiverEvent::NightMode(night) => log!(
            "CarPlay night mode {}, from the head unit",
            if night { "on" } else { "off" }
        ),
        ReceiverEvent::NowPlaying(now) => {
            let before = feed.now_playing.send_replace(now.clone());
            if let Some(line) = describe_now_playing(before.as_ref(), now.as_ref()) {
                log!("{line}");
            }
        }
        ReceiverEvent::Log(line) => log!("CarPlay: {line}"),
    };
    let receiver = Receiver::start(config, events)?;
    started_status.update(|s| s.carplay.receiver = true);
    log!("CarPlay receiver on {local}:{}", options.carplay_port);
    Ok(receiver)
}

/// A line for the log when what plays on the iPhone changed other than in its position: another song, its
/// artwork, or whether it plays.
fn describe_now_playing(before: Option<&NowPlaying>, now: Option<&NowPlaying>) -> Option<String> {
    let Some(now) = now else {
        return before.map(|_| "CarPlay now playing: nothing".into());
    };
    let song = |n: &NowPlaying| {
        (
            n.title.clone(),
            n.artist.clone(),
            n.album.clone(),
            n.duration_ms,
        )
    };
    let same_song = before.is_some_and(|before| song(before) == song(now));
    if same_song && before.is_some_and(|b| b.playing == now.playing && b.artwork == now.artwork) {
        return None;
    }
    let artwork = now.artwork.as_ref().map_or_else(
        || "no artwork".to_owned(),
        |a| format!("artwork of {} bytes", a.len()),
    );
    Some(format!(
        "CarPlay now playing: {}, {}, {artwork}",
        if same_song {
            "the same song"
        } else {
            "another song"
        },
        if now.playing {
            "playing"
        } else {
            "not playing"
        }
    ))
}

/// The cast's name for a codec of the iPhone's screen.
pub fn cast_codec_of(codec: VideoCodec) -> Codec {
    match codec {
        VideoCodec::H264 => Codec::H264,
        VideoCodec::H265 => Codec::H265,
    }
}

/// The first IPv4 address of `iface`.
fn interface_address(iface: &str) -> Option<IpAddr> {
    let output = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", iface])
        .output()
        .ok()?;
    parse_ip_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_ip_output(text: &str) -> Option<IpAddr> {
    let mut words = text.split_whitespace();
    words.find(|word| *word == "inet")?;
    words.next()?.split('/').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_from_ip_output() {
        let text = "5: br-lan    inet 192.168.77.1/24 brd 192.168.77.255 scope global br-lan\\       valid_lft forever\n";
        assert_eq!(parse_ip_output(text), Some(IpAddr::from([192, 168, 77, 1])));
        assert_eq!(parse_ip_output(""), None);
    }

    #[test]
    fn keys_are_told_with_the_carplay_key_they_became() {
        assert_eq!(
            describe_key(0, 4, Key::from_keycode(4), true, true),
            "key from the head unit: code 4, action 0: CarPlay key Back"
        );
        assert_eq!(
            describe_key(1, 4, Key::from_keycode(4), false, true),
            "key from the head unit: code 4, action 1: CarPlay key Back released, nothing sent"
        );
        assert_eq!(
            describe_key(0, 999, Key::from_keycode(999), true, true),
            "key from the head unit: code 999, action 0: no CarPlay key"
        );
        assert_eq!(
            describe_key(2, 85, Key::from_keycode(85), true, false),
            "key from the head unit: code 85, action 2: CarPlay key PlayPause, not sent: CarPlay does not run"
        );
    }

    #[test]
    fn the_picture_turns_to_the_window() {
        let settings = Settings {
            size: picture::SizeSetting::Auto,
            fixed_size: (1280, 720),
            rate: picture::RateSetting::Fixed(60),
            custom_size: None,
            density: picture::DensitySetting::Real,
        };
        let tablet = HeadUnitPicture {
            width: 2458,
            height: 1484,
            dpi: 320,
            real_dpi: 206,
            fps: 30,
            screen_width: 2560,
            screen_height: 1600,
        };
        let mut wanted = Wanted {
            head_unit_id: Some("a1b2c3d4e5f6".into()),
            settings,
            head_unit: Some((tablet, Source::Config)),
            views: Views::default(),
            window: None,
        };
        let full = wanted.choice();
        assert_eq!((full.picture.width, full.picture.height), (2458, 1484));
        wanted.window = Some(Shape::Landscape);
        assert_eq!(wanted.choice(), full);
        wanted.window = Some(Shape::Portrait);
        let upright = wanted.choice();
        assert_eq!(
            upright.picture,
            Picture {
                dpi: Some(206),
                ..Picture::new(1484, 2458, 60)
            }
        );
        assert!(upright
            .reason
            .ends_with(", turned portrait for the head unit's window"));
        // The head unit's own full screen size is turned for its window as well.
        wanted.settings.custom_size = Some((2432, 1484));
        let custom = wanted.choice();
        assert_eq!((custom.picture.width, custom.picture.height), (1484, 2432));
        assert!(custom.reason.starts_with("custom size for this head unit"));
        // Without a head unit, the fixed size is turned as well.
        wanted.settings.custom_size = None;
        wanted.head_unit = None;
        assert_eq!(wanted.choice().picture, Picture::new(720, 1280, 60));
    }

    #[test]
    fn a_mode_change_renews_the_picture_when_its_density_differs() {
        use crate::views::Measured;
        use carbridge_carlink_session::display::PictureSize;
        let size = |width, height| PictureSize { width, height };
        let c10 = HeadUnitPicture {
            width: 2458,
            height: 1176,
            dpi: 320,
            real_dpi: 201,
            fps: 30,
            screen_width: 2560,
            screen_height: 1440,
        };
        let mut wanted = Wanted {
            head_unit_id: Some("5940a63aab3c".into()),
            settings: Settings {
                size: picture::SizeSetting::Auto,
                fixed_size: (1280, 720),
                rate: picture::RateSetting::Fixed(60),
                custom_size: Some((2432, 1164)),
                density: picture::DensitySetting::Real,
            },
            head_unit: Some((c10, Source::Config)),
            views: Views::default(),
            window: None,
        };
        // The iPhone's session is ended when the picture it was given is not the one wanted now (`follow`).
        let renews = |given: &Picture, wanted: &Wanted| *given != wanted.choice().picture;
        let full = wanted.choice();
        assert_eq!(
            full.to_string(),
            "2432x1164 at 60 fps, 201 dpi (custom size for this head unit, fixed frame rate, real dpi from the head unit's configuration)"
        );
        // Without views, full screen and the landscape window are the same picture: the session goes on.
        wanted.window = Some(Shape::Landscape);
        assert!(!renews(&full.picture, &wanted));
        // The landscape window calibrated: another density, which ends it.
        wanted.views.landscape_window = Some(Measured {
            view: size(1712, 819),
            picture: size(2432, 1164),
        });
        assert!(renews(&full.picture, &wanted));
        assert_eq!(wanted.choice().picture.dpi, Some(286));
        // The portrait window, turned and at the density of its own view.
        wanted.views.portrait_window = Some(Measured {
            view: size(528, 1104),
            picture: size(1164, 2432),
        });
        wanted.window = Some(Shape::Portrait);
        let portrait = wanted.choice();
        assert_eq!(
            portrait.picture,
            Picture {
                dpi: Some(443),
                ..Picture::new(1164, 2432, 60)
            }
        );
        assert!(portrait
            .reason
            .contains("dpi calibrated from the portrait window's view 528x1104"));
        assert!(renews(&full.picture, &wanted));
        // Back to full screen: the full screen picture again.
        wanted.window = None;
        assert!(!renews(&full.picture, &wanted));
        assert!(renews(&portrait.picture, &wanted));
        // The full screen view of the picture's own size changes nothing.
        wanted.views.full_screen = Some(Measured {
            view: size(2432, 1164),
            picture: size(2432, 1164),
        });
        assert!(!renews(&full.picture, &wanted));
    }

    #[test]
    fn touches_wait_while_the_iphone_draws_for_another_shape() {
        let feed = Feed::default();
        let landscape = Picture::new(2458, 1484, 30);
        feed.set_pictures(|pictures| {
            pictures.next = Some(landscape);
            pictures.session = Some(landscape);
        });
        assert!(!feed.reshaping());
        feed.set_pictures(|pictures| pictures.next = Some(landscape.turned(true)));
        assert!(feed.reshaping());
        feed.set_pictures(|pictures| pictures.session = None);
        assert!(!feed.reshaping());
        feed.set_pictures(|pictures| pictures.session = Some(landscape.turned(true)));
        assert!(!feed.reshaping());
    }

    #[test]
    fn now_playing_is_logged_when_more_than_its_position_changes() {
        let song = NowPlaying {
            title: "A".into(),
            artist: None,
            album: None,
            duration_ms: Some(1000),
            elapsed: None,
            playing: true,
            artwork: None,
        };
        assert_eq!(describe_now_playing(None, None), None);
        assert_eq!(
            describe_now_playing(None, Some(&song)).as_deref(),
            Some("CarPlay now playing: another song, playing, no artwork")
        );
        let mut moved = song.clone();
        moved.elapsed = Some((10, Instant::now()));
        assert_eq!(describe_now_playing(Some(&song), Some(&moved)), None);
        let mut covered = song.clone();
        covered.artwork = Some(vec![1, 2, 3].into());
        assert_eq!(
            describe_now_playing(Some(&song), Some(&covered)).as_deref(),
            Some("CarPlay now playing: the same song, playing, artwork of 3 bytes")
        );
        assert_eq!(
            describe_now_playing(Some(&song), None).as_deref(),
            Some("CarPlay now playing: nothing")
        );
    }

    #[test]
    fn shown_picture_follows_the_iphone() {
        let feed = Feed::default();
        let mut shown = feed.shown.subscribe();
        assert_eq!(*shown.borrow_and_update(), None);
        let fixed = Picture::new(1280, 720, 30);
        let head_unit = Picture::new(2458, 1484, 30);
        feed.set_pictures(|pictures| pictures.next = Some(fixed));
        assert!(shown.has_changed().unwrap());
        assert_eq!(*shown.borrow_and_update(), Some(fixed));
        // The iPhone connected now keeps its picture until it connects again.
        feed.set_pictures(|pictures| pictures.session = Some(fixed));
        assert!(!shown.has_changed().unwrap());
        feed.set_pictures(|pictures| pictures.next = Some(head_unit));
        assert!(!shown.has_changed().unwrap());
        assert_eq!(*shown.borrow(), Some(fixed));
        feed.set_pictures(|pictures| pictures.session = None);
        assert_eq!(*shown.borrow_and_update(), Some(head_unit));
        feed.set_pictures(|pictures| pictures.session = Some(head_unit));
        assert!(!shown.has_changed().unwrap());
    }
}
