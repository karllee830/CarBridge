// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The MFi identity CarPlay authenticates with, as the web interface shows it and replaces it.
//!
//! The web interface uploads a private key and a certificate next to its request file (`mfi-identity.pk8` and
//! `mfi-certificate.p7b`, in DER, PEM or base64 of either), then writes the request: `install` to put them in place of
//! an identity that is missing or cannot be used, `replace` to put them in place of one that can. CarBridge checks
//! them as the receiver loads them, installs them readable by root only, tells how it went in the status, and removes
//! what was uploaded. The private key is never logged or put in the status.

use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use carbridge_carplay::mfi::{self, Certificate, FileState, Inspection};
use serde::Serialize;

/// The identity as the status shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MfiStatus {
    /// The directory the identity is kept in.
    pub dir: String,
    /// Whether the private key and the certificate are there: `missing`, `unreadable` or `present`.
    pub key: &'static str,
    pub certificate: &'static str,
    /// CarPlay can authenticate with the identity.
    pub usable: bool,
    /// Why it cannot: `missing`, `unreadable`, `bad-key`, `bad-certificate` or `mismatch`.
    pub problem: Option<&'static str>,
    /// The same, in words.
    pub problem_detail: Option<String>,
    /// What the certificate says, when it can be read.
    pub certificate_info: Option<CertificateInfo>,
    /// How the last request to install an identity went.
    pub install: Option<MfiInstall>,
}

/// What the certificate says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CertificateInfo {
    pub serial: String,
    pub subject: String,
    pub issuer: String,
    /// The validity, in Unix seconds.
    pub not_before: u64,
    pub not_after: u64,
}

impl From<&Certificate> for CertificateInfo {
    fn from(facts: &Certificate) -> CertificateInfo {
        CertificateInfo {
            serial: facts.serial.clone(),
            subject: facts.subject.clone(),
            issuer: facts.issuer.clone(),
            not_before: facts.not_before,
            not_after: facts.not_after,
        }
    }
}

/// How a request to install an identity went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MfiInstall {
    /// Counts the requests, for one to be told from the one before.
    pub attempt: u64,
    /// `installed`; `refused`, the files, which are not installed; or `failed`, writing them.
    pub result: &'static str,
    /// For `refused`: `upload`, a file not uploaded or not DER, PEM or base64; `present`, a usable identity is
    /// installed and the request does not replace it; or what is wrong with the pair, as [`MfiStatus::problem`].
    pub reason: Option<&'static str>,
    pub detail: Option<String>,
    /// For `installed`: CarPlay already ran with the identity before, which it keeps until CarBridge starts again.
    pub restart_needed: bool,
}

impl MfiStatus {
    /// The status of the identity `inspection` found in `dir`, with the outcome of the last request.
    pub fn new(dir: &Path, inspection: &Inspection, install: Option<MfiInstall>) -> MfiStatus {
        let state = |file: FileState| match file {
            FileState::Missing => "missing",
            FileState::Unreadable => "unreadable",
            FileState::Present => "present",
        };
        MfiStatus {
            dir: dir.display().to_string(),
            key: state(inspection.key),
            certificate: state(inspection.certificate),
            usable: inspection.usable(),
            problem: inspection.problem.as_ref().map(mfi::Problem::code),
            problem_detail: inspection.problem.as_ref().map(ToString::to_string),
            certificate_info: inspection.facts.as_ref().map(CertificateInfo::from),
            install,
        }
    }
}

/// The identity `inspection` found in `dir`, for `--check-mfi`: whether it is usable, and what its certificate says.
pub fn describe(dir: &Path, inspection: &Inspection) -> String {
    let status = MfiStatus::new(dir, inspection, None);
    let mut lines = vec![
        format!("MFi identity in {}", status.dir),
        format!("  {}: {}", mfi::KEY_FILE, status.key),
        format!("  {}: {}", mfi::CERTIFICATE_FILE, status.certificate),
    ];
    if let Some(info) = &status.certificate_info {
        lines.push(format!("  subject: {}", info.subject));
        lines.push(format!("  issuer: {}", info.issuer));
        lines.push(format!("  serial: {}", info.serial));
        lines.push(format!(
            "  valid: {} to {} (Unix seconds)",
            info.not_before, info.not_after
        ));
    }
    lines.push(match &status.problem_detail {
        None => "  usable: yes".to_owned(),
        Some(problem) => format!("  usable: no, {problem}"),
    });
    lines.join("\n") + "\n"
}

/// Where the web interface leaves an identity to install, next to its request file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub request: PathBuf,
    pub key: PathBuf,
    pub certificate: PathBuf,
}

impl Upload {
    pub fn next_to(request: &Path) -> Upload {
        let dir = request.parent().unwrap_or(Path::new("."));
        Upload {
            request: request.to_owned(),
            key: dir.join("mfi-identity.pk8"),
            certificate: dir.join("mfi-certificate.p7b"),
        }
    }

