// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The phone's side against a head unit written from the protocol description.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use carbridge_carlink_proto::auth::{
    derive_session_key, request_hmac, response_hmac, AuthKey, Handshake, PhoneInfo, PublicKey,
    RANDOM_LEN,
};
use carbridge_carlink_proto::command;
use carbridge_carlink_proto::frame::Message;
use carbridge_carlink_proto::header::{BodyFormat, Category, Kind, Source};
use carbridge_carlink_proto::pb::{
    AppInvokeCategory, AppStateCategory, AppStateChanged, AuthConfirm, AuthRequest, AuthResponse,
    BluetoothMacInfo, CarCertificate, Disconnect, DisplayVisibleRegion, GetAppListRequest,
    GetAppListResponse, GetUCarConfigResponse, Heartbeat, InvokeApp,
};
use carbridge_carlink_proto::record::SessionKey;
use p256::ecdh::EphemeralSecret;
use rand_core::{OsRng, RngCore};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::auth::authenticate;
use crate::channel::Channel;
use crate::session::{
    describe_app_state, run, visible_region, Apps, Display, End, Event, Layout, Listeners, Options,
    PictureSize, Ports, Request, Shape, WindowMode, LANDSCAPE_FULL_SCREEN, LANDSCAPE_WINDOW,
    PORTRAIT_FULL_SCREEN, PORTRAIT_WINDOW,
};
use crate::Error;

const PIN: &str = "932914";
const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;

fn phone() -> PhoneInfo {
    PhoneInfo {
        id: "cbtest".into(),
        model: "CarBridge".into(),
        work_modes: Some(1),
        ..PhoneInfo::default()
    }
}

fn from_head_unit(mut message: Message) -> Message {
    message.source = Source::HeadUnit;
    message
}

/// Answers one authentication on `listener`; returns the session key and the address the phone confirmed.
async fn head_unit_auth(
    listener: TcpListener,
    car_key: AuthKey,
    pin: &str,
) -> Option<(SessionKey, String)> {
    let (stream, _) = listener.accept().await.unwrap();
    let mut channel = Channel::new(stream, None);
    let request_message = channel.recv().await.unwrap();
    assert_eq!(
        (request_message.category, request_message.command),
        (Category::AUTH, 1)
    );
    assert_eq!(request_message.kind, Kind::Request);
    let request: AuthRequest = request_message.decode_body().unwrap();
    let phone_key = PublicKey::from_wire(&request.auth_pk).unwrap();
    let respond = |response: AuthResponse| {
        from_head_unit(request_message.response(command::auth::AUTH_RESPONSE, &response))
    };
    if request.auth_pk_hmac != request_hmac(&request.random, pin, &request.auth_pk) {
        channel
            .reply(respond(AuthResponse {
                result: 3,
                ..AuthResponse::default()
            }))
            .await
            .unwrap();
        return None;
    }
    phone_key
        .verify(
            &[&request.agreement_pk, &request.random],
            &request.agreement_pk_sig,
        )
        .unwrap();
    let agreement = EphemeralSecret::random(&mut OsRng);
    let agreement_pk = PublicKey::from_wire(agreement.public_key().to_sec1_bytes().as_ref())
        .unwrap()
        .to_wire();
    let mut random = [0u8; RANDOM_LEN];
    OsRng.fill_bytes(&mut random);
    let car_pk = car_key.public_key().to_wire();
    let response = AuthResponse {
        version: 1793,
        auth_pk: car_pk.to_vec(),
        auth_pk_hmac: response_hmac(&random, &request.random, pin, &car_pk, &request.auth_pk)
            .to_vec(),
        agreement_pk_sig: car_key.sign(&[
            &agreement_pk,
            &request.agreement_pk,
            &random,
            &request.random,
        ]),
        agreement_pk: agreement_pk.to_vec(),
        random: random.to_vec(),
        result: 0,
        work_modes: Some(1),
        day_or_night_mode: None,
        screen_info: Some("2560_1600_320".into()),
    };
    channel.reply(respond(response)).await.unwrap();
    let phone_agreement = PublicKey::from_wire(&request.agreement_pk).unwrap();
    let shared = agreement.diffie_hellman(
        &p256::PublicKey::from_sec1_bytes(&[&[4u8][..], &phone_agreement.to_wire()].concat())
            .unwrap(),
    );
    let key = SessionKey::new(&derive_session_key(
        shared.raw_secret_bytes(),
        &request.random,
        &random,
    ));
    let confirm_message = channel.recv().await.unwrap();
    assert_eq!(
        (confirm_message.command, confirm_message.kind),
        (command::auth::AUTH_CONFIRM, Kind::Send)
    );
    let confirm: AuthConfirm = confirm_message.decode_body().unwrap();
    let address = String::from_utf8(key.open(&confirm.cipher).unwrap()).unwrap();
    Some((key, address))
}

