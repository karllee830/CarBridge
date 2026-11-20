// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/PairSetup.kt and
// airplay/PairVerify.kt (DiPlay commit 615b32c).

//! The receiver's side of AirPlay pairing, in TLV8 bodies of `application/pairing+tlv8`.
//!
//! Pair-setup runs SRP-6a with the fixed setup code [`SETUP_CODE`], then the two sides swap their long-term
//! Ed25519 keys, encrypted and signed; the controller's key is remembered. Pair-verify, on every connection, runs
//! an X25519 exchange signed with the long-term keys and gives the keys of the encrypted control channel.

use std::fmt;
use std::sync::Arc;

use crate::crypto::{self, X25519};
use crate::identity::{Identity, Pairings};
use crate::srp;
use crate::tlv8::{kind, Items, Writer};

/// The setup code CarPlay pairs with.
pub const SETUP_CODE: &str = "3939";
/// The TLV8 error that answers a failed step.
const ERROR_AUTHENTICATION: u8 = 0x02;

const SETUP_ENCRYPT_SALT: &[u8] = b"Pair-Setup-Encrypt-Salt";
const SETUP_ENCRYPT_INFO: &[u8] = b"Pair-Setup-Encrypt-Info";
const CONTROLLER_SIGN_SALT: &[u8] = b"Pair-Setup-Controller-Sign-Salt";
const CONTROLLER_SIGN_INFO: &[u8] = b"Pair-Setup-Controller-Sign-Info";
const ACCESSORY_SIGN_SALT: &[u8] = b"Pair-Setup-Accessory-Sign-Salt";
const ACCESSORY_SIGN_INFO: &[u8] = b"Pair-Setup-Accessory-Sign-Info";
const VERIFY_ENCRYPT_SALT: &[u8] = b"Pair-Verify-Encrypt-Salt";
const VERIFY_ENCRYPT_INFO: &[u8] = b"Pair-Verify-Encrypt-Info";
const CONTROL_SALT: &[u8] = b"Control-Salt";
const CONTROL_READ_INFO: &[u8] = b"Control-Read-Encryption-Key";
const CONTROL_WRITE_INFO: &[u8] = b"Control-Write-Encryption-Key";

/// Why a pairing step failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// A message whose state is not the next one, or no state at all.
    UnexpectedState(Option<u8>),
    /// An item the step needs is missing or malformed.
    Malformed(&'static str),
    /// The client's SRP proof did not match: a wrong setup code, or a broken client.
    WrongProof,
    /// The encrypted part did not decrypt.
    Decryption,
    /// A signature did not verify.
    Signature,
    /// Pair-verify from a controller that never paired.
    UnknownController(String),
    /// The pairings could not be read or written.
    Storage(String),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::UnexpectedState(Some(state)) => write!(f, "unexpected pairing state {state}"),
            Failure::UnexpectedState(None) => f.write_str("pairing message without a state"),
            Failure::Malformed(what) => write!(f, "pairing message without a valid {what}"),
            Failure::WrongProof => f.write_str("SRP proof did not match"),
            Failure::Decryption => f.write_str("encrypted pairing data did not decrypt"),
            Failure::Signature => f.write_str("pairing signature did not verify"),
            Failure::UnknownController(id) => write!(f, "controller {id} is not paired"),
            Failure::Storage(error) => write!(f, "pairings not available: {error}"),
        }
    }
}

impl std::error::Error for Failure {}

/// The answer to a pairing message: the body to send back, and why the step failed when it did. A failed step's
/// body is a TLV8 error the client understands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub body: Vec<u8>,
    pub failure: Option<Failure>,
}

impl Reply {
    fn ok(body: Writer) -> Reply {
        Reply {
            body: body.into_bytes(),
            failure: None,
        }
    }

    fn failed(state: u8, failure: Failure) -> Reply {
        Reply {
            body: Writer::new()
                .byte(kind::STATE, state)
                .byte(kind::ERROR, ERROR_AUTHENTICATION)
                .into_bytes(),
            failure: Some(failure),
        }
    }
}

