// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The MFi identity the receiver authenticates with: the accessory's private key (PKCS #8, DER) and its certificate
//! (PKCS #7 SignedData holding one P-256 certificate, DER), kept as two files in one directory.
//!
//! [`inspect`] tells whether the identity in a directory is there and usable, and what its certificate says;
//! [`check`] tells the same of a key and a certificate given as bytes, as the receiver would load them. Nothing here
//! returns or formats the private key.

use std::fmt;
use std::io;
use std::path::Path;

use carbridge_iap2::mfi::{IdentityError, LocalAuthenticator};
use der::Decode;
use p256::pkcs8::DecodePrivateKey;

/// The private key's file in the identity's directory.
pub const KEY_FILE: &str = "identity.pk8";
/// The certificate's file in the identity's directory.
pub const CERTIFICATE_FILE: &str = "certificate.p7b";
/// The largest file of an identity the receiver loads.
pub const MAX_FILE_BYTES: usize = 16 * 1024;

/// What the certificate says of the accessory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Certificate {
    /// The serial number, in hex.
    pub serial: String,
    /// The subject, as RFC 4514 writes it.
    pub subject: String,
    pub issuer: String,
    /// The validity, in Unix seconds.
    pub not_before: u64,
    pub not_after: u64,
}

/// Why an identity cannot be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The file is not there.
    Missing(&'static str),
    /// The file cannot be read.
    Unreadable(&'static str, String),
    /// The key is not a P-256 private key in PKCS #8.
    BadKey(String),
    /// The certificate is not a PKCS #7 SignedData holding one P-256 certificate.
    BadCertificate(String),
    /// The key is not the one the certificate is for.
    Mismatch,
}

impl Problem {
    /// A short name of the problem, for the web interface.
    pub fn code(&self) -> &'static str {
        match self {
            Problem::Missing(_) => "missing",
            Problem::Unreadable(..) => "unreadable",
            Problem::BadKey(_) => "bad-key",
            Problem::BadCertificate(_) => "bad-certificate",
            Problem::Mismatch => "mismatch",
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Missing(file) => write!(f, "{file} is missing"),
            Problem::Unreadable(file, err) => write!(f, "{file} cannot be read: {err}"),
            Problem::BadKey(why) => write!(f, "the private key cannot be used: {why}"),
            Problem::BadCertificate(why) => write!(f, "the certificate cannot be used: {why}"),
            Problem::Mismatch => f.write_str("the private key does not belong to the certificate"),
        }
    }
}

/// The state of a file of the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Missing,
    Unreadable,
    Present,
}

/// The identity in a directory, as [`inspect`] finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub key: FileState,
    pub certificate: FileState,
    /// What the certificate says, when it can be read, whether or not the key belongs to it.
    pub facts: Option<Certificate>,
    /// Why the identity cannot be used, `None` when it can.
    pub problem: Option<Problem>,
}

impl Inspection {
    /// Whether the receiver can authenticate with the identity.
    pub fn usable(&self) -> bool {
        self.problem.is_none()
    }
}

/// Finds the identity in `dir` and checks it as the receiver would load it.
pub fn inspect(dir: &Path) -> Inspection {
    let read = |name: &'static str| -> (FileState, Result<Vec<u8>, Problem>) {
        match std::fs::read(dir.join(name)) {
            Ok(bytes) => (FileState::Present, Ok(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                (FileState::Missing, Err(Problem::Missing(name)))
            }
            Err(err) => (
                FileState::Unreadable,
                Err(Problem::Unreadable(name, err.to_string())),
            ),
        }
    };
    let (key_state, key) = read(KEY_FILE);
    let (certificate_state, certificate) = read(CERTIFICATE_FILE);
    let facts = certificate
        .as_deref()
        .ok()
        .and_then(|bytes| certificate_facts(bytes).ok());
    let problem = match (key, certificate) {
        (Err(problem), _) | (_, Err(problem)) => Some(problem),
        (Ok(key), Ok(certificate)) => check(&key, &certificate).err(),
    };
    Inspection {
        key: key_state,
        certificate: certificate_state,
        facts,
        problem,
    }
}

