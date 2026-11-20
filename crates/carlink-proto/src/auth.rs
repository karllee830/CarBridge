// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The phone's side of authentication on the auth channel, and the session key it yields.
//!
//! The phone sends an [`AuthRequest`], the head unit answers with an [`AuthResponse`], and the phone sends an
//! [`AuthConfirm`] encrypted with the new session key. A first pairing proves knowledge of the PIN the head unit
//! shows; a reconnection relies on the long-term keys both sides stored at the first pairing.
//!
//! All keys are P-256. Public keys travel as 64 bytes, X then Y, without the SEC1 prefix; signatures are DER-encoded
//! ECDSA over SHA-256.

use std::fmt;
use std::net::Ipv4Addr;

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::ecdh::EphemeralSecret;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::{EncodedPoint, FromEncodedPoint, ToEncodedPoint};
use p256::NistP256;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

use crate::pb::{AuthConfirm, AuthRequest, AuthResponse};
use crate::record::SessionKey;
use crate::{Error, PROTOCOL_VERSION};

/// Length of the `random` of each side.
pub const RANDOM_LEN: usize = 32;
/// Length of a public key on the wire.
pub const PUBLIC_KEY_LEN: usize = 64;

const SESSION_KEY_INFO: &[u8] = b"session_key";

/// A P-256 public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(p256::PublicKey);

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey(")?;
        for byte in &self.to_wire()[..8] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "..)")
    }
}

impl PublicKey {
    /// Reads a key in the wire form: 64 bytes, X then Y. The uncompressed SEC1 form, with a leading 0x04, is also
    /// accepted.
    pub fn from_wire(bytes: &[u8]) -> Result<PublicKey, Error> {
        let point = match bytes.len() {
            PUBLIC_KEY_LEN => {
                let (x, y) = bytes.split_at(32);
                EncodedPoint::<NistP256>::from_affine_coordinates(x.into(), y.into(), false)
            }
            65 if bytes[0] == 0x04 => {
                EncodedPoint::<NistP256>::from_bytes(bytes).map_err(|_| Error::PublicKey)?
            }
            _ => return Err(Error::PublicKey),
        };
        Option::from(p256::PublicKey::from_encoded_point(&point))
            .map(PublicKey)
            .ok_or(Error::PublicKey)
    }

    /// The key in the wire form: 64 bytes, X then Y.
    pub fn to_wire(&self) -> [u8; PUBLIC_KEY_LEN] {
        let point = self.0.to_encoded_point(false);
        let mut out = [0u8; PUBLIC_KEY_LEN];
        out.copy_from_slice(&point.as_bytes()[1..]);
        out
    }

    /// Checks a DER-encoded ECDSA signature over the concatenation of `parts`. Signatures with a high S value are
    /// accepted too.
    pub fn verify(&self, parts: &[&[u8]], signature: &[u8]) -> Result<(), Error> {
        let signature = Signature::from_der(signature).map_err(|_| Error::Signature)?;
        let signature = signature.normalize_s().unwrap_or(signature);
        VerifyingKey::from(&self.0)
            .verify(&parts.concat(), &signature)
            .map_err(|_| Error::Signature)
    }
}

/// A long-term signing key: the phone auth key, kept across sessions.
#[derive(Clone)]
pub struct AuthKey(SigningKey);

impl fmt::Debug for AuthKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AuthKey({:?})", self.public_key())
    }
}

impl AuthKey {
    /// Makes a new random key.
    pub fn generate() -> AuthKey {
        AuthKey(SigningKey::random(&mut OsRng))
    }

    /// Restores a key from the 32-byte private scalar [`AuthKey::to_bytes`] gave.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<AuthKey, Error> {
        SigningKey::from_bytes(bytes.into())
            .map(AuthKey)
            .map_err(|_| Error::PrivateKey)
    }

    /// The private scalar, for storage.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes().into()
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey(p256::PublicKey::from(self.0.verifying_key()))
    }

    /// Signs the concatenation of `parts`; returns the DER-encoded signature.
    pub fn sign(&self, parts: &[&[u8]]) -> Vec<u8> {
        let signature: Signature = self.0.sign(&parts.concat());
        signature.to_der().as_bytes().to_vec()
    }
}

