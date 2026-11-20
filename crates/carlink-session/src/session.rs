// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The channels the head unit connects to after authentication, and the control channel's part of the session.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use carbridge_carlink_proto::frame::Message;
use carbridge_carlink_proto::header::{Category, Kind};
use carbridge_carlink_proto::pb::{
    AppInvokeCategory, AppStateCategory, AppStateChanged, BluetoothMacInfo, CarCertificate,
    CustomKeyEvent, GetAppDetailInfoResponse, GetAppListResponse, GetUCarConfigRequest,
    GetUCarConfigResponse, Heartbeat, InvokeApp, NotifySwitchDayOrNight, OpType,
};
use carbridge_carlink_proto::record::SessionKey;
use carbridge_carlink_proto::{command, port};
use tokio::net::{TcpListener, TcpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{interval, sleep_until, timeout, Instant, MissedTickBehavior};

use crate::channel::{now_millis, Channel};
pub use crate::display::{
    visible_region, Display, Layout, PictureSize, Shape, WindowMode, LANDSCAPE_FULL_SCREEN,
    LANDSCAPE_WINDOW, PORTRAIT_FULL_SCREEN, PORTRAIT_WINDOW,
};
use crate::Error;

/// Media messages waiting for the media channel; more are dropped.
const MEDIA_QUEUE: usize = 64;

/// The ports the phone listens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub control: u16,
    pub media: u16,
    pub sensor: u16,
    pub cert: u16,
    pub video: u16,
}

impl Ports {
    /// The fixed ports of the protocol.
    pub const STANDARD: Ports = Ports {
        control: port::CONTROL,
        media: port::MEDIA,
        sensor: port::SENSOR,
        cert: port::CERT,
        video: port::VIDEO,
    };
}

/// The phone's listening sockets, bound before it confirms the authentication.
pub struct Listeners {
    control: TcpListener,
    media: TcpListener,
    sensor: TcpListener,
    cert: TcpListener,
    video: TcpListener,
}

impl Listeners {
    /// Listens on `address`, the phone's address on the head unit's network, at `ports`.
    pub async fn bind(address: Ipv4Addr, ports: Ports) -> io::Result<Listeners> {
        Listeners::bind_with_tos(address, ports, None).await
    }

    /// Listens as [`Listeners::bind`] does, the channels sending with `tos` as their IPv4 type of service ([`listen`]).
    pub async fn bind_with_tos(
        address: Ipv4Addr,
        ports: Ports,
        tos: Option<u32>,
    ) -> io::Result<Listeners> {
        let bind = |port| listen(SocketAddr::from((address, port)), tos);
        Ok(Listeners {
            control: bind(ports.control)?,
            media: bind(ports.media)?,
            sensor: bind(ports.sensor)?,
            cert: bind(ports.cert)?,
            video: bind(ports.video)?,
        })
    }

    /// The ports actually bound.
    pub fn ports(&self) -> io::Result<Ports> {
        Ok(Ports {
            control: self.control.local_addr()?.port(),
            media: self.media.local_addr()?.port(),
            sensor: self.sensor.local_addr()?.port(),
            cert: self.cert.local_addr()?.port(),
            video: self.video.local_addr()?.port(),
        })
    }
}

/// Listens on `address` as [`TcpListener::bind`] does. With `tos`, the IPv4 type of service of what the listener
/// sends, which the connections it accepts take over from it on Linux. Must be called within a Tokio runtime.
pub fn listen(address: SocketAddr, tos: Option<u32>) -> io::Result<TcpListener> {
    let socket = match address {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    socket.set_reuseaddr(true)?;
    if let Some(tos) = tos {
        socket.set_tos_v4(tos)?;
    }
    socket.bind(address)?;
    socket.listen(1024)
}

/// Timing and identity of a session.
#[derive(Clone, Debug)]
pub struct Options {
    /// SHA-256 hash of the phone's Bluetooth address, sent in `BluetoothMacInfo` when the control channel connects so
    /// that the head unit recognises the phone later.
    pub bluetooth_hash: Option<String>,
    /// How long the head unit has to connect the control channel.
    pub control_timeout: Duration,
    /// How often the phone sends a heartbeat.
    pub heartbeat_interval: Duration,
    /// How long without anything from the head unit counts as a lost session.
    pub heartbeat_timeout: Duration,
    /// The size of the phone's picture, which the head unit sizes its cast window to; without it the head unit sizes
    /// the window by the video. [`Request::Picture`] changes it. It may be of either shape: the head unit is told the
    /// picture turned to the display's shape.
    pub picture: Option<PictureSize>,
    /// How the head unit shows the cast when it connects; its window button and window mode menu change it
    /// ([`Event::Display`]).
    pub display: Display,
    /// Where the media messages the session drops are counted.
    pub media_drops: Option<Arc<MediaDrops>>,
    /// Where the media channel's socket is shown while the head unit has it connected.
    pub media_socket: Option<Arc<SocketWatch>>,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            bluetooth_hash: None,
            control_timeout: Duration::from_secs(10),
            heartbeat_interval: Duration::from_secs(2),
            heartbeat_timeout: Duration::from_secs(7),
            picture: None,
            display: Display::default(),
            media_drops: None,
            media_socket: None,
        }
    }
}