/// Runs one pair-setup: messages M1, M3 and M5 from the controller, answered with M2, M4 and M6.
pub struct PairSetup {
    identity: Arc<Identity>,
    pairings: Arc<dyn Pairings>,
    srp: Option<srp::Server>,
    session_key: Option<[u8; 64]>,
    paired: Option<String>,
}

impl PairSetup {
    pub fn new(identity: Arc<Identity>, pairings: Arc<dyn Pairings>) -> PairSetup {
        PairSetup {
            identity,
            pairings,
            srp: None,
            session_key: None,
            paired: None,
        }
    }

    /// The controller that completed pair-setup, once it has.
    pub fn paired_controller(&self) -> Option<&str> {
        self.paired.as_deref()
    }

    pub fn handle(&mut self, body: &[u8]) -> Reply {
        let items = Items::parse(body);
        let state = items.byte(kind::STATE);
        let result = match state {
            Some(1) => Ok(self.start()),
            Some(3) => self.verify(&items).map_err(|failure| (4, failure)),
            Some(5) => self.exchange(&items).map_err(|failure| (6, failure)),
            other => Err((other.unwrap_or(0), Failure::UnexpectedState(other))),
        };
        result.unwrap_or_else(|(state, failure)| Reply::failed(state, failure))
    }

    /// M1 → M2: a fresh SRP server's salt and public key.
    fn start(&mut self) -> Reply {
        let server = srp::Server::new(srp::USERNAME, SETUP_CODE);
        let reply = Writer::new()
            .byte(kind::STATE, 2)
            .item(kind::PUBLIC_KEY, server.public_key())
            .item(kind::SALT, server.salt());
        self.srp = Some(server);
        self.session_key = None;
        self.paired = None;
        Reply::ok(reply)
    }

    /// M3 → M4: the client's SRP public key and proof, answered with the server's proof.
    fn verify(&mut self, items: &Items) -> Result<Reply, Failure> {
        let server = self.srp.take().ok_or(Failure::UnexpectedState(Some(3)))?;
        let public = items
            .get(kind::PUBLIC_KEY)
            .ok_or(Failure::Malformed("public key"))?;
        let proof = items.get(kind::PROOF).ok_or(Failure::Malformed("proof"))?;
        let verified = server.verify(public, proof).ok_or(Failure::WrongProof)?;
        self.session_key = Some(verified.session_key);
        Ok(Reply::ok(
            Writer::new()
                .byte(kind::STATE, 4)
                .item(kind::PROOF, &verified.server_proof),
        ))
    }

    /// M5 → M6: the controller's signed long-term key, answered with the receiver's.
    fn exchange(&mut self, items: &Items) -> Result<Reply, Failure> {
        let session_key = self.session_key.ok_or(Failure::UnexpectedState(Some(5)))?;
        let encrypted = items
            .get(kind::ENCRYPTED_DATA)
            .ok_or(Failure::Malformed("encrypted data"))?;
        let key = crypto::hkdf_sha512(&session_key, SETUP_ENCRYPT_SALT, SETUP_ENCRYPT_INFO);
        let sub = crypto::open(&key, &crypto::label_nonce("PS-Msg05"), &[], encrypted)
            .map_err(|_| Failure::Decryption)?;
        let sub = Items::parse(&sub);
        let controller = sub
            .get(kind::IDENTIFIER)
            .ok_or(Failure::Malformed("controller identifier"))?;
        let controller_key: [u8; 32] = sub
            .get(kind::PUBLIC_KEY)
            .and_then(|key| key.try_into().ok())
            .ok_or(Failure::Malformed("controller public key"))?;
        let signature = sub
            .get(kind::SIGNATURE)
            .ok_or(Failure::Malformed("controller signature"))?;
        let sign_key =
            crypto::hkdf_sha512(&session_key, CONTROLLER_SIGN_SALT, CONTROLLER_SIGN_INFO);
        let signed = [&sign_key[..], controller, &controller_key].concat();
        if !crypto::ed25519_verify(&controller_key, &signed, signature) {
            return Err(Failure::Signature);
        }
        let controller = String::from_utf8_lossy(controller).into_owned();
        self.pairings
            .remember(&controller, &controller_key)
            .map_err(|error| Failure::Storage(error.to_string()))?;

        let accessory = self.identity.pairing_id().as_bytes();
        let public = self.identity.public_key();
        let sign_key = crypto::hkdf_sha512(&session_key, ACCESSORY_SIGN_SALT, ACCESSORY_SIGN_INFO);
        let signature = self
            .identity
            .sign(&[&sign_key[..], accessory, &public].concat());
        let sub = Writer::new()
            .item(kind::IDENTIFIER, accessory)
            .item(kind::PUBLIC_KEY, &public)
            .item(kind::SIGNATURE, &signature)
            .into_bytes();
        let sealed = crypto::seal(&key, &crypto::label_nonce("PS-Msg06"), &[], &sub);
        self.session_key = None;
        self.paired = Some(controller);
        Ok(Reply::ok(
            Writer::new()
                .byte(kind::STATE, 6)
                .item(kind::ENCRYPTED_DATA, &sealed),
        ))
    }
}