#[tokio::test]
async fn authentication_with_pin() {
    let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let car_key = AuthKey::generate();
    let car_public = car_key.public_key();
    let car = tokio::spawn(head_unit_auth(listener, car_key, PIN));
    let handshake = Handshake::first_pairing(&AuthKey::generate(), PIN, &phone());
    let accepted = authenticate(addr, handshake).await.unwrap();
    assert_eq!(accepted.session.car_auth_key, car_public);
    let session = accepted
        .confirm(Ipv4Addr::new(192, 168, 49, 117))
        .await
        .unwrap();
    let (car_session_key, address) = car.await.unwrap().unwrap();
    assert_eq!(address, "192.168.49.117:0");
    let probe = session.key.seal(b"probe");
    assert_eq!(car_session_key.open(&probe).unwrap(), b"probe");
}

#[tokio::test]
async fn authentication_with_wrong_pin() {
    let listener = TcpListener::bind((LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let car = tokio::spawn(head_unit_auth(listener, AuthKey::generate(), PIN));
    let handshake = Handshake::first_pairing(&AuthKey::generate(), "000000", &phone());
    let err = authenticate(addr, handshake).await.err().unwrap();
    assert!(
        matches!(
            err,
            Error::Auth(carbridge_carlink_proto::auth::AuthError::Rejected)
        ),
        "{err}"
    );
    assert!(car.await.unwrap().is_none());
}

struct Running {
    ports: Ports,
    key: SessionKey,
    events: mpsc::Receiver<Event>,
    requests: mpsc::Sender<Request>,
    session: tokio::task::JoinHandle<Result<End, Error>>,
}

async fn start(options: Options) -> Running {
    let listeners = Listeners::bind(
        LOCALHOST,
        Ports {
            control: 0,
            media: 0,
            sensor: 0,
            cert: 0,
            video: 0,
        },
    )
    .await
    .unwrap();
    let ports = listeners.ports().unwrap();
    let key = SessionKey::new(&[9; 16]);
    let (event_tx, events) = mpsc::channel(32);
    let (requests, request_rx) = mpsc::channel(8);
    let session = tokio::spawn(run(listeners, key.clone(), options, event_tx, request_rx));
    Running {
        ports,
        key,
        events,
        requests,
        session,
    }
}

async fn connect(port: u16, key: Option<SessionKey>) -> Channel {
    Channel::new(
        TcpStream::connect(SocketAddr::from((LOCALHOST, port)))
            .await
            .unwrap(),
        key,
    )
}

fn car_message<M: prost::Message>(
    category: Category,
    command: u16,
    kind: Kind,
    body: &M,
) -> Message {
    from_head_unit(Message::protobuf(category, command, kind, body))
}

#[tokio::test]
async fn control_channel_session() {
    let options = Options {
        bluetooth_hash: Some("ab".repeat(32)),
        picture: Some(TABLET),
        ..Options::default()
    };
    let mut s = start(options).await;
    let mut cert = connect(s.ports.cert, Some(s.key.clone())).await;
    cert.send(car_message(
        Category::CERT,
        command::cert::CAR_CERTIFICATE,
        Kind::Send,
        &CarCertificate {
            content: b"cert".to_vec(),
        },
    ))
    .await
    .unwrap();
    let mut control = connect(s.ports.control, Some(s.key.clone())).await;

    // The phone's first heartbeat, then its Bluetooth hash, which waits for an ack.
    let first = control.recv().await.unwrap();
    assert_eq!(
        (first.category, first.command, first.source),
        (
            Category::CONTROL,
            command::control::HEARTBEAT,
            Source::Phone
        )
    );
    assert!(first.decode_body::<Heartbeat>().unwrap().timestamp > 0);
    let mac = control.recv().await.unwrap();
    assert_eq!(
        (mac.command, mac.kind),
        (command::control::BLUETOOTH_MAC_INFO, Kind::SendAck)
    );
    assert_eq!(
        mac.decode_body::<BluetoothMacInfo>().unwrap().bluetooth_mac,
        "ab".repeat(32)
    );
    control.reply(from_head_unit(mac.ack())).await.unwrap();

    // The head unit's heartbeat draws the configuration request.
    control
        .send(car_message(
            Category::CONTROL,
            command::control::HEARTBEAT,
            Kind::Send,
            &Heartbeat { timestamp: 1 },
        ))
        .await
        .unwrap();
    let request = control.recv().await.unwrap();
    assert_eq!(
        (request.command, request.kind),
        (command::control::GET_UCAR_CONFIG_REQUEST, Kind::Request)
    );
    let config = GetUCarConfigResponse {
        screen_width: 2560,
        screen_height: 1600,
        fps: 30,
        ..GetUCarConfigResponse::default()
    };
    control
        .reply(from_head_unit(
            request.response(command::control::GET_UCAR_CONFIG_RESPONSE, &config),
        ))
        .await
        .unwrap();

    // A key from the head unit, as a send-and-ack.
    let foreground = car_message(
        Category::CONTROL,
        command::control::NOTIFY_CAR_TO_FOREGROUND,
        Kind::SendAck,
        &Heartbeat { timestamp: 2 },
    );
    let sequence = control.send(foreground).await.unwrap();
    let ack = loop {
        let message = control.recv().await.unwrap();
        if message.category == Category::ACK {
            break message;
        }
    };
    assert_eq!(
        (ack.sequence, ack.kind, ack.format),
        (sequence, Kind::Response, BodyFormat::Raw)
    );

    let mut seen = Vec::new();
    while seen.len() < 4 {
        seen.push(s.events.recv().await.unwrap());
    }
    assert!(seen.contains(&Event::ControlConnected));
    assert!(seen.contains(&Event::Certificate(b"cert".to_vec())));
    assert!(seen.contains(&Event::Config(config)));
    assert!(seen.contains(&Event::Foreground));

    // Window cast: the head unit asks for apps, then invokes its virtual app and waits for its state.
    let list = car_message(
        Category::CONTROL,
        command::control::GET_APP_LIST_REQUEST,
        Kind::Request,
        &GetAppListRequest {},
    );
    let list_sequence = control.send(list).await.unwrap();
    let list_response = control.recv().await.unwrap();
    assert_eq!(
        (
            list_response.command,
            list_response.kind,
            list_response.sequence
        ),
        (
            command::control::GET_APP_LIST_RESPONSE,
            Kind::Response,
            list_sequence
        )
    );
    assert_eq!(
        list_response
            .decode_body::<GetAppListResponse>()
            .unwrap()
            .app_info
            .len(),
        0
    );
    let invoke = InvokeApp {
        id: 10,
        package_name: String::new(),
        invoke_state: 0,
        display_mode: 0,
    };
    control
        .send(car_message(
            Category::CONTROL,
            command::control::INVOKE_APP,
            Kind::SendAck,
            &invoke,
        ))
        .await
        .unwrap();
    let invoke_ack = control.recv().await.unwrap();
    assert_eq!(invoke_ack.category, Category::ACK);
    let state = loop {
        let message = control.recv().await.unwrap();
        if message.command == command::control::APP_STATE_CHANGED {
            assert_eq!(message.kind, Kind::SendAck);
            break message.decode_body::<AppStateChanged>().unwrap();
        }
    };
    assert_eq!(
        (state.id, state.is_need_show, state.state),
        (10, true, AppStateCategory::AppStateResumed as i32)
    );
    assert_eq!(state.visible_region, Some(visible_region(TABLET)));
    assert_eq!(state.is_allowed_switch_mode, Some(true));
    assert_eq!(s.events.recv().await, Some(Event::AppState(state)));
    assert_eq!(s.events.recv().await, Some(Event::InvokeApp(invoke)));
    // The mirror window's app: the window mode is the mirror window's from now on.
    let mirror = Display {
        window_mode: Some(WindowMode::Mirror),
        ..Display::default()
    };
    assert_eq!(s.events.recv().await, Some(Event::Display(mirror)));

    // A new picture size: the app on screen is reported again, with the new region.
    let wide = PictureSize {
        width: 1920,
        height: 720,
    };
    s.requests.send(Request::Picture(Some(wide))).await.unwrap();
    let again = loop {
        let message = control.recv().await.unwrap();
        if message.command == command::control::APP_STATE_CHANGED {
            break message.decode_body::<AppStateChanged>().unwrap();
        }
    };
    assert_eq!(
        (again.id, again.is_need_show, again.state),
        (10, true, AppStateCategory::AppStateResumed as i32)
    );
    assert_eq!(again.visible_region, Some(visible_region(wide)));
    assert_eq!(s.events.recv().await, Some(Event::AppState(again)));

    // The window button, then the simple launcher window from the window's mode menu.
    let window = invoke_and_report(&mut control, 10, AppInvokeCategory::InvokeUpdate, 1).await;
    let region = window.visible_region.unwrap();
    assert_eq!(
        (region.mode, region.right, region.bottom),
        (LANDSCAPE_WINDOW, 1920, 720)
    );
    assert_eq!(s.events.recv().await, Some(Event::AppState(window)));
    assert!(matches!(s.events.recv().await, Some(Event::InvokeApp(_))));
    let in_window = Display {
        layout: Layout::Window,
        ..mirror
    };
    assert_eq!(s.events.recv().await, Some(Event::Display(in_window)));
    let simple = invoke_and_report(&mut control, 11, AppInvokeCategory::InvokeStart, 1).await;
    let region = simple.visible_region.unwrap();
    assert_eq!(
        (region.mode, region.origin_width, region.origin_height),
        (PORTRAIT_WINDOW, 720, 1920)
    );
    assert_eq!(s.events.recv().await, Some(Event::AppState(simple)));
    assert!(matches!(s.events.recv().await, Some(Event::InvokeApp(_))));
    let portrait = Display {
        window_mode: Some(WindowMode::Simple),
        ..in_window
    };
    assert_eq!(s.events.recv().await, Some(Event::Display(portrait)));
    assert_eq!(portrait.shape(), Shape::Portrait);
    control
        .send(car_message(
            Category::CONTROL,
            command::control::DISCONNECT,
            Kind::SendAck,
            &Disconnect {},
        ))
        .await
        .unwrap();
    assert_eq!(s.session.await.unwrap().unwrap(), End::HeadUnitDisconnected);
    drop(s.requests);
}

#[tokio::test]
async fn heartbeat_lost() {
    let options = Options {
        heartbeat_interval: Duration::from_millis(50),
        heartbeat_timeout: Duration::from_millis(300),
        ..Options::default()
    };
    let s = start(options).await;
    let _control = connect(s.ports.control, Some(s.key.clone())).await;
    assert_eq!(s.session.await.unwrap().unwrap(), End::HeartbeatLost);
}

#[tokio::test]
async fn control_never_connects() {
    let options = Options {
        control_timeout: Duration::from_millis(100),
        ..Options::default()
    };
    let s = start(options).await;
    assert_eq!(s.session.await.unwrap().unwrap(), End::ControlNotConnected);
}

#[tokio::test]
async fn owner_stops() {
    let s = start(Options::default()).await;
    let _control = connect(s.ports.control, Some(s.key.clone())).await;
    s.requests.send(Request::Stop).await.unwrap();
    assert_eq!(s.session.await.unwrap().unwrap(), End::Stopped);
}

#[tokio::test]
async fn closed_control_channel() {
    let s = start(Options::default()).await;
    drop(connect(s.ports.control, Some(s.key.clone())).await);
    assert_eq!(s.session.await.unwrap().unwrap(), End::ControlClosed);
}

#[tokio::test]
async fn media_channel_carries_the_phones_audio() {
    let s = start(Options::default()).await;
    let _control = connect(s.ports.control, Some(s.key.clone())).await;
    // Sent before the head unit connects the media channel: dropped.
    let early = Message::new(
        Category::AUDIO,
        6,
        Kind::Send,
        BodyFormat::Plain,
        vec![0; 4],
    );
    s.requests.send(Request::Media(early)).await.unwrap();
    let mut media = connect(s.ports.media, Some(s.key.clone())).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let pcm = Message::new(
        Category::AUDIO,
        6,
        Kind::Send,
        BodyFormat::Plain,
        vec![1, 2, 3, 4],
    );
    s.requests.send(Request::Media(pcm)).await.unwrap();
    let received = media.recv().await.unwrap();
    assert_eq!(
        (received.category, received.command, received.format),
        (Category::AUDIO, 6, BodyFormat::Plain)
    );
    assert_eq!(received.body, [1, 2, 3, 4]);
    s.requests.send(Request::Stop).await.unwrap();
    assert_eq!(s.session.await.unwrap().unwrap(), End::Stopped);
}

const TABLET: PictureSize = PictureSize {
    width: 2458,
    height: 1484,
};

fn invoke(id: i32, state: AppInvokeCategory) -> InvokeApp {
    InvokeApp {
        id,
        package_name: String::new(),
        invoke_state: state as i32,
        display_mode: 0,
    }
}

fn shown(state: &AppStateChanged) -> (i32, bool, i32, Option<DisplayVisibleRegion>) {
    (
        state.id,
        state.is_need_show,
        state.state,
        state.visible_region,
    )
}

#[test]
fn region_covers_the_whole_picture() {
    assert_eq!(
        visible_region(TABLET),
        DisplayVisibleRegion {
            mode: LANDSCAPE_FULL_SCREEN,
            origin_width: 2458,
            origin_height: 1484,
            left: 0,
            top: 0,
            right: 2458,
            bottom: 1484,
        }
    );
    let portrait = visible_region(PictureSize {
        width: 1080,
        height: 1920,
    });
    assert_eq!(portrait.mode, PORTRAIT_FULL_SCREEN);
    assert_eq!(
        (
            portrait.origin_width,
            portrait.origin_height,
            portrait.right,
            portrait.bottom
        ),
        (1080, 1920, 1080, 1920)
    );
    let square = visible_region(PictureSize {
        width: 1200,
        height: 1200,
    });
    assert_eq!(square.mode, LANDSCAPE_FULL_SCREEN);
}

#[test]
fn app_states_carry_the_region() {
    let mut apps = Apps::new(Some(TABLET), Display::default());
    let state = apps.invoked(&invoke(10, AppInvokeCategory::InvokeStart)).0;
    assert_eq!(
        shown(&state),
        (
            10,
            true,
            AppStateCategory::AppStateResumed as i32,
            Some(visible_region(TABLET))
        )
    );
    assert!(state.timestamp > 0);
    assert_eq!(state.is_allowed_switch_mode, Some(true));
    assert_eq!(
        describe_app_state(&state),
        "app state for app 10: resumed, full screen landscape 2458x1484"
    );
    let stopped = apps.invoked(&invoke(10, AppInvokeCategory::InvokeStop)).0;
    assert_eq!(
        shown(&stopped),
        (
            10,
            false,
            AppStateCategory::AppStateStopped as i32,
            Some(visible_region(TABLET))
        )
    );
    // Without a picture size the head unit sizes its window by the video.
    let mut unsized_apps = Apps::new(None, Display::default());
    let state = unsized_apps
        .invoked(&invoke(10, AppInvokeCategory::InvokeStart))
        .0;
    assert_eq!(state.visible_region, None);
    assert_eq!(describe_app_state(&state), "app state for app 10: resumed");
    let portrait = AppStateChanged {
        visible_region: Some(visible_region(PictureSize {
            width: 720,
            height: 1280,
        })),
        state: AppStateCategory::AppStatePaused as i32,
        ..state
    };
    assert_eq!(
        describe_app_state(&portrait),
        "app state for app 10: paused, full screen portrait 720x1280"
    );
}

#[test]
fn every_start_is_reported() {
    let mut apps = Apps::new(Some(TABLET), Display::default());
    let start = invoke(10, AppInvokeCategory::InvokeStart);
    let first = apps.invoked(&start).0;
    let again = apps.invoked(&start).0;
    assert_eq!(shown(&first), shown(&again));
}

#[test]
fn a_new_picture_size_reports_the_app_on_screen_again() {
    let small = PictureSize {
        width: 1280,
        height: 720,
    };
    let mut apps = Apps::new(Some(small), Display::default());
    // Nothing on screen: nothing to report, but later states carry the new region.
    assert!(apps.resize(Some(TABLET)).is_none());
    let start = invoke(10, AppInvokeCategory::InvokeStart);
    assert_eq!(
        apps.invoked(&start).0.visible_region,
        Some(visible_region(TABLET))
    );
    // The same size changes nothing.
    assert!(apps.resize(Some(TABLET)).is_none());
    let state = apps.resize(Some(small)).unwrap();
    assert_eq!(
        shown(&state),
        (
            10,
            true,
            AppStateCategory::AppStateResumed as i32,
            Some(visible_region(small))
        )
    );
    // A start right after is answered as well.
    assert_eq!(
        apps.invoked(&start).0.visible_region,
        Some(visible_region(small))
    );
    // Without a size, the window goes back to the video's.
    assert_eq!(apps.resize(None).unwrap().visible_region, None);
    // An app no longer on screen is not reported.
    apps.invoked(&invoke(10, AppInvokeCategory::InvokeStop));
    assert!(apps.resize(Some(TABLET)).is_none());
    // Pausing another app leaves the one on screen.
    apps.invoked(&start);
    apps.invoked(&invoke(11, AppInvokeCategory::InvokePause));
    assert_eq!(apps.resize(Some(small)).unwrap().id, 10);
}

/// Sends `InvokeApp` as the head unit does and returns the state the phone reports for it.
async fn invoke_and_report(
    control: &mut Channel,
    id: i32,
    state: AppInvokeCategory,
    display_mode: i32,
) -> AppStateChanged {
    let invoke = InvokeApp {
        id,
        package_name: String::new(),
        invoke_state: state as i32,
        display_mode,
    };
    control
        .send(car_message(
            Category::CONTROL,
            command::control::INVOKE_APP,
            Kind::SendAck,
            &invoke,
        ))
        .await
        .unwrap();
    loop {
        let message = control.recv().await.unwrap();
        if message.command == command::control::APP_STATE_CHANGED {
            break message.decode_body::<AppStateChanged>().unwrap();
        }
    }
}

fn tablet_display(layout: Layout, window_mode: Option<WindowMode>) -> Display {
    Display {
        layout,
        window_mode,
        screen: Shape::Landscape,
    }
}

fn mode_and_size(state: &AppStateChanged) -> (i32, i32, i32) {
    let region = state.visible_region.unwrap();
    (region.mode, region.origin_width, region.origin_height)
}

fn invoke_with(id: i32, state: AppInvokeCategory, display_mode: i32) -> InvokeApp {
    InvokeApp {
        display_mode,
        ..invoke(id, state)
    }
}

#[test]
fn the_window_button_switches_between_full_screen_and_the_window() {
    let mut apps = Apps::new(
        Some(TABLET),
        tablet_display(Layout::FullScreen, Some(WindowMode::Mirror)),
    );
    let (state, display) = apps.invoked(&invoke(10, AppInvokeCategory::InvokeStart));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_FULL_SCREEN, 2458, 1484));
    assert_eq!(display, None);
    // The display mode the request carries does not matter: each UPDATE switches.
    for flag in [0, 1, -1] {
        let (state, display) =
            apps.invoked(&invoke_with(10, AppInvokeCategory::InvokeUpdate, flag));
        assert_eq!(mode_and_size(&state), (LANDSCAPE_WINDOW, 2458, 1484));
        assert_eq!(state.state, AppStateCategory::AppStateResumed as i32);
        assert_eq!(state.is_allowed_switch_mode, Some(true));
        assert_eq!(
            display,
            Some(tablet_display(Layout::Window, Some(WindowMode::Mirror)))
        );
        assert_eq!(
            describe_app_state(&state),
            "app state for app 10: resumed, window landscape 2458x1484"
        );
        let (state, display) =
            apps.invoked(&invoke_with(10, AppInvokeCategory::InvokeUpdate, flag));
        assert_eq!(mode_and_size(&state), (LANDSCAPE_FULL_SCREEN, 2458, 1484));
        assert_eq!(
            display,
            Some(tablet_display(Layout::FullScreen, Some(WindowMode::Mirror)))
        );
    }
    // A start of the same app changes nothing.
    let (state, display) = apps.invoked(&invoke(10, AppInvokeCategory::InvokeStart));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_FULL_SCREEN, 2458, 1484));
    assert_eq!(display, None);
}