/// Checks that `key` and `certificate` make an identity the receiver can authenticate with, and returns what the
/// certificate says.
pub fn check(key: &[u8], certificate: &[u8]) -> Result<Certificate, Problem> {
    let size = |bytes: &[u8]| -> Result<(), String> {
        match bytes.len() {
            0 => Err("the file is empty".into()),
            n if n > MAX_FILE_BYTES => {
                Err(format!("the file is larger than {MAX_FILE_BYTES} bytes"))
            }
            _ => Ok(()),
        }
    };
    size(key).map_err(Problem::BadKey)?;
    size(certificate).map_err(Problem::BadCertificate)?;
    p256::ecdsa::SigningKey::from_pkcs8_der(key)
        .map_err(|_| Problem::BadKey("not a P-256 private key in PKCS #8 (DER)".into()))?;
    let facts = certificate_facts(certificate).map_err(Problem::BadCertificate)?;
    match LocalAuthenticator::new(key, certificate) {
        Ok(_) => Ok(facts),
        Err(IdentityError::Mismatch) => Err(Problem::Mismatch),
        Err(err) => Err(Problem::BadCertificate(err.to_string())),
    }
}

/// What the certificate in a PKCS #7 SignedData says, for one that holds a single certificate.
pub fn certificate_facts(p7b: &[u8]) -> Result<Certificate, String> {
    let info = cms::content_info::ContentInfo::from_der(p7b)
        .map_err(|_| "not PKCS #7 in DER".to_owned())?;
    let signed = info
        .content
        .decode_as::<cms::signed_data::SignedData>()
        .map_err(|_| "not a PKCS #7 SignedData".to_owned())?;
    let certificates = signed
        .certificates
        .ok_or_else(|| "it holds no certificate".to_owned())?;
    if certificates.0.len() != 1 {
        return Err(format!(
            "it holds {} certificates, not one",
            certificates.0.len()
        ));
    }
    let Some(cms::cert::CertificateChoices::Certificate(certificate)) =
        certificates.0.iter().next()
    else {
        return Err("it holds no X.509 certificate".into());
    };
    let tbs = &certificate.tbs_certificate;
    Ok(Certificate {
        serial: tbs
            .serial_number
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        subject: tbs.subject.to_string(),
        issuer: tbs.issuer.to_string(),
        not_before: tbs.validity.not_before.to_unix_duration().as_secs(),
        not_after: tbs.validity.not_after.to_unix_duration().as_secs(),
    })
}

/// A file of an identity as it was uploaded, in DER: as it is when it is DER already, else decoded from PEM or from
/// plain base64 of DER or PEM. `None` when it is none of these.
pub fn to_der(upload: &[u8]) -> Option<Vec<u8>> {
    // DER starts with a SEQUENCE; neither PEM nor base64 starts with its tag.
    if upload.first() == Some(&0x30) {
        return Some(upload.to_vec());
    }
    let text = std::str::from_utf8(upload)
        .ok()
        .map(str::trim)
        .filter(|text| !text.is_empty())?;
    if text.starts_with("-----BEGIN") {
        let body: String = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("-----") && !line.contains(':'))
            .collect();
        return base64(&body).filter(|der| der.first() == Some(&0x30));
    }
    let decoded = base64(&text.split_whitespace().collect::<String>())?;
    if decoded.first() == Some(&0x30) {
        return Some(decoded);
    }
    std::str::from_utf8(&decoded)
        .ok()
        .filter(|inner| inner.trim_start().starts_with("-----BEGIN"))
        .and_then(|inner| to_der(inner.as_bytes()))
}