/// HMAC-SHA256 over the concatenation of `data`, keyed with SHA-256 of the concatenation of `key`.
fn pin_mac(key: &[&[u8]], data: &[&[u8]]) -> Hmac<Sha256> {
    let mut digest = Sha256::new();
    for part in key {
        digest.update(part);
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(&digest.finalize()).expect("HMAC takes any key length");
    for part in data {
        mac.update(part);
    }
    mac
}

/// `authPkHmac` of a first-pairing request: over the phone `authPk`, keyed with SHA-256(phone `random` + PIN).
pub fn request_hmac(phone_random: &[u8], pin: &str, phone_auth_pk: &[u8]) -> [u8; 32] {
    pin_mac(&[phone_random, pin.as_bytes()], &[phone_auth_pk])
        .finalize()
        .into_bytes()
        .into()
}

/// `authPkHmac` of a first-pairing response: over car `authPk` + phone `authPk`, keyed with SHA-256(car `random` +
/// phone `random` + PIN).
pub fn response_hmac(
    car_random: &[u8],
    phone_random: &[u8],
    pin: &str,
    car_auth_pk: &[u8],
    phone_auth_pk: &[u8],
) -> [u8; 32] {
    pin_mac(
        &[car_random, phone_random, pin.as_bytes()],
        &[car_auth_pk, phone_auth_pk],
    )
    .finalize()
    .into_bytes()
    .into()
}

/// The session key: HKDF-SHA256 of the X coordinate of the ECDH result, salted with SHA-256(phone `random` + car
/// `random`), info `session_key`, 16 bytes.
pub fn derive_session_key(shared_x: &[u8], phone_random: &[u8], car_random: &[u8]) -> [u8; 16] {
    let salt = Sha256::new()
        .chain_update(phone_random)
        .chain_update(car_random)
        .finalize();
    let mut key = [0u8; 16];
    Hkdf::<Sha256>::new(Some(&salt), shared_x)
        .expand(SESSION_KEY_INFO, &mut key)
        .expect("16 bytes is a valid HKDF-SHA256 length");
    key
}

/// The `AuthConfirm` the phone sends once authenticated: its IPv4 address on the head unit's network followed by
/// `:0`, encrypted with the session key.
pub fn auth_confirm(key: &SessionKey, phone_address: Ipv4Addr) -> AuthConfirm {
    AuthConfirm {
        cipher: key.seal(format!("{phone_address}:0").as_bytes()),
    }
}

/// What the phone tells the head unit about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhoneInfo {
    /// The identity the phone gave over BLE.
    pub id: String,
    pub model: String,
    /// The work modes the phone offers, as bits; the head unit assumes normal cast (1) when absent.
    pub work_modes: Option<i32>,
    /// Shown on the head unit in the app and window work modes.
    pub product_name: Option<String>,
    /// The user agreed to replace the phone the head unit used last.
    pub user_confirmed: bool,
}

/// Why authentication did not produce a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Result 1: the head unit does not know this phone; pair again with the PIN.
    UnknownPhone,
    /// Result 2: another phone was the last one connected; ask the user and retry with `user_confirmed`.
    OtherPhoneLast,
    /// Result 3: wrong PIN, bad signature or no PIN; forget the stored car auth key and pair again.
    Rejected,
    /// Result 4: no work mode in common.
    NoCommonWorkMode,
    /// A result the protocol does not define.
    UnknownResult(i32),
    /// The response's `authPkHmac` does not match the PIN.
    WrongPin,
    /// A field of the response is missing or malformed, or its signature does not verify.
    BadResponse(Error),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::UnknownPhone => f.write_str("head unit does not know this phone"),
            AuthError::OtherPhoneLast => f.write_str("head unit last used another phone"),
            AuthError::Rejected => f.write_str("head unit rejected the authentication"),
            AuthError::NoCommonWorkMode => f.write_str("no work mode in common"),
            AuthError::UnknownResult(result) => write!(f, "unknown authentication result {result}"),
            AuthError::WrongPin => f.write_str("response does not match the PIN"),
            AuthError::BadResponse(err) => write!(f, "bad authentication response: {err}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<Error> for AuthError {
    fn from(err: Error) -> Self {
        AuthError::BadResponse(err)
    }
}