/// The keys of the encrypted control channel, from the receiver's side.
#[derive(Clone, PartialEq, Eq)]
pub struct ControlKeys {
    /// Decrypts what the controller sends.
    pub read: [u8; 32],
    /// Encrypts what the receiver sends.
    pub write: [u8; 32],
}

impl fmt::Debug for ControlKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ControlKeys { .. }")
    }
}

/// What pair-verify established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The controller's pairing identifier.
    pub controller: String,
    pub control_keys: ControlKeys,
    /// The X25519 secret, which the media streams' keys derive from.
    pub shared_secret: [u8; 32],
}

/// The first half of pair-verify, waiting for M3.
struct Exchange {
    own_public: [u8; 32],
    client_public: [u8; 32],
    shared: [u8; 32],
    key: [u8; 32],
}

/// Runs pair-verify: messages M1 and M3 from the controller, answered with M2 and M4.
pub struct PairVerify {
    identity: Arc<Identity>,
    pairings: Arc<dyn Pairings>,
    exchange: Option<Exchange>,
    verified: Option<Verified>,
}

impl PairVerify {
    pub fn new(identity: Arc<Identity>, pairings: Arc<dyn Pairings>) -> PairVerify {
        PairVerify {
            identity,
            pairings,
            exchange: None,
            verified: None,
        }
    }

    /// What the last completed pair-verify established.
    pub fn verified(&self) -> Option<&Verified> {
        self.verified.as_ref()
    }

    pub fn handle(&mut self, body: &[u8]) -> Reply {
        let items = Items::parse(body);
        let state = items.byte(kind::STATE);
        let result = match state {
            Some(1) => self.start(&items).map_err(|failure| (2, failure)),
            Some(3) => self.finish(&items).map_err(|failure| (4, failure)),
            other => Err((other.unwrap_or(0), Failure::UnexpectedState(other))),
        };
        result.unwrap_or_else(|(state, failure)| Reply::failed(state, failure))
    }

    /// M1 → M2: an ephemeral key, and the receiver's identity signed over both ephemeral keys.
    fn start(&mut self, items: &Items) -> Result<Reply, Failure> {
        self.exchange = None;
        self.verified = None;
        let client_public: [u8; 32] = items
            .get(kind::PUBLIC_KEY)
            .and_then(|key| key.try_into().ok())
            .ok_or(Failure::Malformed("public key"))?;
        let own = X25519::generate();
        let own_public = own.public_key();
        let shared = own
            .shared_secret(&client_public)
            .ok_or(Failure::Malformed("public key"))?;
        let key = crypto::hkdf_sha512(&shared, VERIFY_ENCRYPT_SALT, VERIFY_ENCRYPT_INFO);
        let accessory = self.identity.pairing_id().as_bytes();
        let signature = self
            .identity
            .sign(&[&own_public[..], accessory, &client_public].concat());
        let sub = Writer::new()
            .item(kind::IDENTIFIER, accessory)
            .item(kind::SIGNATURE, &signature)
            .into_bytes();
        let sealed = crypto::seal(&key, &crypto::label_nonce("PV-Msg02"), &[], &sub);
        self.exchange = Some(Exchange {
            own_public,
            client_public,
            shared,
            key,
        });
        Ok(Reply::ok(
            Writer::new()
                .byte(kind::STATE, 2)
                .item(kind::PUBLIC_KEY, &own_public)
                .item(kind::ENCRYPTED_DATA, &sealed),
        ))
    }

