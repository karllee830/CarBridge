// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's mfi/MfiAuthenticationClient.kt (the
// authenticator interface), mfi/LocalMfiAuthenticationClient.kt and mfi/Iap2MfiAuthenticationClient.kt (DiPlay commit
// 615b32c).

//! MFi accessory authentication: the authenticator that holds the accessory's certificate and signs challenges,
//! a local one with a P-256 key, and the iAP2 exchange that uses it. AirPlay's auth-setup uses the same
//! authenticator.

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use der::{Decode, Encode};
use p256::ecdsa::signature::hazmat::PrehashSigner;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::DecodePrivateKey;

use crate::error::Error;
use crate::messages::{self, id};
use crate::session::Session;

/// The largest challenge an iPhone sends.
const MAX_CHALLENGE_BYTES: usize = 128;

/// Holds the accessory's certificate and signs the challenges that prove the accessory has its key.
pub trait Authenticator: Send + Sync {
    /// The certificate as the iPhone is sent it.
    fn certificate(&self) -> io::Result<Vec<u8>>;

    /// The signature of a challenge, which is a digest already.
    fn sign(&self, challenge: &[u8]) -> io::Result<Vec<u8>>;

    /// The authentication protocol's major version: 2 signs SHA-1 digests, 3 and later SHA-256 ones.
    fn protocol_major(&self) -> u8 {
        3
    }
}

/// Why a local identity cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityError {
    /// The key is not a P-256 private key in PKCS #8 DER.
    Key,
    /// The certificate cannot be read, or holds other than one P-256 certificate.
    Certificate(String),
    /// The key is not the certificate's.
    Mismatch,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::Key => f.write_str("not a P-256 private key in PKCS #8 DER"),
            IdentityError::Certificate(reason) => write!(f, "unusable certificate: {reason}"),
            IdentityError::Mismatch => f.write_str("the private key is not the certificate's"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<IdentityError> for io::Error {
    fn from(error: IdentityError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// An authenticator with the accessory's P-256 key and certificate at hand, rather than in a coprocessor.
pub struct LocalAuthenticator {
    key: SigningKey,
    certificate: Vec<u8>,
}

impl fmt::Debug for LocalAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalAuthenticator").finish_non_exhaustive()
    }
}

impl LocalAuthenticator {
    /// From a PKCS #8 DER key and a certificate: PKCS #7 SignedData holding one certificate, or one X.509
    /// certificate, in DER. The certificate is sent as given.
    pub fn new(key: &[u8], certificate: &[u8]) -> Result<LocalAuthenticator, IdentityError> {
        let key = SigningKey::from_pkcs8_der(key).map_err(|_| IdentityError::Key)?;
        let public = certificate_key(certificate)?;
        if &public != key.verifying_key() {
            return Err(IdentityError::Mismatch);
        }
        Ok(LocalAuthenticator {
            key,
            certificate: certificate.to_vec(),
        })
    }
}

impl Authenticator for LocalAuthenticator {
    fn certificate(&self) -> io::Result<Vec<u8>> {
        Ok(self.certificate.clone())
    }

    /// ECDSA over the 32-byte digest the iPhone sends, without hashing it again: `r || s`, 64 bytes.
    fn sign(&self, challenge: &[u8]) -> io::Result<Vec<u8>> {
        if challenge.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "a challenge of {} bytes, not a 32-byte digest",
                    challenge.len()
                ),
            ));
        }
        let signature: Signature = self
            .key
            .sign_prehash(challenge)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(signature.to_bytes().to_vec())
    }
}