/// Counts of messages dropped on their way to the head unit, by where they were dropped and by command (for media,
/// the audio type).
#[derive(Debug, Default)]
pub struct MediaDrops(Mutex<Vec<(&'static str, u16, u64)>>);

impl MediaDrops {
    /// The media channel's queue was full.
    pub const MEDIA_QUEUE: &'static str = "media queue";
    /// The head unit had not connected the media channel.
    pub const NOT_CONNECTED: &'static str = "media channel not connected";

    /// Counts one message of `command` dropped at `stage`.
    pub fn add(&self, stage: &'static str, command: u16) {
        let mut counts = lock(&self.0);
        match counts
            .iter_mut()
            .find(|(s, c, _)| *s == stage && *c == command)
        {
            Some((_, _, count)) => *count += 1,
            None => counts.push((stage, command, 1)),
        }
    }

    /// The counts since the last call, in the order they were first counted, and starts again from none.
    pub fn take(&self) -> Vec<(&'static str, u16, u64)> {
        std::mem::take(&mut *lock(&self.0))
    }
}

/// The socket of a connection while it lasts, for its owner to look into, as at its TCP state. The socket stays open
/// while [`SocketWatch::with`] runs.
#[derive(Debug, Default)]
pub struct SocketWatch(Mutex<Option<RawFd>>);

impl SocketWatch {
    /// Shows `socket` until the guard drops; the guard must drop before the socket closes.
    pub fn watch(watch: &Arc<SocketWatch>, socket: &impl AsRawFd) -> Watched {
        *lock(&watch.0) = Some(socket.as_raw_fd());
        Watched(watch.clone())
    }

    /// Runs `look` on the socket, if there is one now.
    pub fn with<T>(&self, look: impl FnOnce(RawFd) -> T) -> Option<T> {
        let socket = lock(&self.0);
        socket.map(look)
    }
}

/// Shows a socket in a [`SocketWatch`] until it drops.
#[derive(Debug)]
pub struct Watched(Arc<SocketWatch>);

impl Drop for Watched {
    fn drop(&mut self) {
        let Watched(watch) = self;
        *lock(&watch.0) = None;
    }
}

/// Locks `mutex`, also after a panic while it was held: what it guards stays usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the head unit did.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The control channel connected and the first heartbeat went out.
    ControlConnected,
    /// The head unit's configuration.
    Config(GetUCarConfigResponse),
    /// The cast view is on screen.
    Foreground,
    /// The cast view left the screen.
    Background,
    /// The head unit switched between day and night.
    DayNight(i32),
    Key(CustomKeyEvent),
    /// The head unit started, stopped, paused or resumed an app; the session has already reported its new state.
    InvokeApp(InvokeApp),
    /// The phone reported an app's state to the head unit ([`describe_app_state`]).
    AppState(AppStateChanged),
    /// The head unit shows the cast another way from now on: its window button switched between full screen and the
    /// window, or it started another window mode's app. The new state has been reported already.
    Display(Display),
    /// The head unit's certificate data.
    Certificate(Vec<u8>),
    /// Any other message on the control channel.
    Control(Message),
    /// A message on the sensor channel.
    Sensor(Message),
    /// A message on the media channel.
    Media(Message),
}

/// What the session's owner asks for.
#[derive(Debug)]
pub enum Request {
    /// Send a message on the control channel.
    Send(Message),
    /// Send a message on the media channel; dropped while the head unit has not connected it, or when it lags.
    Media(Message),
    /// The phone's picture is now this size ([`Options::picture`]); the app on screen has its state reported again
    /// for the head unit's window to follow.
    Picture(Option<PictureSize>),
    /// End the session.
    Stop,
}

/// How the session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The head unit sent `Disconnect`.
    HeadUnitDisconnected,
    /// Nothing came from the head unit for the heartbeat timeout.
    HeartbeatLost,
    /// The head unit closed the control channel.
    ControlClosed,
    /// The head unit did not connect the control channel in time.
    ControlNotConnected,
    /// The owner asked to stop, or dropped its request sender.
    Stopped,
}

