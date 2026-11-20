// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's
// transport/Iap2WirelessControlClient.kt (DiPlay commit 615b32c).

//! The accessory's side of wireless CarPlay on an iAP2 session: identification, MFi authentication and the
//! subscriptions, then, as the iPhone asks, the access point to join (0x5703) and the AirPlay receiver to reach
//! (0x4301). It keeps running until the link ends or the host stops it, passing on every other message.

use std::time::{Duration, Instant};

use crate::error::Error;
use crate::identification::{self, Accessory, Wireless};
use crate::messages::{self, id, Network, Receiver};
use crate::mfi::{self, Authenticator};
use crate::session::Session;
use crate::wire::Frame;

/// How long identification and authentication may take together.
pub const BRING_UP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one wait for a message lasts before the host is asked whether to go on.
const POLL: Duration = Duration::from_millis(500);
/// How long one message may take to send.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the access point is sent, before and after the iPhone names its transport.
const MAX_WIFI_CONFIGURATIONS: u32 = 5;
const MAX_WIFI_CONFIGURATIONS_AFTER_TRANSPORT: u32 = 2;

/// What the accessory sets up wireless CarPlay with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub accessory: Accessory,
    /// The Bluetooth adapter the iPhone paired with.
    pub bluetooth_address: [u8; 6],
    pub network: Network,
    pub receiver: Receiver,
}

/// What the run tells the host as it happens.
#[derive(Debug)]
pub enum Event<'a> {
    /// The accessory is identified and authenticated, and the subscriptions are sent.
    Ready,
    /// The iPhone was sent the access point to join.
    WifiConfigurationSent,
    /// The iPhone was asked to start CarPlay, in answer to its CarPlay availability.
    StartSessionSent,
    /// Any other message from the iPhone, such as a now-playing update.
    Message(&'a Frame),
}