/// The public key of the one certificate in `certificate`.
fn certificate_key(certificate: &[u8]) -> Result<VerifyingKey, IdentityError> {
    let bad = |reason: &str| IdentityError::Certificate(reason.to_owned());
    let x509 = match cms::content_info::ContentInfo::from_der(certificate) {
        Ok(info) => {
            let signed = info
                .content
                .decode_as::<cms::signed_data::SignedData>()
                .map_err(|_| bad("not a PKCS #7 SignedData"))?;
            let certificates = signed
                .certificates
                .ok_or_else(|| bad("it holds no certificate"))?;
            if certificates.0.len() != 1 {
                return Err(bad("it holds more than one certificate"));
            }
            match certificates.0.iter().next() {
                Some(cms::cert::CertificateChoices::Certificate(certificate)) => {
                    certificate.clone()
                }
                _ => return Err(bad("it holds no X.509 certificate")),
            }
        }
        Err(_) => x509_cert::Certificate::from_der(certificate)
            .map_err(|_| bad("neither PKCS #7 nor X.509 in DER"))?,
    };
    let info = &x509.tbs_certificate.subject_public_key_info;
    let spki = info.to_der().map_err(|_| bad("unreadable public key"))?;
    use p256::pkcs8::DecodePublicKey;
    VerifyingKey::from_public_key_der(&spki).map_err(|_| bad("not a P-256 public key"))
}

/// Runs MFi authentication on an identified session: the certificate and the challenge answer the iPhone asks
/// for, until it says authentication succeeded.
pub fn authenticate(
    session: &Session,
    authenticator: &dyn Authenticator,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), Error> {
    let deadline = Instant::now() + timeout;
    let certificate = authenticator.certificate()?;
    let certificate = messages::authentication_certificate(&certificate)?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = session
            .recv(remaining)?
            .ok_or(Error::TimedOut("waiting for MFi authentication"))?;
        match frame.message_id() {
            id::REQUEST_AUTHENTICATION_CERTIFICATE => {
                session.send(
                    &certificate,
                    deadline.saturating_duration_since(Instant::now()),
                )?;
                log("iAP2 MFi certificate sent");
            }
            id::REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => {
                let challenge = messages::authentication_challenge(&frame)?;
                if challenge.is_empty() || challenge.len() > MAX_CHALLENGE_BYTES {
                    return Err(Error::Authentication(format!(
                        "a challenge of {} bytes",
                        challenge.len()
                    )));
                }
                let signature = authenticator
                    .sign(&challenge)
                    .map_err(|error| Error::Authentication(error.to_string()))?;
                let response = messages::authentication_response(&signature)?;
                session.send(
                    &response,
                    deadline.saturating_duration_since(Instant::now()),
                )?;
                log("iAP2 MFi challenge answered");
            }
            id::AUTHENTICATION_SUCCEEDED => return Ok(()),
            id::AUTHENTICATION_FAILED => {
                return Err(Error::Authentication(
                    "the iPhone refused the accessory".into(),
                ))
            }
            other => return Err(Error::UnexpectedMessage(other)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use p256::ecdsa::signature::hazmat::PrehashVerifier;

    // A throwaway test identity, made with openssl for these tests only.
    pub(crate) const KEY: &[u8] = include_bytes!("testdata/identity.pk8");
    pub(crate) const P7B: &[u8] = include_bytes!("testdata/certificate.p7b");
    const X509: &[u8] = include_bytes!("testdata/cert.der");
    const OTHER_KEY: &[u8] = include_bytes!("testdata/other.pk8");

    #[test]
    fn a_matching_identity_loads_and_signs_digests() {
        let local = LocalAuthenticator::new(KEY, P7B).unwrap();
        assert_eq!(local.certificate().unwrap(), P7B);
        let digest = [0x5a; 32];
        let signature = local.sign(&digest).unwrap();
        assert_eq!(signature.len(), 64);
        let verifying = *SigningKey::from_pkcs8_der(KEY).unwrap().verifying_key();
        let signature = Signature::from_slice(&signature).unwrap();
        assert!(verifying.verify_prehash(&digest, &signature).is_ok());
        assert!(local.sign(&[0; 31]).is_err());
        assert!(LocalAuthenticator::new(KEY, X509).is_ok());
    }

    #[test]
    fn unusable_identities_are_refused() {
        assert_eq!(
            LocalAuthenticator::new(OTHER_KEY, P7B).unwrap_err(),
            IdentityError::Mismatch
        );
        assert_eq!(
            LocalAuthenticator::new(b"not a key", P7B).unwrap_err(),
            IdentityError::Key
        );
        assert!(matches!(
            LocalAuthenticator::new(KEY, b"not a certificate").unwrap_err(),
            IdentityError::Certificate(_)
        ));
    }
}