#[test]
fn the_window_takes_the_shape_of_the_mode_the_head_unit_starts() {
    let mut apps = Apps::new(
        Some(TABLET),
        tablet_display(Layout::Window, Some(WindowMode::Mirror)),
    );
    let (state, _) = apps.invoked(&invoke(10, AppInvokeCategory::InvokeStart));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_WINDOW, 2458, 1484));
    // The mode menu's simple launcher: a portrait window.
    let (state, display) = apps.invoked(&invoke(11, AppInvokeCategory::InvokeStart));
    assert_eq!(state.id, 11);
    assert_eq!(mode_and_size(&state), (PORTRAIT_WINDOW, 1484, 2458));
    assert_eq!(
        state
            .visible_region
            .map(|r| (r.left, r.top, r.right, r.bottom)),
        Some((0, 0, 1484, 2458))
    );
    assert_eq!(
        display,
        Some(tablet_display(Layout::Window, Some(WindowMode::Simple)))
    );
    assert_eq!(display.unwrap().shape(), Shape::Portrait);
    // The old mode's app paused or stopped: the window keeps its mode.
    for leave in [
        AppInvokeCategory::InvokePause,
        AppInvokeCategory::InvokeStop,
    ] {
        let (state, display) = apps.invoked(&invoke(10, leave));
        assert_eq!(mode_and_size(&state), (PORTRAIT_WINDOW, 1484, 2458));
        assert!(!state.is_need_show);
        assert_eq!(display, None);
    }
    // Other apps leave the window mode as it is.
    let (state, display) = apps.invoked(&invoke(12, AppInvokeCategory::InvokeStart));
    assert_eq!(mode_and_size(&state), (PORTRAIT_WINDOW, 1484, 2458));
    assert_eq!(display, None);
    // Full screen is the screen's shape in the simple launcher mode as well.
    let (state, display) = apps.invoked(&invoke(11, AppInvokeCategory::InvokeUpdate));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_FULL_SCREEN, 2458, 1484));
    assert_eq!(display.unwrap().shape(), Shape::Landscape);
    // And the mirror window again, resumed: landscape.
    apps.invoked(&invoke(11, AppInvokeCategory::InvokeUpdate));
    let (state, display) = apps.invoked(&invoke(10, AppInvokeCategory::InvokeResume));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_WINDOW, 2458, 1484));
    assert_eq!(
        display,
        Some(tablet_display(Layout::Window, Some(WindowMode::Mirror)))
    );
}