fn base64(text: &str) -> Option<Vec<u8>> {
    use base64ct::{Base64, Encoding};
    Base64::decode_vec(text)
        .ok()
        .filter(|bytes| !bytes.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway P-256 key and a self-signed certificate for it, made for these tests only, and another key.
    const KEY: &[u8] = include_bytes!("../testdata/mfi/identity.pk8");
    const KEY_PEM: &[u8] = include_bytes!("../testdata/mfi/identity.pk8.pem");
    const CERTIFICATE: &[u8] = include_bytes!("../testdata/mfi/certificate.p7b");
    const CERTIFICATE_PEM: &[u8] = include_bytes!("../testdata/mfi/certificate.p7b.pem");
    const OTHER_KEY: &[u8] = include_bytes!("../testdata/mfi/other.pk8");

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("carplay-mfi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_good_pair() {
        let facts = check(KEY, CERTIFICATE).unwrap();
        assert_eq!(facts.serial, "1234abcd");
        assert_eq!(
            facts.subject,
            "O=CarBridge tests,CN=CarBridge test accessory"
        );
        assert_eq!(facts.issuer, facts.subject);
        assert!(facts.not_before < facts.not_after);
    }

    #[test]
    fn a_key_of_another_certificate() {
        assert_eq!(check(OTHER_KEY, CERTIFICATE), Err(Problem::Mismatch));
        assert_eq!(Problem::Mismatch.code(), "mismatch");
    }

    #[test]
    fn garbage() {
        assert!(matches!(
            check(b"garbage", CERTIFICATE),
            Err(Problem::BadKey(_))
        ));
        assert!(matches!(
            check(KEY, b"garbage"),
            Err(Problem::BadCertificate(_))
        ));
        assert!(matches!(check(&[], CERTIFICATE), Err(Problem::BadKey(_))));
        assert!(matches!(
            check(KEY, &vec![0x30; MAX_FILE_BYTES + 1]),
            Err(Problem::BadCertificate(_))
        ));
        // The two files the wrong way round.
        assert!(matches!(check(CERTIFICATE, KEY), Err(Problem::BadKey(_))));
        assert!(certificate_facts(KEY).is_err());
    }

    #[test]
    fn inspected_in_a_directory() {
        let dir = temp_dir("inspect");
        let missing = inspect(&dir);
        assert_eq!(
            (missing.key, missing.certificate),
            (FileState::Missing, FileState::Missing)
        );
        assert_eq!(missing.problem, Some(Problem::Missing(KEY_FILE)));
        assert!(!missing.usable());

        std::fs::write(dir.join(CERTIFICATE_FILE), CERTIFICATE).unwrap();
        let no_key = inspect(&dir);
        assert_eq!(no_key.problem, Some(Problem::Missing(KEY_FILE)));
        assert_eq!(no_key.certificate, FileState::Present);
        assert_eq!(no_key.facts.unwrap().serial, "1234abcd");

        std::fs::write(dir.join(KEY_FILE), OTHER_KEY).unwrap();
        let mismatched = inspect(&dir);
        assert_eq!(mismatched.problem, Some(Problem::Mismatch));
        assert!(mismatched.facts.is_some());

        std::fs::write(dir.join(KEY_FILE), KEY).unwrap();
        let good = inspect(&dir);
        assert!(good.usable());
        assert_eq!(
            (good.key, good.certificate),
            (FileState::Present, FileState::Present)
        );

        std::fs::remove_file(dir.join(CERTIFICATE_FILE)).unwrap();
        assert_eq!(
            inspect(&dir).problem,
            Some(Problem::Missing(CERTIFICATE_FILE))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uploads_in_der_pem_or_base64() {
        use base64ct::{Base64, Encoding};
        assert_eq!(to_der(KEY).as_deref(), Some(KEY));
        assert_eq!(to_der(KEY_PEM).as_deref(), Some(KEY));
        assert_eq!(to_der(CERTIFICATE_PEM).as_deref(), Some(CERTIFICATE));
        let encoded = Base64::encode_string(CERTIFICATE);
        assert_eq!(to_der(encoded.as_bytes()).as_deref(), Some(CERTIFICATE));
        let pem_encoded = Base64::encode_string(KEY_PEM);
        assert_eq!(to_der(pem_encoded.as_bytes()).as_deref(), Some(KEY));
        assert_eq!(to_der(b""), None);
        assert_eq!(to_der(b"not an identity file"), None);
        assert_eq!(
            to_der(b"-----BEGIN NOTHING-----\n!!!\n-----END NOTHING-----"),
            None
        );
    }
}