/// An accepted authentication.
#[derive(Debug)]
pub struct Session {
    pub key: SessionKey,
    /// The car auth key: to store for reconnections after a first pairing.
    pub car_auth_key: PublicKey,
    /// The work mode the head unit chose.
    pub work_mode: Option<i32>,
    pub day_or_night_mode: Option<i32>,
    pub screen_info: Option<ScreenInfo>,
}

/// One authentication attempt, from request to session.
pub struct Handshake {
    agreement: EphemeralSecret,
    request: AuthRequest,
    phone_auth_pk: [u8; PUBLIC_KEY_LEN],
    /// The PIN of a first pairing, or the stored car auth key of a reconnection.
    proof: Proof,
}

enum Proof {
    Pin(String),
    CarKey(PublicKey),
}

impl fmt::Debug for Handshake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.proof {
            Proof::Pin(_) => "first pairing",
            Proof::CarKey(_) => "reconnection",
        };
        f.debug_struct("Handshake")
            .field("mode", &mode)
            .finish_non_exhaustive()
    }
}

impl Handshake {
    /// A first pairing, with the PIN the head unit shows.
    pub fn first_pairing(auth_key: &AuthKey, pin: &str, phone: &PhoneInfo) -> Handshake {
        Handshake::start(auth_key, Proof::Pin(pin.to_owned()), phone)
    }

    /// A reconnection to a head unit whose car auth key was stored at the first pairing.
    pub fn reconnection(
        auth_key: &AuthKey,
        car_auth_key: PublicKey,
        phone: &PhoneInfo,
    ) -> Handshake {
        Handshake::start(auth_key, Proof::CarKey(car_auth_key), phone)
    }

    fn start(auth_key: &AuthKey, proof: Proof, phone: &PhoneInfo) -> Handshake {
        Handshake::with_secrets(
            auth_key,
            proof,
            phone,
            EphemeralSecret::random(&mut OsRng),
            random_bytes(),
        )
    }

    fn with_secrets(
        auth_key: &AuthKey,
        proof: Proof,
        phone: &PhoneInfo,
        agreement: EphemeralSecret,
        random: [u8; RANDOM_LEN],
    ) -> Handshake {
        let phone_auth_pk = auth_key.public_key().to_wire();
        let agreement_pk = PublicKey(agreement.public_key()).to_wire();
        let (auth_pk, auth_pk_hmac) = match &proof {
            Proof::Pin(pin) => (
                phone_auth_pk.to_vec(),
                request_hmac(&random, pin, &phone_auth_pk).to_vec(),
            ),
            Proof::CarKey(_) => (Vec::new(), Vec::new()),
        };
        let request = AuthRequest {
            version: PROTOCOL_VERSION,
            auth_pk,
            auth_pk_hmac,
            agreement_pk_sig: auth_key.sign(&[&agreement_pk, &random]),
            agreement_pk: agreement_pk.to_vec(),
            random: random.to_vec(),
            id: phone.id.as_bytes().to_vec(),
            model: phone.model.clone(),
            user_confirmed: phone.user_confirmed,
            work_modes: phone.work_modes,
            product_name: phone.product_name.clone(),
        };
        Handshake {
            agreement,
            request,
            phone_auth_pk,
            proof,
        }
    }

    /// The `AuthRequest` to send.
    pub fn request(&self) -> &AuthRequest {
        &self.request
    }