#[test]
fn the_region_turns_the_picture_the_iphone_renders() {
    let upright = PictureSize {
        width: 1484,
        height: 2458,
    };
    // In full screen, a portrait picture of the iPhone's is reported as the screen is.
    let mut apps = Apps::new(
        Some(upright),
        tablet_display(Layout::FullScreen, Some(WindowMode::Simple)),
    );
    let (state, _) = apps.invoked(&invoke(11, AppInvokeCategory::InvokeStart));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_FULL_SCREEN, 2458, 1484));
    apps.invoked(&invoke(11, AppInvokeCategory::InvokeUpdate));
    // The iPhone renders the portrait picture now: the same picture, nothing to report again.
    assert!(apps.resize(Some(upright)).is_none());
    assert!(apps.resize(Some(TABLET)).is_none());
    // Another size is reported in the window's shape.
    let state = apps
        .resize(Some(PictureSize {
            width: 1920,
            height: 720,
        }))
        .unwrap();
    assert_eq!(mode_and_size(&state), (PORTRAIT_WINDOW, 720, 1920));
}

#[test]
fn a_portrait_screen() {
    let tall = PictureSize {
        width: 1080,
        height: 1920,
    };
    let display = Display {
        layout: Layout::FullScreen,
        window_mode: Some(WindowMode::Mirror),
        screen: Shape::Portrait,
    };
    let mut apps = Apps::new(Some(tall), display);
    let (state, _) = apps.invoked(&invoke(10, AppInvokeCategory::InvokeStart));
    assert_eq!(mode_and_size(&state), (PORTRAIT_FULL_SCREEN, 1080, 1920));
    let (state, _) = apps.invoked(&invoke(10, AppInvokeCategory::InvokeUpdate));
    assert_eq!(mode_and_size(&state), (LANDSCAPE_WINDOW, 1920, 1080));
}

#[test]
fn media_drops_counted_by_stage_and_command() {
    let drops = crate::session::MediaDrops::default();
    drops.add(crate::session::MediaDrops::MEDIA_QUEUE, 5);
    drops.add(crate::session::MediaDrops::NOT_CONNECTED, 5);
    drops.add(crate::session::MediaDrops::MEDIA_QUEUE, 5);
    drops.add(crate::session::MediaDrops::MEDIA_QUEUE, 9);
    assert_eq!(
        drops.take(),
        [
            ("media queue", 5, 2),
            ("media channel not connected", 5, 1),
            ("media queue", 9, 1)
        ]
    );
    assert!(drops.take().is_empty());
}

#[tokio::test]
async fn a_socket_is_shown_while_it_is_watched() {
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let watch = Arc::new(crate::session::SocketWatch::default());
    assert_eq!(watch.with(|fd| fd), None);
    {
        let _watched = crate::session::SocketWatch::watch(&watch, &listener);
        assert_eq!(watch.with(|fd| fd), Some(listener.as_raw_fd()));
    }
    assert_eq!(watch.with(|fd| fd), None);
}