/// Runs a session until it ends: serves the channels the head unit connects to and keeps the control channel alive.
pub async fn run(
    listeners: Listeners,
    key: SessionKey,
    options: Options,
    events: mpsc::Sender<Event>,
    mut requests: mpsc::Receiver<Request>,
) -> Result<End, Error> {
    let mut side = JoinSet::new();
    let (media, media_out) = mpsc::channel(MEDIA_QUEUE);
    let media_side = Watching {
        drops: options.media_drops.clone(),
        socket: options.media_socket.clone(),
    };
    side.spawn(serve(
        listeners.media,
        Some(key.clone()),
        Side::Media,
        events.clone(),
        Some(media_out),
        media_side,
    ));
    side.spawn(serve(
        listeners.sensor,
        Some(key.clone()),
        Side::Sensor,
        events.clone(),
        None,
        Watching::default(),
    ));
    side.spawn(serve(
        listeners.cert,
        Some(key.clone()),
        Side::Cert,
        events.clone(),
        None,
        Watching::default(),
    ));
    side.spawn(serve(
        listeners.video,
        None,
        Side::Video,
        events.clone(),
        None,
        Watching::default(),
    ));

    let stream = match timeout(options.control_timeout, listeners.control.accept()).await {
        Ok(accepted) => accepted?.0,
        Err(_) => return Ok(End::ControlNotConnected),
    };
    let mut control = Channel::new(stream, Some(key));
    let mut beat = interval(options.heartbeat_interval);
    beat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heard = Instant::now();
    let mut asked_config = false;
    let mut announced = false;
    let mut apps = Apps::new(options.picture, options.display);

    loop {
        tokio::select! {
            _ = beat.tick() => {
                let heartbeat = Heartbeat { timestamp: now_millis() };
                control.send(Message::protobuf(Category::CONTROL, command::control::HEARTBEAT, Kind::Send, &heartbeat)).await?;
                if !announced {
                    announced = true;
                    let _ = events.send(Event::ControlConnected).await;
                    if let Some(hash) = &options.bluetooth_hash {
                        let info = BluetoothMacInfo { op_type: OpType::OpAdd as i32, bluetooth_mac: hash.clone() };
                        control.send(Message::protobuf(Category::CONTROL, command::control::BLUETOOTH_MAC_INFO, Kind::SendAck, &info)).await?;
                    }
                }
            }
            _ = sleep_until(heard + options.heartbeat_timeout) => return Ok(End::HeartbeatLost),
            request = requests.recv() => match request {
                Some(Request::Send(message)) => { control.send(message).await?; }
                Some(Request::Media(message)) => {
                    if let Err(err) = media.try_send(message) {
                        if let Some(drops) = &options.media_drops {
                            let message = match err {
                                mpsc::error::TrySendError::Full(message) | mpsc::error::TrySendError::Closed(message) => message,
                            };
                            drops.add(MediaDrops::MEDIA_QUEUE, message.command);
                        }
                    }
                }
                Some(Request::Picture(picture)) => {
                    if let Some(state) = apps.resize(picture) {
                        report(&mut control, &events, state).await?;
                    }
                }
                Some(Request::Stop) | None => return Ok(End::Stopped),
            },
            received = control.recv() => {
                let message = match received {
                    Ok(message) => message,
                    Err(Error::Closed) => return Ok(End::ControlClosed),
                    Err(err) => return Err(err),
                };
                heard = Instant::now();
                if message.kind == Kind::SendAck {
                    control.reply(message.ack()).await?;
                }
                if message.category == Category::ACK {
                    continue;
                }
                if message.category != Category::CONTROL {
                    let _ = events.send(Event::Control(message)).await;
                    continue;
                }
                match message.command {
                    command::control::HEARTBEAT => {
                        if !asked_config {
                            asked_config = true;
                            let request = Message::protobuf(Category::CONTROL, command::control::GET_UCAR_CONFIG_REQUEST, Kind::Request, &GetUCarConfigRequest {});
                            control.send(request).await?;
                        }
                    }
                    command::control::DISCONNECT => return Ok(End::HeadUnitDisconnected),
                    command::control::GET_UCAR_CONFIG_RESPONSE => {
                        let _ = events.send(Event::Config(message.decode_body()?)).await;
                    }
                    command::control::NOTIFY_CAR_TO_FOREGROUND => { let _ = events.send(Event::Foreground).await; }
                    command::control::NOTIFY_CAR_TO_BACKGROUND => { let _ = events.send(Event::Background).await; }
                    command::control::NOTIFY_SWITCH_DAY_OR_NIGHT => {
                        let mode: NotifySwitchDayOrNight = message.decode_body()?;
                        let _ = events.send(Event::DayNight(mode.mode)).await;
                    }
                    command::control::CUSTOM_KEY_EVENT => {
                        let _ = events.send(Event::Key(message.decode_body()?)).await;
                    }
                    // The phone offers no apps of its own; answering at once spares the head unit its timeout.
                    command::control::GET_APP_LIST_REQUEST => {
                        let empty = GetAppListResponse::default();
                        control.reply(message.response(command::control::GET_APP_LIST_RESPONSE, &empty)).await?;
                    }
                    command::control::GET_APP_DETAIL_INFO_REQUEST => {
                        let empty = GetAppDetailInfoResponse::default();
                        control.reply(message.response(command::control::GET_APP_DETAIL_INFO_RESPONSE, &empty)).await?;
                    }
                    command::control::INVOKE_APP => {
                        let invoke: InvokeApp = message.decode_body()?;
                        let (state, display) = apps.invoked(&invoke);
                        report(&mut control, &events, state).await?;
                        let _ = events.send(Event::InvokeApp(invoke)).await;
                        if let Some(display) = display {
                            let _ = events.send(Event::Display(display)).await;
                        }
                    }
                    _ => { let _ = events.send(Event::Control(message)).await; }
                }
            }
        }
    }
}