    /// Checks the head unit's response and derives the session key.
    pub fn finish(self, response: &AuthResponse) -> Result<Session, AuthError> {
        match response.result {
            0 => {}
            1 => return Err(AuthError::UnknownPhone),
            2 => return Err(AuthError::OtherPhoneLast),
            3 => return Err(AuthError::Rejected),
            4 => return Err(AuthError::NoCommonWorkMode),
            other => return Err(AuthError::UnknownResult(other)),
        }
        let phone_random = &self.request.random;
        let car_random = &response.random;
        if car_random.len() != RANDOM_LEN {
            return Err(AuthError::BadResponse(Error::Field("random")));
        }
        let car_auth_key = match &self.proof {
            Proof::Pin(pin) => {
                let car_auth_key = PublicKey::from_wire(&response.auth_pk)?;
                pin_mac(
                    &[car_random, phone_random, pin.as_bytes()],
                    &[&response.auth_pk, &self.phone_auth_pk],
                )
                .verify_slice(&response.auth_pk_hmac)
                .map_err(|_| AuthError::WrongPin)?;
                car_auth_key
            }
            Proof::CarKey(key) => *key,
        };
        let car_agreement = PublicKey::from_wire(&response.agreement_pk)?;
        car_auth_key.verify(
            &[
                &response.agreement_pk,
                &self.request.agreement_pk,
                car_random,
                phone_random,
            ],
            &response.agreement_pk_sig,
        )?;
        let shared = self.agreement.diffie_hellman(&car_agreement.0);
        let key = derive_session_key(shared.raw_secret_bytes(), phone_random, car_random);
        Ok(Session {
            key: SessionKey::new(&key),
            car_auth_key,
            work_mode: response.work_modes,
            day_or_night_mode: response.day_or_night_mode,
            screen_info: response.screen_info.as_deref().and_then(ScreenInfo::parse),
        })
    }
}

fn random_bytes() -> [u8; RANDOM_LEN] {
    let mut random = [0u8; RANDOM_LEN];
    OsRng.fill_bytes(&mut random);
    random
}

/// `AuthResponse.screenInfo`: the size the cast picture is shown at, the screen's density, and the small window's
/// size when the head unit has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenInfo {
    pub width: u32,
    pub height: u32,
    pub dpi: u32,
    pub window: Option<(u32, u32)>,
}