    /// Removes what was uploaded and the request.
    pub fn clear(&self) {
        for path in [&self.key, &self.certificate, &self.request] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// How installing an identity went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Installed; with `replaced`, in place of a usable one.
    Installed {
        facts: Certificate,
        replaced: bool,
    },
    Refused {
        reason: &'static str,
        detail: String,
    },
    Failed(String),
}

impl Outcome {
    /// The outcome as the status tells it, as the `attempt`th request.
    pub fn status(&self, attempt: u64, restart_needed: bool) -> MfiInstall {
        let (result, reason, detail) = match self {
            Outcome::Installed { .. } => ("installed", None, None),
            Outcome::Refused { reason, detail } => ("refused", Some(*reason), Some(detail.clone())),
            Outcome::Failed(err) => ("failed", None, Some(err.clone())),
        };
        MfiInstall {
            attempt,
            result,
            reason,
            detail,
            restart_needed: restart_needed && matches!(self, Outcome::Installed { .. }),
        }
    }

    /// A line for the log.
    pub fn describe(&self) -> String {
        match self {
            Outcome::Installed { facts, replaced } => format!(
                "MFi identity {}: certificate {}, serial {}",
                if *replaced { "replaced" } else { "installed" },
                facts.subject,
                facts.serial
            ),
            Outcome::Refused { detail, .. } => format!("MFi identity not installed: {detail}"),
            Outcome::Failed(err) => format!("installing the MFi identity: {err}"),
        }
    }
}

/// Installs the private key `key` and the certificate `certificate`, as uploaded, in `dir`, once they make an identity
/// CarPlay can authenticate with; an identity in `dir` that it can authenticate with already is replaced only with
/// `replace`.
pub fn install(dir: &Path, key: &[u8], certificate: &[u8], replace: bool) -> Outcome {
    let (Some(key), Some(certificate)) = (mfi::to_der(key), mfi::to_der(certificate)) else {
        return Outcome::Refused {
            reason: "upload",
            detail: "the files are not in DER, PEM or base64".into(),
        };
    };
    let facts = match mfi::check(&key, &certificate) {
        Ok(facts) => facts,
        Err(problem) => {
            return Outcome::Refused {
                reason: problem.code(),
                detail: problem.to_string(),
            }
        }
    };
    let present = mfi::inspect(dir).usable();
    if present && !replace {
        return Outcome::Refused {
            reason: "present",
            detail: "a usable identity is installed; replacing it needs confirming".into(),
        };
    }
    match write_identity(dir, &key, &certificate) {
        Ok(()) => Outcome::Installed {
            facts,
            replaced: present,
        },
        Err(err) => Outcome::Failed(err.to_string()),
    }
}

/// Installs the request the web interface left at `upload`, if there is one, in `dir`, and removes the request and
/// what it uploaded. `None` without a request.
pub fn take_request(upload: &Upload, dir: &Path) -> Option<Outcome> {
    let request = std::fs::read_to_string(&upload.request).ok()?;
    let outcome = match request.trim() {
        verb @ ("install" | "replace") => {
            match (
                std::fs::read(&upload.key),
                std::fs::read(&upload.certificate),
            ) {
                (Ok(key), Ok(certificate)) => install(dir, &key, &certificate, verb == "replace"),
                _ => Outcome::Refused {
                    reason: "upload",
                    detail: "the private key or the certificate was not uploaded".into(),
                },
            }
        }
        other => Outcome::Refused {
            reason: "upload",
            detail: format!("unknown request {other:?}"),
        },
    };
    upload.clear();
    Some(outcome)
}

/// Writes the identity into `dir`, which only root can enter, each file readable by root only; both files are written
/// before either replaces the one in place.
fn write_identity(dir: &Path, key: &[u8], certificate: &[u8]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let files = [(mfi::KEY_FILE, key), (mfi::CERTIFICATE_FILE, certificate)];
    let mut written = Vec::new();
    for (name, bytes) in files {
        let tmp = dir.join(format!(".{name}.new"));
        let result = write_private(&tmp, bytes);
        written.push((tmp.clone(), dir.join(name)));
        if let Err(err) = result {
            for (tmp, _) in &written {
                let _ = std::fs::remove_file(tmp);
            }
            return Err(err);
        }
    }
    for (tmp, path) in written {
        std::fs::rename(tmp, path)?;
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // A file left from before keeps its mode; this one is readable by root only whatever it had.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Throwaway test files of the carplay crate: a key, its certificate, and another key.
    const KEY: &[u8] = include_bytes!("../../carplay/testdata/mfi/identity.pk8");
    const KEY_PEM: &[u8] = include_bytes!("../../carplay/testdata/mfi/identity.pk8.pem");
    const CERTIFICATE: &[u8] = include_bytes!("../../carplay/testdata/mfi/certificate.p7b");
    const OTHER_KEY: &[u8] = include_bytes!("../../carplay/testdata/mfi/other.pk8");

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("carbridge-mfi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_good_pair_is_installed_private() {
        let dir = temp_dir("install").join("mfi");
        let outcome = install(&dir, KEY_PEM, CERTIFICATE, false);
        assert!(matches!(
            outcome,
            Outcome::Installed {
                replaced: false,
                ..
            }
        ));
        assert!(outcome
            .describe()
            .starts_with("MFi identity installed: certificate "));
        assert_eq!(std::fs::read(dir.join(mfi::KEY_FILE)).unwrap(), KEY);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(mfi::KEY_FILE)), 0o600);
        assert_eq!(mode(&dir.join(mfi::CERTIFICATE_FILE)), 0o600);
        assert!(mfi::inspect(&dir).usable());
        // No files of the writing are left.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);

        // A usable identity is replaced only when asked to.
        let again = install(&dir, KEY, CERTIFICATE, false);
        assert_eq!(
            again.status(2, true),
            MfiInstall {
                attempt: 2,
                result: "refused",
                reason: Some("present"),
                detail: Some(
                    "a usable identity is installed; replacing it needs confirming".into()
                ),
                restart_needed: false,
            }
        );
        let replaced = install(&dir, KEY, CERTIFICATE, true);
        assert!(matches!(
            replaced,
            Outcome::Installed { replaced: true, .. }
        ));
        assert!(replaced.status(3, true).restart_needed);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_broken_identity_is_replaced_without_asking() {
        let dir = temp_dir("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(mfi::KEY_FILE), OTHER_KEY).unwrap();
        std::fs::write(dir.join(mfi::CERTIFICATE_FILE), CERTIFICATE).unwrap();
        assert!(matches!(
            install(&dir, KEY, CERTIFICATE, false),
            Outcome::Installed {
                replaced: false,
                ..
            }
        ));
        assert_eq!(mode(&dir.join(mfi::KEY_FILE)), 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_pairs_are_refused_and_nothing_is_written() {
        let dir = temp_dir("refused");
        let refused = |key: &[u8], certificate: &[u8]| match install(&dir, key, certificate, true) {
            Outcome::Refused { reason, .. } => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(refused(OTHER_KEY, CERTIFICATE), "mismatch");
        assert_eq!(refused(b"garbage", CERTIFICATE), "upload");
        assert_eq!(refused(KEY, b"\x30garbage"), "bad-certificate");
        assert_eq!(refused(CERTIFICATE, KEY), "bad-key");
        assert_eq!(refused(b"", CERTIFICATE), "upload");
        assert!(!dir.exists());
    }

    #[test]
    fn requests_of_the_web_interface() {
        let run = temp_dir("requests");
        std::fs::create_dir_all(&run).unwrap();
        let dir = run.join("mfi");
        let upload = Upload::next_to(&run.join("mfi-install"));
        assert_eq!(upload.key, run.join("mfi-identity.pk8"));
        assert_eq!(take_request(&upload, &dir), None);

        // A file missing from the upload.
        std::fs::write(&upload.key, KEY).unwrap();
        std::fs::write(&upload.request, "install\n").unwrap();
        assert!(matches!(
            take_request(&upload, &dir),
            Some(Outcome::Refused {
                reason: "upload",
                ..
            })
        ));
        assert!(!upload.key.exists() && !upload.request.exists());

        // Both, in base64 as the web interface writes them.
        use base64ct::{Base64, Encoding};
        std::fs::write(&upload.key, Base64::encode_string(KEY)).unwrap();
        std::fs::write(&upload.certificate, Base64::encode_string(CERTIFICATE)).unwrap();
        std::fs::write(&upload.request, "install\n").unwrap();
        assert!(matches!(
            take_request(&upload, &dir),
            Some(Outcome::Installed {
                replaced: false,
                ..
            })
        ));
        for path in [&upload.key, &upload.certificate, &upload.request] {
            assert!(!path.exists());
        }
        let status = MfiStatus::new(&dir, &mfi::inspect(&dir), None);
        assert!(status.usable);
        assert_eq!((status.key, status.certificate), ("present", "present"));
        assert_eq!(status.certificate_info.unwrap().serial, "1234abcd");
        // The status never carries the key.
        let json = serde_json::to_string(&MfiStatus::new(&dir, &mfi::inspect(&dir), None)).unwrap();
        assert!(!json.contains(&Base64::encode_string(KEY)));

        std::fs::write(&upload.request, "erase\n").unwrap();
        assert!(matches!(
            take_request(&upload, &dir),
            Some(Outcome::Refused {
                reason: "upload",
                ..
            })
        ));
        std::fs::remove_dir_all(run).unwrap();
    }

    #[test]
    fn described_for_the_command_line() {
        let dir = temp_dir("describe");
        assert!(install(&dir, KEY, CERTIFICATE, false)
            .describe()
            .contains("installed"));
        let text = describe(&dir, &mfi::inspect(&dir));
        assert!(text.contains("  serial: 1234abcd\n"));
        assert!(text.ends_with("  usable: yes\n"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            describe(&dir, &mfi::inspect(&dir)).ends_with("usable: no, identity.pk8 is missing\n")
        );
    }

    #[test]
    fn the_status_of_a_missing_identity() {
        let dir = temp_dir("missing");
        let status = MfiStatus::new(&dir, &mfi::inspect(&dir), None);
        assert_eq!(
            (status.key, status.certificate, status.usable),
            ("missing", "missing", false)
        );
        assert_eq!(status.problem, Some("missing"));
        assert_eq!(status.certificate_info, None);
    }
}