/// Reports an app's state to the head unit, and to the owner.
async fn report(
    control: &mut Channel,
    events: &mpsc::Sender<Event>,
    state: AppStateChanged,
) -> Result<(), Error> {
    // The head unit only takes this as a message that waits for its ack.
    control
        .send(Message::protobuf(
            Category::CONTROL,
            command::control::APP_STATE_CHANGED,
            Kind::SendAck,
            &state,
        ))
        .await?;
    let _ = events.send(Event::AppState(state)).await;
    Ok(())
}

/// A line for the log about the state the phone reported, as "app state for app 10: resumed, full screen landscape
/// 2458x1484".
pub fn describe_app_state(state: &AppStateChanged) -> String {
    let name = match AppStateCategory::try_from(state.state) {
        Ok(AppStateCategory::AppStateStarted) => "started".to_owned(),
        Ok(AppStateCategory::AppStateStopped) => "stopped".to_owned(),
        Ok(AppStateCategory::AppStatePaused) => "paused".to_owned(),
        Ok(AppStateCategory::AppStateResumed) => "resumed".to_owned(),
        Ok(AppStateCategory::AppStateDestroyed) => "destroyed".to_owned(),
        Err(_) => format!("state {}", state.state),
    };
    let mut line = format!("app state for app {}: {name}", state.id);
    if let Some(region) = &state.visible_region {
        let mode = match region.mode {
            LANDSCAPE_FULL_SCREEN => "full screen landscape".to_owned(),
            PORTRAIT_FULL_SCREEN => "full screen portrait".to_owned(),
            LANDSCAPE_WINDOW => "window landscape".to_owned(),
            PORTRAIT_WINDOW => "window portrait".to_owned(),
            mode => format!("display mode {mode}"),
        };
        line += &format!(
            ", {mode} {}x{}",
            region.right - region.left,
            region.bottom - region.top
        );
    }
    line
}

/// The states the phone reports for the apps the head unit invokes. The head unit shows a window cast only once the
/// phone reports its app; it shows it full screen or in its window as the display mode of the state's visible region
/// says, sized by the region, or without one by the video.
pub(crate) struct Apps {
    /// The picture, turned to the screen's shape.
    picture: Option<PictureSize>,
    display: Display,
    /// The app on screen: the one reported resumed last, unless it was stopped or paused since.
    shown: Option<i32>,
}

impl Apps {
    pub(crate) fn new(picture: Option<PictureSize>, display: Display) -> Apps {
        Apps {
            picture: picture.map(|picture| picture.turned(display.screen)),
            display,
            shown: None,
        }
    }