    /// M3 → M4: the controller's identity, signed with its long-term key over both ephemeral keys.
    fn finish(&mut self, items: &Items) -> Result<Reply, Failure> {
        let exchange = self
            .exchange
            .take()
            .ok_or(Failure::UnexpectedState(Some(3)))?;
        let encrypted = items
            .get(kind::ENCRYPTED_DATA)
            .ok_or(Failure::Malformed("encrypted data"))?;
        let sub = crypto::open(
            &exchange.key,
            &crypto::label_nonce("PV-Msg03"),
            &[],
            encrypted,
        )
        .map_err(|_| Failure::Decryption)?;
        let sub = Items::parse(&sub);
        let controller = sub
            .get(kind::IDENTIFIER)
            .ok_or(Failure::Malformed("controller identifier"))?;
        let signature = sub
            .get(kind::SIGNATURE)
            .ok_or(Failure::Malformed("controller signature"))?;
        let name = String::from_utf8_lossy(controller).into_owned();
        let long_term = self
            .pairings
            .lookup(&name)
            .map_err(|error| Failure::Storage(error.to_string()))?
            .ok_or_else(|| Failure::UnknownController(name.clone()))?;
        let signed = [
            &exchange.client_public[..],
            controller,
            &exchange.own_public,
        ]
        .concat();
        if !crypto::ed25519_verify(&long_term, &signed, signature) {
            return Err(Failure::Signature);
        }
        self.verified = Some(Verified {
            controller: name,
            control_keys: ControlKeys {
                read: crypto::hkdf_sha512(&exchange.shared, CONTROL_SALT, CONTROL_WRITE_INFO),
                write: crypto::hkdf_sha512(&exchange.shared, CONTROL_SALT, CONTROL_READ_INFO),
            },
            shared_secret: exchange.shared,
        });
        Ok(Reply::ok(Writer::new().byte(kind::STATE, 4)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::from_hex;
    use crate::identity::MemoryPairings;
    use crate::srp::tests::{client, Form, CLIENT_PRIVATE, CLIENT_PRIVATE_SHORT};
    use ed25519_dalek::{Signer, SigningKey};

    const CONTROLLER: &str = "C0A1B2C3-0000-4000-8000-0000000000AA";

    struct Controller {
        key: SigningKey,
    }

    impl Controller {
        fn new() -> Controller {
            Controller {
                key: SigningKey::from_bytes(&[42; 32]),
            }
        }

        fn public(&self) -> [u8; 32] {
            self.key.verifying_key().to_bytes()
        }
    }

    fn receiver() -> (Arc<Identity>, Arc<MemoryPairings>) {
        (
            Arc::new(Identity::generate()),
            Arc::new(MemoryPairings::new()),
        )
    }

    fn ok(reply: Reply) -> Items {
        assert_eq!(reply.failure, None);
        Items::parse(&reply.body)
    }

    fn failed(reply: &Reply, state: u8) {
        let items = Items::parse(&reply.body);
        assert_eq!(items.byte(kind::STATE), Some(state));
        assert_eq!(items.byte(kind::ERROR), Some(ERROR_AUTHENTICATION));
        assert!(reply.failure.is_some());
    }

    /// Runs pair-setup as a controller would, up to and including M5; gives M6's reply and the session key.
    fn pair_setup(
        setup: &mut PairSetup,
        controller: &Controller,
        private: &str,
        form: Form,
        code: &str,
    ) -> (Reply, [u8; 64]) {
        let m2 = ok(setup.handle(
            &Writer::new()
                .byte(kind::STATE, 1)
                .byte(kind::METHOD, 0)
                .into_bytes(),
        ));
        assert_eq!(m2.byte(kind::STATE), Some(2));
        let salt = m2.get(kind::SALT).unwrap();
        let server_public = m2.get(kind::PUBLIC_KEY).unwrap();
        assert_eq!(server_public.len(), srp::GROUP_BYTES);
        let srp = client(
            salt,
            server_public,
            code,
            from_hex(private).try_into().unwrap(),
            form,
            form,
            form,
        );
        let m3 = Writer::new()
            .byte(kind::STATE, 3)
            .item(kind::PUBLIC_KEY, &srp.public)
            .item(kind::PROOF, &srp.proof)
            .into_bytes();
        let m4 = setup.handle(&m3);
        if m4.failure.is_some() {
            return (m4, srp.session_key);
        }
        let m4 = ok(m4);
        assert_eq!(m4.get(kind::PROOF), Some(&srp.expected_server_proof[..]));

        let key = crypto::hkdf_sha512(&srp.session_key, SETUP_ENCRYPT_SALT, SETUP_ENCRYPT_INFO);
        let sign_key =
            crypto::hkdf_sha512(&srp.session_key, CONTROLLER_SIGN_SALT, CONTROLLER_SIGN_INFO);
        let signature = controller
            .key
            .sign(&[&sign_key[..], CONTROLLER.as_bytes(), &controller.public()].concat());
        let sub = Writer::new()
            .item(kind::IDENTIFIER, CONTROLLER.as_bytes())
            .item(kind::PUBLIC_KEY, &controller.public())
            .item(kind::SIGNATURE, &signature.to_bytes())
            .into_bytes();
        let m5 = Writer::new()
            .byte(kind::STATE, 5)
            .item(
                kind::ENCRYPTED_DATA,
                &crypto::seal(&key, &crypto::label_nonce("PS-Msg05"), &[], &sub),
            )
            .into_bytes();
        (setup.handle(&m5), srp.session_key)
    }

    fn check_m6(reply: Reply, session_key: &[u8; 64], identity: &Identity) {
        let m6 = ok(reply);
        assert_eq!(m6.byte(kind::STATE), Some(6));
        let key = crypto::hkdf_sha512(session_key, SETUP_ENCRYPT_SALT, SETUP_ENCRYPT_INFO);
        let sub = crypto::open(
            &key,
            &crypto::label_nonce("PS-Msg06"),
            &[],
            m6.get(kind::ENCRYPTED_DATA).unwrap(),
        )
        .unwrap();
        let sub = Items::parse(&sub);
        assert_eq!(
            sub.get(kind::IDENTIFIER),
            Some(identity.pairing_id().as_bytes())
        );
        assert_eq!(sub.get(kind::PUBLIC_KEY), Some(&identity.public_key()[..]));
        let sign_key = crypto::hkdf_sha512(session_key, ACCESSORY_SIGN_SALT, ACCESSORY_SIGN_INFO);
        let signed = [
            &sign_key[..],
            identity.pairing_id().as_bytes(),
            &identity.public_key(),
        ]
        .concat();
        assert!(crypto::ed25519_verify(
            &identity.public_key(),
            &signed,
            sub.get(kind::SIGNATURE).unwrap()
        ));
    }

    #[test]
    fn pair_setup_remembers_the_controller() {
        let (identity, pairings) = receiver();
        let controller = Controller::new();
        let mut setup = PairSetup::new(identity.clone(), pairings.clone());
        let (m6, session_key) = pair_setup(
            &mut setup,
            &controller,
            CLIENT_PRIVATE,
            Form::Padded,
            SETUP_CODE,
        );
        check_m6(m6, &session_key, &identity);
        assert_eq!(setup.paired_controller(), Some(CONTROLLER));
        assert_eq!(
            pairings.lookup(CONTROLLER).unwrap(),
            Some(controller.public())
        );
    }

    #[test]
    fn pair_setup_accepts_a_client_key_with_a_leading_zero() {
        for form in [Form::Padded, Form::Minimal] {
            let (identity, pairings) = receiver();
            let mut setup = PairSetup::new(identity.clone(), pairings);
            let (m6, session_key) = pair_setup(
                &mut setup,
                &Controller::new(),
                CLIENT_PRIVATE_SHORT,
                form,
                SETUP_CODE,
            );
            check_m6(m6, &session_key, &identity);
        }
    }

    #[test]
    fn a_wrong_setup_code_fails_at_m4() {
        let (identity, pairings) = receiver();
        let mut setup = PairSetup::new(identity, pairings.clone());
        let (m4, _) = pair_setup(
            &mut setup,
            &Controller::new(),
            CLIENT_PRIVATE,
            Form::Padded,
            "1234",
        );
        failed(&m4, 4);
        assert_eq!(m4.failure, Some(Failure::WrongProof));
        assert_eq!(pairings.lookup(CONTROLLER).unwrap(), None);
    }

    #[test]
    fn pair_setup_out_of_order_fails() {
        let (identity, pairings) = receiver();
        let mut setup = PairSetup::new(identity, pairings);
        failed(
            &setup.handle(&Writer::new().byte(kind::STATE, 3).into_bytes()),
            4,
        );
        failed(
            &setup.handle(&Writer::new().byte(kind::STATE, 5).into_bytes()),
            6,
        );
        failed(
            &setup.handle(&Writer::new().byte(kind::STATE, 9).into_bytes()),
            9,
        );
        failed(&setup.handle(&[]), 0);
    }

    #[test]
    fn a_bad_controller_signature_is_not_remembered() {
        let (identity, pairings) = receiver();
        let mut setup = PairSetup::new(identity, pairings.clone());
        let m2 = ok(setup.handle(&Writer::new().byte(kind::STATE, 1).into_bytes()));
        let srp = client(
            m2.get(kind::SALT).unwrap(),
            m2.get(kind::PUBLIC_KEY).unwrap(),
            SETUP_CODE,
            from_hex(CLIENT_PRIVATE).try_into().unwrap(),
            Form::Padded,
            Form::Padded,
            Form::Padded,
        );
        ok(setup.handle(
            &Writer::new()
                .byte(kind::STATE, 3)
                .item(kind::PUBLIC_KEY, &srp.public)
                .item(kind::PROOF, &srp.proof)
                .into_bytes(),
        ));
        let key = crypto::hkdf_sha512(&srp.session_key, SETUP_ENCRYPT_SALT, SETUP_ENCRYPT_INFO);
        let controller = Controller::new();
        let sub = Writer::new()
            .item(kind::IDENTIFIER, CONTROLLER.as_bytes())
            .item(kind::PUBLIC_KEY, &controller.public())
            .item(kind::SIGNATURE, &[0; 64])
            .into_bytes();
        let m5 = Writer::new()
            .byte(kind::STATE, 5)
            .item(
                kind::ENCRYPTED_DATA,
                &crypto::seal(&key, &crypto::label_nonce("PS-Msg05"), &[], &sub),
            )
            .into_bytes();
        let m6 = setup.handle(&m5);
        failed(&m6, 6);
        assert_eq!(m6.failure, Some(Failure::Signature));
        assert_eq!(pairings.lookup(CONTROLLER).unwrap(), None);
    }

    /// Runs pair-verify as a paired controller would; gives the controller's view of the shared secret.
    fn pair_verify(
        verify: &mut PairVerify,
        controller: &Controller,
        identity: &Identity,
    ) -> (Reply, [u8; 32]) {
        let ephemeral = X25519::generate();
        let m2 = ok(verify.handle(
            &Writer::new()
                .byte(kind::STATE, 1)
                .item(kind::PUBLIC_KEY, &ephemeral.public_key())
                .into_bytes(),
        ));
        assert_eq!(m2.byte(kind::STATE), Some(2));
        let receiver_public = m2.get(kind::PUBLIC_KEY).unwrap();
        let shared = ephemeral.shared_secret(receiver_public).unwrap();
        let key = crypto::hkdf_sha512(&shared, VERIFY_ENCRYPT_SALT, VERIFY_ENCRYPT_INFO);
        let sub = crypto::open(
            &key,
            &crypto::label_nonce("PV-Msg02"),
            &[],
            m2.get(kind::ENCRYPTED_DATA).unwrap(),
        )
        .unwrap();
        let sub = Items::parse(&sub);
        assert_eq!(
            sub.get(kind::IDENTIFIER),
            Some(identity.pairing_id().as_bytes())
        );
        let signed = [
            receiver_public,
            identity.pairing_id().as_bytes(),
            &ephemeral.public_key(),
        ]
        .concat();
        assert!(crypto::ed25519_verify(
            &identity.public_key(),
            &signed,
            sub.get(kind::SIGNATURE).unwrap()
        ));

        let signature = controller.key.sign(
            &[
                &ephemeral.public_key()[..],
                CONTROLLER.as_bytes(),
                receiver_public,
            ]
            .concat(),
        );
        let sub = Writer::new()
            .item(kind::IDENTIFIER, CONTROLLER.as_bytes())
            .item(kind::SIGNATURE, &signature.to_bytes())
            .into_bytes();
        let m3 = Writer::new()
            .byte(kind::STATE, 3)
            .item(
                kind::ENCRYPTED_DATA,
                &crypto::seal(&key, &crypto::label_nonce("PV-Msg03"), &[], &sub),
            )
            .into_bytes();
        (verify.handle(&m3), shared)
    }

    #[test]
    fn pair_verify_gives_the_control_keys() {
        let (identity, pairings) = receiver();
        let controller = Controller::new();
        pairings.remember(CONTROLLER, &controller.public()).unwrap();
        let mut verify = PairVerify::new(identity.clone(), pairings);
        let (m4, shared) = pair_verify(&mut verify, &controller, &identity);
        assert_eq!(ok(m4).byte(kind::STATE), Some(4));
        let verified = verify.verified().unwrap();
        assert_eq!(verified.controller, CONTROLLER);
        assert_eq!(verified.shared_secret, shared);
        // The controller writes with the key the receiver reads with, and the other way round.
        let controller_write = crypto::hkdf_sha512(&shared, CONTROL_SALT, CONTROL_WRITE_INFO);
        let controller_read = crypto::hkdf_sha512(&shared, CONTROL_SALT, CONTROL_READ_INFO);
        assert_eq!(verified.control_keys.read, controller_write);
        assert_eq!(verified.control_keys.write, controller_read);
    }

    #[test]
    fn pair_verify_after_pair_setup() {
        let (identity, pairings) = receiver();
        let controller = Controller::new();
        let mut setup = PairSetup::new(identity.clone(), pairings.clone());
        let (m6, session_key) = pair_setup(
            &mut setup,
            &controller,
            CLIENT_PRIVATE,
            Form::Padded,
            SETUP_CODE,
        );
        check_m6(m6, &session_key, &identity);
        let mut verify = PairVerify::new(identity.clone(), pairings);
        let (m4, _) = pair_verify(&mut verify, &controller, &identity);
        ok(m4);
        assert!(verify.verified().is_some());
    }

    #[test]
    fn pair_verify_refuses_unknown_controllers_and_bad_signatures() {
        let (identity, pairings) = receiver();
        let controller = Controller::new();
        let mut verify = PairVerify::new(identity.clone(), pairings.clone());
        let (m4, _) = pair_verify(&mut verify, &controller, &identity);
        failed(&m4, 4);
        assert_eq!(
            m4.failure,
            Some(Failure::UnknownController(CONTROLLER.into()))
        );

        pairings
            .remember(
                CONTROLLER,
                &SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes(),
            )
            .unwrap();
        let (m4, _) = pair_verify(&mut verify, &controller, &identity);
        failed(&m4, 4);
        assert_eq!(m4.failure, Some(Failure::Signature));
        assert!(verify.verified().is_none());
    }

    #[test]
    fn pair_verify_refuses_bad_keys_and_order() {
        let (identity, pairings) = receiver();
        let mut verify = PairVerify::new(identity, pairings);
        failed(
            &verify.handle(&Writer::new().byte(kind::STATE, 3).into_bytes()),
            4,
        );
        let small_order = Writer::new()
            .byte(kind::STATE, 1)
            .item(kind::PUBLIC_KEY, &[0; 32])
            .into_bytes();
        failed(&verify.handle(&small_order), 2);
        let short = Writer::new()
            .byte(kind::STATE, 1)
            .item(kind::PUBLIC_KEY, &[9; 31])
            .into_bytes();
        failed(&verify.handle(&short), 2);
    }
}