/// The host of a run.
pub trait Host {
    fn event(&self, event: Event<'_>);

    /// A line worth logging.
    fn log(&self, _message: &str) {}

    /// Asked at least every half second; the run stops once it says no.
    fn keep_running(&self) -> bool {
        true
    }
}

/// How a run ended, and what it did.
#[derive(Debug)]
pub struct Outcome {
    /// Why the link ended; `None` when the host stopped the run.
    pub link_ended: Option<Error>,
    pub wifi_configurations_sent: u32,
    pub start_sessions_sent: u32,
    /// Whether the iPhone named the transport it uses (0x4e0e).
    pub transport_notified: bool,
    /// Whether the iPhone said wireless CarPlay is available (0x4e0d).
    pub wireless_available: bool,
}

/// Runs wireless CarPlay on a session. Bring-up failures are errors; once the accessory is ready the run ends
/// with an [`Outcome`] when the link ends or the host stops it.
pub fn run(
    session: &Session,
    setup: &Setup,
    authenticator: &dyn Authenticator,
    host: &dyn Host,
) -> Result<Outcome, Error> {
    let log = |message: &str| host.log(message);
    let deadline = Instant::now() + BRING_UP_TIMEOUT;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let wireless = Wireless {
        bluetooth_address: setup.bluetooth_address,
        ssid: setup.network.ssid.clone(),
    };
    identification::identify(session, &setup.accessory, &wireless, remaining(), &log)?;
    host.log("iAP2 identification accepted");
    mfi::authenticate(session, authenticator, remaining(), &log)?;
    host.log("iAP2 authentication accepted");
    for subscription in messages::subscriptions() {
        session.send(&subscription, SEND_TIMEOUT)?;
    }
    host.event(Event::Ready);

    let wifi_configuration = messages::accessory_wifi_configuration(&setup.network)?;
    let start_session = messages::carplay_start_session(&setup.network, &setup.receiver)?;
    let mut outcome = Outcome {
        link_ended: None,
        wifi_configurations_sent: 0,
        start_sessions_sent: 0,
        transport_notified: false,
        wireless_available: false,
    };
    let mut before_transport = 0;
    let mut after_transport = 0;
    loop {
        if !host.keep_running() {
            return Ok(outcome);
        }
        let frame = match session.recv(POLL) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(error) => {
                outcome.link_ended = Some(error);
                return Ok(outcome);
            }
        };
        let send = |frame: &Frame| session.send(frame, SEND_TIMEOUT);
        let result = match frame.message_id() {
            id::REQUEST_ACCESSORY_WIFI_CONFIGURATION => {
                let (sent, limit) = if outcome.transport_notified {
                    (
                        &mut after_transport,
                        MAX_WIFI_CONFIGURATIONS_AFTER_TRANSPORT,
                    )
                } else {
                    (&mut before_transport, MAX_WIFI_CONFIGURATIONS)
                };
                if *sent >= limit {
                    host.log("iAP2 Wi-Fi configuration asked for again, not sent");
                    Ok(())
                } else {
                    *sent += 1;
                    send(&wifi_configuration).map(|()| {
                        outcome.wifi_configurations_sent += 1;
                        host.event(Event::WifiConfigurationSent);
                    })
                }
            }
            id::CARPLAY_AVAILABILITY => {
                match messages::carplay_availability(&frame) {
                    Ok(availability) => host.log(&format!(
                        "iAP2 CarPlay availability: wired {:?}, wireless {:?}",
                        availability.wired, availability.wireless
                    )),
                    Err(error) => host.log(&format!("iAP2 CarPlay availability not read: {error}")),
                }
                send(&start_session).map(|()| {
                    outcome.start_sessions_sent += 1;
                    host.event(Event::StartSessionSent);
                })
            }
            id::WIRELESS_CARPLAY_UPDATE => {
                match messages::wireless_carplay_available(&frame) {
                    Ok(available) => {
                        outcome.wireless_available |= available;
                        host.log(&format!("iAP2 wireless CarPlay available: {available}"));
                    }
                    Err(error) => {
                        host.log(&format!("iAP2 wireless CarPlay update not read: {error}"))
                    }
                }
                Ok(())
            }
            id::DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION => {
                outcome.transport_notified = true;
                if after_transport >= MAX_WIFI_CONFIGURATIONS_AFTER_TRANSPORT {
                    Ok(())
                } else {
                    after_transport += 1;
                    host.log("iAP2 transport named; Wi-Fi configuration sent again");
                    send(&wifi_configuration).map(|()| {
                        outcome.wifi_configurations_sent += 1;
                        host.event(Event::WifiConfigurationSent);
                    })
                }
            }
            _ => {
                host.event(Event::Message(&frame));
                Ok(())
            }
        };
        if let Err(error) = result {
            outcome.link_ended = Some(error);
            return Ok(outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use p256::ecdsa::signature::hazmat::PrehashVerifier;
    use p256::ecdsa::{Signature, SigningKey};
    use p256::pkcs8::DecodePrivateKey;

    use super::*;
    use crate::identification::tests::accessory;
    use crate::messages::Security;
    use crate::mfi::tests::{KEY, P7B};
    use crate::mfi::LocalAuthenticator;
    use crate::session::Transport;
    use crate::stream::testing::pipe;
    use crate::wire::Parameters;

    const WAIT: Duration = Duration::from_secs(5);

    fn setup() -> Setup {
        Setup {
            accessory: accessory(),
            bluetooth_address: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            network: Network {
                ssid: "LIVI".into(),
                passphrase: "secret123".into(),
                channel: 36,
                security: Security::Wpa3Transition,
                bssid: None,
            },
            receiver: Receiver {
                addresses: vec!["192.168.2.1".into()],
                port: 49152,
                device_id: "dev-1".into(),
                public_key: "aabbcc".into(),
                source_version: "1.0".into(),
            },
        }
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
        stop: AtomicBool,
    }

    impl Host for Recorder {
        fn event(&self, event: Event<'_>) {
            let text = match event {
                Event::Message(frame) => format!("message 0x{:04x}", frame.message_id()),
                other => format!("{other:?}"),
            };
            self.events.lock().unwrap().push(text);
        }

        fn keep_running(&self) -> bool {
            !self.stop.load(Ordering::Acquire)
        }
    }

    fn message(id: u16, parameters: Parameters) -> Frame {
        Frame::with_parameters(id, &parameters).unwrap()
    }

    fn expect(phone: &Session, id: u16) -> Frame {
        let frame = phone
            .recv(WAIT)
            .unwrap()
            .expect("a message from the accessory");
        assert_eq!(frame.message_id(), id, "expected 0x{id:04x}, got {frame:?}");
        frame
    }

    /// The iPhone's side, scripted: identification with one correctable rejection, MFi, then CarPlay.
    fn phone_script(phone: &Session, recorder: &Recorder) {
        phone.await_ready(WAIT).unwrap();
        phone
            .send(&message(id::START_IDENTIFICATION, Parameters::new()), WAIT)
            .unwrap();
        expect(phone, id::IDENTIFICATION_INFORMATION);
        let rejected = Parameters::new()
            .u16_list(6, [0x4301])
            .u16_list(7, [0x4300]);
        phone
            .send(&message(id::IDENTIFICATION_REJECTED, rejected), WAIT)
            .unwrap();
        let retried = expect(phone, id::IDENTIFICATION_INFORMATION);
        let sent = retried
            .parameters()
            .unwrap()
            .first(6)
            .unwrap()
            .as_u16_list()
            .unwrap();
        assert!(!sent.contains(&0x4301));
        phone
            .send(
                &message(id::IDENTIFICATION_ACCEPTED, Parameters::new()),
                WAIT,
            )
            .unwrap();

        phone
            .send(
                &message(id::REQUEST_AUTHENTICATION_CERTIFICATE, Parameters::new()),
                WAIT,
            )
            .unwrap();
        let certificate = expect(phone, id::AUTHENTICATION_CERTIFICATE);
        assert_eq!(
            certificate
                .parameters()
                .unwrap()
                .first(0)
                .unwrap()
                .payload(),
            P7B
        );
        let challenge = [0x33u8; 32];
        phone
            .send(
                &message(
                    id::REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
                    Parameters::new().bytes(0, &challenge),
                ),
                WAIT,
            )
            .unwrap();
        let response = expect(phone, id::AUTHENTICATION_RESPONSE);
        let signature =
            Signature::from_slice(response.parameters().unwrap().first(0).unwrap().payload())
                .unwrap();
        let key = SigningKey::from_pkcs8_der(KEY).unwrap();
        assert!(key
            .verifying_key()
            .verify_prehash(&challenge, &signature)
            .is_ok());
        phone
            .send(
                &message(id::AUTHENTICATION_SUCCEEDED, Parameters::new()),
                WAIT,
            )
            .unwrap();
        for subscription in [0x5000, 0xae00, 0x4157, 0x4154] {
            expect(phone, subscription);
        }

        phone
            .send(
                &message(id::REQUEST_ACCESSORY_WIFI_CONFIGURATION, Parameters::new()),
                WAIT,
            )
            .unwrap();
        expect(phone, id::ACCESSORY_WIFI_CONFIGURATION);
        let available = Parameters::new().group(1, Parameters::new().u8(0, 1));
        phone
            .send(&message(id::CARPLAY_AVAILABILITY, available), WAIT)
            .unwrap();
        let start = expect(phone, id::CARPLAY_START_SESSION);
        assert_eq!(
            start,
            messages::carplay_start_session(&setup().network, &setup().receiver).unwrap()
        );
        phone
            .send(
                &message(id::WIRELESS_CARPLAY_UPDATE, Parameters::new().u8(0, 1)),
                WAIT,
            )
            .unwrap();
        phone
            .send(
                &message(
                    id::DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
                    Parameters::new().string(0, "AA:BB"),
                ),
                WAIT,
            )
            .unwrap();
        expect(phone, id::ACCESSORY_WIFI_CONFIGURATION);
        phone
            .send(&message(id::NOW_PLAYING_UPDATE, Parameters::new()), WAIT)
            .unwrap();
        let deadline = Instant::now() + WAIT;
        while !recorder
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e == "message 0x5001")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_full_wireless_bring_up() {
        let (a, b) = pipe();
        let accessory = Session::open(a, Transport::Bluetooth, |_| {});
        let phone = Arc::new(Session::open(b, Transport::Tunnel, |_| {}));
        let recorder = Arc::new(Recorder::default());
        let script = {
            let (phone, recorder) = (phone.clone(), recorder.clone());
            std::thread::spawn(move || {
                phone_script(&phone, &recorder);
                phone.close();
            })
        };
        let authenticator = LocalAuthenticator::new(KEY, P7B).unwrap();
        let outcome = run(&accessory, &setup(), &authenticator, recorder.as_ref()).unwrap();
        script.join().unwrap();
        assert!(outcome.link_ended.is_some());
        assert_eq!(outcome.wifi_configurations_sent, 2);
        assert_eq!(outcome.start_sessions_sent, 1);
        assert!(outcome.transport_notified);
        assert!(outcome.wireless_available);
        assert_eq!(
            *recorder.events.lock().unwrap(),
            [
                "Ready",
                "WifiConfigurationSent",
                "StartSessionSent",
                "WifiConfigurationSent",
                "message 0x5001"
            ]
        );
    }

    #[test]
    fn a_refused_authentication_is_an_error() {
        let (a, b) = pipe();
        let accessory = Session::open(a, Transport::Bluetooth, |_| {});
        let phone = Arc::new(Session::open(b, Transport::Tunnel, |_| {}));
        let script = {
            let phone = phone.clone();
            std::thread::spawn(move || {
                phone.await_ready(WAIT).unwrap();
                phone
                    .send(&message(id::START_IDENTIFICATION, Parameters::new()), WAIT)
                    .unwrap();
                expect(&phone, id::IDENTIFICATION_INFORMATION);
                phone
                    .send(
                        &message(id::IDENTIFICATION_ACCEPTED, Parameters::new()),
                        WAIT,
                    )
                    .unwrap();
                phone
                    .send(&message(id::AUTHENTICATION_FAILED, Parameters::new()), WAIT)
                    .unwrap();
                std::thread::sleep(Duration::from_millis(200));
            })
        };
        let authenticator = LocalAuthenticator::new(KEY, P7B).unwrap();
        let result = run(&accessory, &setup(), &authenticator, &Recorder::default());
        script.join().unwrap();
        assert!(
            matches!(result, Err(Error::Authentication(_))),
            "{result:?}"
        );
    }

    #[test]
    fn the_host_stops_a_running_link() {
        let (a, b) = pipe();
        let accessory = Session::open(a, Transport::Bluetooth, |_| {});
        let phone = Arc::new(Session::open(b, Transport::Tunnel, |_| {}));
        let recorder = Arc::new(Recorder::default());
        let script = {
            let (phone, recorder) = (phone.clone(), recorder.clone());
            std::thread::spawn(move || {
                phone.await_ready(WAIT).unwrap();
                phone
                    .send(&message(id::START_IDENTIFICATION, Parameters::new()), WAIT)
                    .unwrap();
                expect(&phone, id::IDENTIFICATION_INFORMATION);
                phone
                    .send(
                        &message(id::IDENTIFICATION_ACCEPTED, Parameters::new()),
                        WAIT,
                    )
                    .unwrap();
                phone
                    .send(
                        &message(id::AUTHENTICATION_SUCCEEDED, Parameters::new()),
                        WAIT,
                    )
                    .unwrap();
                for _ in 0..4 {
                    phone.recv(WAIT).unwrap();
                }
                recorder.stop.store(true, Ordering::Release);
            })
        };
        let authenticator = LocalAuthenticator::new(KEY, P7B).unwrap();
        let outcome = run(&accessory, &setup(), &authenticator, recorder.as_ref()).unwrap();
        script.join().unwrap();
        assert!(outcome.link_ended.is_none());
        assert!(!accessory.is_ended());
    }
}