    /// The state to report for an app the head unit invoked, and the display from now on when it changed. Every start
    /// is answered, a repeated one too: a head unit that starts an app pauses the cast when no state follows each
    /// start within a few seconds. UPDATE switches between full screen and the window; starting, resuming or updating
    /// a window mode's app makes that mode the window's.
    pub(crate) fn invoked(&mut self, invoke: &InvokeApp) -> (AppStateChanged, Option<Display>) {
        let invoked = AppInvokeCategory::try_from(invoke.invoke_state);
        let before = self.display;
        if invoked == Ok(AppInvokeCategory::InvokeUpdate) {
            self.display.layout = self.display.layout.switched();
        }
        let leaves = matches!(
            invoked,
            Ok(AppInvokeCategory::InvokeStop) | Ok(AppInvokeCategory::InvokePause)
        );
        if let Some(mode) = WindowMode::from_app(invoke.id).filter(|_| !leaves) {
            self.display.window_mode = Some(mode);
        }
        let state = match invoked {
            Ok(AppInvokeCategory::InvokeStop) => AppStateCategory::AppStateStopped,
            Ok(AppInvokeCategory::InvokePause) => AppStateCategory::AppStatePaused,
            _ => AppStateCategory::AppStateResumed,
        };
        let state = self.state(invoke.id, state);
        if state.is_need_show {
            self.shown = Some(invoke.id);
        } else if self.shown == Some(invoke.id) {
            self.shown = None;
        }
        let changed = (self.display != before).then_some(self.display);
        (stamped(state), changed)
    }

    /// Takes the picture's size, of either shape; the state to report again for the app on screen when the size
    /// changed.
    pub(crate) fn resize(&mut self, picture: Option<PictureSize>) -> Option<AppStateChanged> {
        let picture = picture.map(|picture| picture.turned(self.display.screen));
        if picture == self.picture {
            return None;
        }
        self.picture = picture;
        let state = self.state(self.shown?, AppStateCategory::AppStateResumed);
        Some(stamped(state))
    }

    /// The state of app `id`, with no timestamp. The head unit may switch between full screen and its window.
    fn state(&self, id: i32, state: AppStateCategory) -> AppStateChanged {
        AppStateChanged {
            id,
            is_need_show: state == AppStateCategory::AppStateResumed,
            display_rotation: 0,
            state: state as i32,
            timestamp: 0,
            visible_region: self.picture.map(|picture| self.display.region(picture)),
            is_allowed_switch_mode: Some(true),
        }
    }
}

/// `state` with the time it is reported.
fn stamped(state: AppStateChanged) -> AppStateChanged {
    AppStateChanged {
        timestamp: now_millis(),
        ..state
    }
}

#[derive(Clone, Copy)]
enum Side {
    Media,
    Sensor,
    Cert,
    Video,
}

/// What a side channel counts and shows of itself.
#[derive(Default)]
struct Watching {
    drops: Option<Arc<MediaDrops>>,
    socket: Option<Arc<SocketWatch>>,
}

/// Serves one side channel: one connection at a time, a further one closed at once while it lasts.
async fn serve(
    listener: TcpListener,
    key: Option<SessionKey>,
    side: Side,
    events: mpsc::Sender<Event>,
    mut outgoing: Option<mpsc::Receiver<Message>>,
    watching: Watching,
) {
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => return,
            },
            // Nothing to send to before the head unit connects.
            Some(message) = next_outgoing(&mut outgoing) => {
                if let Some(drops) = &watching.drops {
                    drops.add(MediaDrops::NOT_CONNECTED, message.command);
                }
                continue;
            }
        };
        let mut channel = Channel::new(stream, key.clone());
        // Declared after the channel, so that the socket is no longer shown when the channel closes it.
        let _watched = watching
            .socket
            .as_ref()
            .map(|watch| SocketWatch::watch(watch, channel.socket()));
        loop {
            tokio::select! {
                extra = listener.accept() => drop(extra),
                Some(message) = next_outgoing(&mut outgoing) => {
                    if channel.send(message).await.is_err() {
                        break;
                    }
                }
                received = channel.recv() => {
                    let Ok(message) = received else { break };
                    if message.kind == Kind::SendAck && channel.reply(message.ack()).await.is_err() {
                        break;
                    }
                    let event = match side {
                        Side::Media => Event::Media(message),
                        Side::Sensor => Event::Sensor(message),
                        Side::Cert if message.command == command::cert::CAR_CERTIFICATE => {
                            match message.decode_body::<CarCertificate>() {
                                Ok(cert) => Event::Certificate(cert.content),
                                Err(_) => continue,
                            }
                        }
                        Side::Cert | Side::Video => continue,
                    };
                    if events.send(event).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// The next message to send on a side channel; never ready on a channel the phone does not send on.
async fn next_outgoing(outgoing: &mut Option<mpsc::Receiver<Message>>) -> Option<Message> {
    match outgoing {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}
