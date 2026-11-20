// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What the receiver keeps across sessions, one file each and readable by root only:
//!
//! - `identity`: the AirPlay identity, the Ed25519 key pair and pairing ID, in hex and text lines;
//! - `pairings/<identifier>`: each paired iPhone's long-term public key, in hex.

use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use carbridge_airplay::identity::{Identity, Pairings};

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir.join("pairings"))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Store {
            dir: dir.to_owned(),
        })
    }

    /// The AirPlay identity, made on first use.
    pub fn identity(&self) -> io::Result<Identity> {
        let path = self.dir.join("identity");
        if let Some(identity) = read(&path)?.as_deref().and_then(parse_identity) {
            return Ok(identity);
        }
        let identity = Identity::generate();
        write_private(
            &path,
            &format!(
                "{}\n{}\n{}\n",
                hex(&identity.secret_key()),
                hex(&identity.public_key()),
                identity.pairing_id()
            ),
        )?;
        Ok(identity)
    }

    fn pairing_path(&self, identifier: &str) -> PathBuf {
        let name: String = identifier
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect::<String>()
            .to_lowercase();
        self.dir.join("pairings").join(name)
    }
}

impl Pairings for Store {
    fn remember(&self, identifier: &str, long_term_public_key: &[u8; 32]) -> io::Result<()> {
        write_private(
            &self.pairing_path(identifier),
            &format!("{}\n", hex(long_term_public_key)),
        )
    }

    fn lookup(&self, identifier: &str) -> io::Result<Option<[u8; 32]>> {
        Ok(read(&self.pairing_path(identifier))?.and_then(|text| unhex(text.trim())))
    }
}

impl Store {
    /// Forgets every paired iPhone.
    pub fn clear(&self) -> io::Result<()> {
        for entry in std::fs::read_dir(self.dir.join("pairings"))? {
            std::fs::remove_file(entry?.path())?;
        }
        Ok(())
    }
}

/// The identity in the file's text; `None` for one that cannot be read, or whose public key is not its private
/// key's.
fn parse_identity(text: &str) -> Option<Identity> {
    let mut lines = text.lines().map(str::trim);
    let private_key = unhex(lines.next()?)?;
    let public_key = unhex(lines.next()?)?;
    let pairing_id = lines.next().filter(|id| !id.is_empty())?.to_owned();
    let identity = Identity::new(private_key, pairing_id);
    (identity.public_key() == public_key).then_some(identity)
}

fn read(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("new");
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("carbridge-carplay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn identity_is_kept() {
        let dir = temp_dir("identity");
        let first = Store::open(&dir).unwrap().identity().unwrap();
        let again = Store::open(&dir).unwrap().identity().unwrap();
        assert_eq!(
            (first.secret_key(), first.public_key()),
            (again.secret_key(), again.public_key())
        );
        assert_eq!(first.pairing_id(), again.pairing_id());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pairings() {
        let dir = temp_dir("pairings");
        let store = Store::open(&dir).unwrap();
        let id = "5A0F-11E2-AB34-9C";
        assert_eq!(store.lookup(id).unwrap(), None);
        store.remember(id, &[7; 32]).unwrap();
        assert_eq!(store.lookup(id).unwrap(), Some([7; 32]));
        assert_eq!(store.lookup("../escape").unwrap(), None);
        store.clear().unwrap();
        assert_eq!(store.lookup(id).unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