impl ScreenInfo {
    /// Parses `W_H_D` or `W_H_D_w_h`.
    pub fn parse(text: &str) -> Option<ScreenInfo> {
        let fields: Vec<u32> = text
            .split('_')
            .map(|f| f.trim().parse().ok())
            .collect::<Option<_>>()?;
        match fields[..] {
            [width, height, dpi] => Some(ScreenInfo {
                width,
                height,
                dpi,
                window: None,
            }),
            [width, height, dpi, w, h] => Some(ScreenInfo {
                width,
                height,
                dpi,
                window: Some((w, h)),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: &str = "123456";

    fn phone() -> PhoneInfo {
        PhoneInfo {
            id: "phone-1".into(),
            model: "CarBridge".into(),
            work_modes: Some(1),
            ..PhoneInfo::default()
        }
    }

    /// The head unit's side, written from the protocol description.
    struct HeadUnit {
        auth_key: AuthKey,
        agreement: EphemeralSecret,
        random: [u8; RANDOM_LEN],
        stored_phone_key: Option<PublicKey>,
    }

    impl HeadUnit {
        fn new() -> HeadUnit {
            HeadUnit {
                auth_key: AuthKey::generate(),
                agreement: EphemeralSecret::random(&mut OsRng),
                random: random_bytes(),
                stored_phone_key: None,
            }
        }

        /// Answers a request; returns the response and the session key it derived.
        fn answer(&mut self, request: &AuthRequest, pin: &str) -> (AuthResponse, Option<[u8; 16]>) {
            let mut response = AuthResponse {
                version: PROTOCOL_VERSION,
                ..AuthResponse::default()
            };
            let phone_key = if request.auth_pk.is_empty() {
                match self.stored_phone_key {
                    Some(key) => key,
                    None => {
                        return (
                            AuthResponse {
                                result: 1,
                                ..response
                            },
                            None,
                        )
                    }
                }
            } else {
                let expected = request_hmac(&request.random, pin, &request.auth_pk);
                if request.auth_pk_hmac != expected {
                    return (
                        AuthResponse {
                            result: 3,
                            ..response
                        },
                        None,
                    );
                }
                PublicKey::from_wire(&request.auth_pk).unwrap()
            };
            if phone_key
                .verify(
                    &[&request.agreement_pk, &request.random],
                    &request.agreement_pk_sig,
                )
                .is_err()
            {
                return (
                    AuthResponse {
                        result: 3,
                        ..response
                    },
                    None,
                );
            }
            self.stored_phone_key = Some(phone_key);
            let car_auth_pk = self.auth_key.public_key().to_wire();
            let car_agreement_pk = PublicKey(self.agreement.public_key()).to_wire();
            if !request.auth_pk.is_empty() {
                response.auth_pk = car_auth_pk.to_vec();
                response.auth_pk_hmac = response_hmac(
                    &self.random,
                    &request.random,
                    pin,
                    &car_auth_pk,
                    &request.auth_pk,
                )
                .to_vec();
            }
            response.agreement_pk_sig = self.auth_key.sign(&[
                &car_agreement_pk,
                &request.agreement_pk,
                &self.random,
                &request.random,
            ]);
            response.agreement_pk = car_agreement_pk.to_vec();
            response.random = self.random.to_vec();
            response.work_modes = Some(1);
            response.screen_info = Some("1920_720_320".into());
            let phone_agreement = PublicKey::from_wire(&request.agreement_pk).unwrap();
            let shared = self.agreement.diffie_hellman(&phone_agreement.0);
            let key = derive_session_key(shared.raw_secret_bytes(), &request.random, &self.random);
            (response, Some(key))
        }
    }

    fn key_bytes(key: &SessionKey, other: [u8; 16]) -> bool {
        SessionKey::new(&other).open(&key.seal(b"probe")).is_ok()
    }

    #[test]
    fn first_pairing_then_reconnection() {
        let phone_key = AuthKey::generate();
        let mut car = HeadUnit::new();

        let handshake = Handshake::first_pairing(&phone_key, PIN, &phone());
        let request = handshake.request().clone();
        assert_eq!(request.version, 1793);
        assert_eq!(request.auth_pk.len(), PUBLIC_KEY_LEN);
        assert_eq!(request.agreement_pk.len(), PUBLIC_KEY_LEN);
        assert_eq!(request.random.len(), RANDOM_LEN);
        assert_eq!(request.id, b"phone-1");
        let (response, car_key) = car.answer(&request, PIN);
        assert_eq!(response.result, 0);
        let session = handshake.finish(&response).unwrap();
        assert!(key_bytes(&session.key, car_key.unwrap()));
        assert_eq!(session.car_auth_key, car.auth_key.public_key());
        assert_eq!(
            session.screen_info,
            Some(ScreenInfo {
                width: 1920,
                height: 720,
                dpi: 320,
                window: None
            })
        );

        car.agreement = EphemeralSecret::random(&mut OsRng);
        car.random = random_bytes();
        let handshake = Handshake::reconnection(&phone_key, session.car_auth_key, &phone());
        assert!(handshake.request().auth_pk.is_empty());
        assert!(handshake.request().auth_pk_hmac.is_empty());
        let (response, car_key) = car.answer(handshake.request(), "");
        assert_eq!(response.result, 0);
        assert!(response.auth_pk.is_empty());
        let session = handshake.finish(&response).unwrap();
        assert!(key_bytes(&session.key, car_key.unwrap()));
    }

    #[test]
    fn wrong_pin_rejected_by_head_unit() {
        let mut car = HeadUnit::new();
        let handshake = Handshake::first_pairing(&AuthKey::generate(), "000000", &phone());
        let (response, _) = car.answer(handshake.request(), PIN);
        assert_eq!(
            handshake.finish(&response).unwrap_err(),
            AuthError::Rejected
        );
    }

    #[test]
    fn response_with_other_pin_fails() {
        let mut car = HeadUnit::new();
        let handshake = Handshake::first_pairing(&AuthKey::generate(), PIN, &phone());
        let (mut response, _) = car.answer(handshake.request(), PIN);
        let car_auth_pk = car.auth_key.public_key().to_wire();
        response.auth_pk_hmac = response_hmac(
            &car.random,
            &handshake.request().random,
            "654321",
            &car_auth_pk,
            &handshake.request().auth_pk,
        )
        .to_vec();
        assert_eq!(
            handshake.finish(&response).unwrap_err(),
            AuthError::WrongPin
        );
    }

    #[test]
    fn reconnection_with_other_car_key_fails() {
        let phone_key = AuthKey::generate();
        let mut car = HeadUnit::new();
        car.stored_phone_key = Some(phone_key.public_key());
        let handshake =
            Handshake::reconnection(&phone_key, AuthKey::generate().public_key(), &phone());
        let (response, _) = car.answer(handshake.request(), "");
        assert_eq!(
            handshake.finish(&response).unwrap_err(),
            AuthError::BadResponse(Error::Signature)
        );
    }

    #[test]
    fn results() {
        for (result, error) in [
            (1, AuthError::UnknownPhone),
            (2, AuthError::OtherPhoneLast),
            (3, AuthError::Rejected),
            (4, AuthError::NoCommonWorkMode),
            (9, AuthError::UnknownResult(9)),
        ] {
            let handshake = Handshake::first_pairing(&AuthKey::generate(), PIN, &phone());
            let response = AuthResponse {
                result,
                ..AuthResponse::default()
            };
            assert_eq!(handshake.finish(&response).unwrap_err(), error);
        }
    }

    #[test]
    fn request_signature_covers_agreement_key_and_random() {
        let phone_key = AuthKey::generate();
        let request = Handshake::first_pairing(&phone_key, PIN, &phone())
            .request()
            .clone();
        let pk = phone_key.public_key();
        assert!(pk
            .verify(
                &[&request.agreement_pk, &request.random],
                &request.agreement_pk_sig
            )
            .is_ok());
        assert!(pk
            .verify(
                &[&request.random, &request.agreement_pk],
                &request.agreement_pk_sig
            )
            .is_err());
    }

    #[test]
    fn public_key_wire_forms() {
        let pk = AuthKey::generate().public_key();
        let wire = pk.to_wire();
        assert_eq!(PublicKey::from_wire(&wire), Ok(pk));
        let mut sec1 = vec![0x04];
        sec1.extend_from_slice(&wire);
        assert_eq!(PublicKey::from_wire(&sec1), Ok(pk));
        assert_eq!(PublicKey::from_wire(&wire[..63]), Err(Error::PublicKey));
        assert_eq!(PublicKey::from_wire(&[0u8; 64]), Err(Error::PublicKey));
    }

    #[test]
    fn auth_key_storage() {
        let key = AuthKey::generate();
        let restored = AuthKey::from_bytes(&key.to_bytes()).unwrap();
        assert_eq!(restored.public_key(), key.public_key());
        assert_eq!(
            AuthKey::from_bytes(&[0; 32]).unwrap_err(),
            Error::PrivateKey
        );
    }

    #[test]
    fn high_s_signature_verifies() {
        let key = AuthKey::generate();
        let signature: Signature = key.0.sign(b"message");
        let (r, s) = signature.split_scalars();
        let high = Signature::from_scalars(r, -*s).unwrap();
        let high = if high.normalize_s().is_some() {
            high
        } else {
            signature
        };
        assert!(key
            .public_key()
            .verify(&[b"message"], high.to_der().as_bytes())
            .is_ok());
    }

    #[test]
    fn auth_confirm_text() {
        let key = SessionKey::new(&[1; 16]);
        let confirm = auth_confirm(&key, Ipv4Addr::new(192, 168, 49, 20));
        assert_eq!(key.open(&confirm.cipher).unwrap(), b"192.168.49.20:0");
    }

    #[test]
    fn session_key_derivation_is_order_sensitive() {
        let shared = [5u8; 32];
        let a = derive_session_key(&shared, &[1; 32], &[2; 32]);
        let b = derive_session_key(&shared, &[2; 32], &[1; 32]);
        assert_ne!(a, b);
        assert_eq!(a, derive_session_key(&shared, &[1; 32], &[2; 32]));
    }

    #[test]
    fn screen_info() {
        assert_eq!(
            ScreenInfo::parse("1280_720_160_640_360"),
            Some(ScreenInfo {
                width: 1280,
                height: 720,
                dpi: 160,
                window: Some((640, 360))
            })
        );
        assert_eq!(ScreenInfo::parse("1280_720"), None);
        assert_eq!(ScreenInfo::parse("a_b_c"), None);
    }
}
