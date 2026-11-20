// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What CarBridge keeps across connections: the phone's identity and key, and the keys of the head units it paired
//! with. One file each, readable by root only.
//!
//! - `phone.id`: the identity the phone gives the head unit, six hex digits.
//! - `phone.key`: the phone auth key's private scalar, hex.
//! - `head-units/<id>.pub`: a head unit's auth public key, hex, by the head unit's car ID or address.
//! - `head-units/<id>.name`: the name the head unit advertises, when it gives one.
//! - `head-units/<id>.tile`: what the head unit's certificate names it, JSON, for CarPlay's car tile.
//! - `head-units/<id>.picture`: the picture the head unit's configuration asks for, JSON, for CarPlay's picture.
//! - `head-units/<id>.window`: the head unit's window mode, `mirror` or `simple`, for the shape of CarPlay's picture
//!   in its window.
//! - `head-units/<id>.settings`: the head unit's own settings, JSON ([`HeadUnitSettings`]); none while it has none.
//! - `head-units/<id>.views`: the views of CarPlay measured from the head unit's touches in each of its modes, JSON
//!   ([`Views`]); none while none is measured.
//! - `head-units/<id>.network`: the kind of network the head unit opened that the phone joined last, `wifi-direct` or
//!   `access-point`, asked for first at its next connection.
//! - `head-units/<id>.alias`: for a head unit with no key of its own that authenticated with the key paired with last,
//!   the ID that key is kept under, for it to be found again whatever head unit pairs next.
//! - `head-units/last.pub`: the key of the head unit paired with last.
//! - `head-units/last-connected`: the ID of the head unit connected with last.
//!
//! A head unit's name, tile, picture, window mode, settings, views and network are kept under the ID of the key it connected
//! with: its own, else the one its alias names, else the ID of the key paired with last ([`Store::key_owner`]).
//! Forgetting the head unit removes them with the key, and the aliases that name it.

use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use carbridge_carlink_ble::exchange::NetworkType;
use carbridge_carlink_proto::auth::{AuthKey, PublicKey};
use carbridge_carlink_proto::certificate::HeadUnitIdentity;
use carbridge_carlink_session::display::WindowMode;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};

use crate::picture::HeadUnitPicture;
use crate::settings::HeadUnitSettings;
use crate::views::Views;
use crate::wifi::{network_name, parse_network};

/// A head unit the phone paired with, as the web interface lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SavedHeadUnit {
    /// The name its key is kept under: its car ID or address, lower-case hex digits only.
    pub id: String,
    /// The name it advertises, else what its certificate names it, if either is known.
    pub name: Option<String>,
    /// When it was paired, in Unix seconds.
    pub paired: u64,
    /// It is the head unit paired with last, whose key is also tried for head units that give no car ID.
    pub last: bool,
    /// The picture its configuration asked for last, if it is kept.
    pub picture: Option<HeadUnitPicture>,
    /// Its own settings.
    pub settings: HeadUnitSettings,
    /// The views measured of it.
    pub views: Views,
}

/// A `<id>.tile` file.
#[derive(Serialize, Deserialize)]
struct TileFile {
    short_name: Option<String>,
    product_name: Option<String>,
    vendor_id: Option<String>,
}

impl From<&HeadUnitIdentity> for TileFile {
    fn from(identity: &HeadUnitIdentity) -> TileFile {
        TileFile {
            short_name: identity.short_name.clone(),
            product_name: identity.product_name.clone(),
            vendor_id: identity.vendor_id.clone(),
        }
    }
}

impl From<TileFile> for HeadUnitIdentity {
    fn from(file: TileFile) -> HeadUnitIdentity {
        HeadUnitIdentity {
            short_name: file.short_name,
            product_name: file.product_name,
            vendor_id: file.vendor_id,
        }
    }
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Opens the store in `dir`, creating the directory if needed.
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir.join("head-units"))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Store {
            dir: dir.to_owned(),
        })
    }

    /// The phone's identity, made on first use.
    pub fn phone_id(&self) -> io::Result<String> {
        let path = self.dir.join("phone.id");
        if let Some(id) = read_trimmed(&path)?.filter(|id| !id.is_empty()) {
            return Ok(id);
        }
        let mut bytes = [0u8; 3];
        OsRng.fill_bytes(&mut bytes);
        let id = hex(&bytes);
        write_private(&path, &id)?;
        Ok(id)
    }

    /// The phone auth key, made on first use.
    pub fn phone_key(&self) -> io::Result<AuthKey> {
        let path = self.dir.join("phone.key");
        if let Some(text) = read_trimmed(&path)? {
            if let Some(key) = unhex::<32>(&text).and_then(|bytes| AuthKey::from_bytes(&bytes).ok())
            {
                return Ok(key);
            }
        }
        let key = AuthKey::generate();
        write_private(&path, &hex(&key.to_bytes()))?;
        Ok(key)
    }

    fn head_unit_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.pub", file_name(id)))
    }

    fn name_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.name", file_name(id)))
    }

    fn tile_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.tile", file_name(id)))
    }

    fn picture_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.picture", file_name(id)))
    }

    fn window_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.window", file_name(id)))
    }

    fn settings_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.settings", file_name(id)))
    }

    fn views_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.views", file_name(id)))
    }

    fn network_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.network", file_name(id)))
    }

    fn alias_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("head-units")
            .join(format!("{}.alias", file_name(id)))
    }

    fn last_path(&self) -> PathBuf {
        self.dir.join("head-units").join("last.pub")
    }

    fn last_connected_path(&self) -> PathBuf {
        self.dir.join("head-units").join("last-connected")
    }

    /// The auth key of the head unit `id`, if the phone paired with it.
    fn read_key(path: &Path) -> io::Result<Option<PublicKey>> {
        Ok(read_trimmed(path)?
            .and_then(|text| unhex::<64>(&text))
            .and_then(|bytes| PublicKey::from_wire(&bytes).ok()))
    }

    /// The auth key of the head unit `id`, if the phone paired with it.
    pub fn car_key(&self, id: &str) -> io::Result<Option<PublicKey>> {
        Store::read_key(&self.head_unit_path(id))
    }

    /// The ID whose key the head unit `id` authenticated with, as its alias names it, while that key is kept.
    pub fn alias(&self, id: &str) -> io::Result<Option<String>> {
        let Some(owner) = read_trimmed(&self.alias_path(id))?.map(|owner| file_name(&owner)) else {
            return Ok(None);
        };
        if owner == file_name(id) || self.car_key(&owner)?.is_none() {
            return Ok(None);
        }
        Ok(Some(owner))
    }

    /// Notes that the head unit `id`, which has no key of its own, authenticated with the key kept under `owner`;
    /// written only when it changed.
    pub fn set_alias(&self, id: &str, owner: &str) -> io::Result<()> {
        let owner = file_name(owner);
        if owner == file_name(id) {
            return Ok(());
        }
        let path = self.alias_path(id);
        if read_trimmed(&path)?.as_deref() == Some(owner.as_str()) {
            return Ok(());
        }
        write_private(&path, &owner)
    }

    /// Drops the alias of the head unit `id`, leaving the key it named as it is.
    pub fn forget_alias(&self, id: &str) -> io::Result<()> {
        remove(&self.alias_path(id))
    }

    /// The key the head unit `id` is known by and the ID it is kept under: its own, else the one its alias names.
    pub fn known_key(&self, id: &str) -> io::Result<Option<(PublicKey, String)>> {
        if let Some(key) = self.car_key(id)? {
            return Ok(Some((key, file_name(id))));
        }
        match self.alias(id)? {
            Some(owner) => Ok(self.car_key(&owner)?.map(|key| (key, owner))),
            None => Ok(None),
        }
    }

    /// The auth key of the head unit the phone paired with last. Head units that advertise from a changing random
    /// address and give no car ID can only be found again this way.
    pub fn last_car_key(&self) -> io::Result<Option<PublicKey>> {
        Store::read_key(&self.last_path())
    }

    pub fn set_car_key(&self, id: &str, key: &PublicKey) -> io::Result<()> {
        let text = hex(&key.to_wire());
        write_private(&self.head_unit_path(id), &text)?;
        write_private(&self.last_path(), &text)
    }

    /// Keeps the name the head unit `id` advertises, for the list of head units; written only when it changed.
    pub fn set_car_name(&self, id: &str, name: &str) -> io::Result<()> {
        let path = self.name_path(id);
        if read_trimmed(&path)?.as_deref() == Some(name) {
            return Ok(());
        }
        write_private(&path, name)
    }

    /// The ID that owns the key a connection to the head unit `id` authenticates with: `id` itself when it has a key
    /// of its own, else the one its alias names, else the head unit whose key is the one paired with last, the latest
    /// paired if several are. `None` when none is kept.
    pub fn key_owner(&self, id: &str) -> io::Result<Option<String>> {
        if let Some((_, owner)) = self.known_key(id)? {
            return Ok(Some(owner));
        }
        Ok(self
            .head_units()?
            .into_iter()
            .find(|unit| unit.last)
            .map(|unit| unit.id))
    }

    /// Keeps what the certificate of the head unit `id` names it; written only when it changed.
    pub fn set_car_tile(&self, id: &str, identity: &HeadUnitIdentity) -> io::Result<()> {
        let text = serde_json::to_string(&TileFile::from(identity)).map_err(io::Error::other)?;
        let path = self.tile_path(id);
        if read_trimmed(&path)?.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        write_private(&path, &text)
    }

    /// What the certificate of the head unit `id` named it, if it is kept and readable.
    pub fn car_tile(&self, id: &str) -> io::Result<Option<HeadUnitIdentity>> {
        Ok(read_trimmed(&self.tile_path(id))?
            .and_then(|text| serde_json::from_str::<TileFile>(&text).ok())
            .map(HeadUnitIdentity::from))
    }

    /// Keeps the picture the configuration of the head unit `id` asks for; written only when it changed.
    pub fn set_car_picture(&self, id: &str, picture: &HeadUnitPicture) -> io::Result<()> {
        let text = serde_json::to_string(picture).map_err(io::Error::other)?;
        let path = self.picture_path(id);
        if read_trimmed(&path)?.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        write_private(&path, &text)
    }

    /// The picture the configuration of the head unit `id` asked for, if it is kept and readable.
    pub fn car_picture(&self, id: &str) -> io::Result<Option<HeadUnitPicture>> {
        Ok(read_trimmed(&self.picture_path(id))?
            .and_then(|text| serde_json::from_str::<HeadUnitPicture>(&text).ok()))
    }

    /// Keeps the window mode the head unit `id` casts in; written only when it changed.
    pub fn set_car_window(&self, id: &str, mode: WindowMode) -> io::Result<()> {
        let path = self.window_path(id);
        if read_trimmed(&path)?.as_deref() == Some(mode.name()) {
            return Ok(());
        }
        write_private(&path, mode.name())
    }

    /// The window mode the head unit `id` cast in last, if it is kept and readable.
    pub fn car_window(&self, id: &str) -> io::Result<Option<WindowMode>> {
        Ok(read_trimmed(&self.window_path(id))?.and_then(|name| WindowMode::from_name(&name)))
    }

    /// Keeps the settings of the head unit `id`; written only when they changed, and removed when it has none of its
    /// own.
    pub fn set_car_settings(&self, id: &str, settings: &HeadUnitSettings) -> io::Result<()> {
        let path = self.settings_path(id);
        if settings.is_default() {
            return remove(&path);
        }
        let text = settings.to_json();
        if read_trimmed(&path)?.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        write_private(&path, &text)
    }

    /// The settings of the head unit `id`: none of its own when none are kept, and only those that can be read.
    pub fn car_settings(&self, id: &str) -> io::Result<HeadUnitSettings> {
        Ok(read_trimmed(&self.settings_path(id))?
            .map(|text| HeadUnitSettings::from_json(&text))
            .unwrap_or_default())
    }

    /// Keeps the views measured of the head unit `id`; written only when they changed, and removed when none is.
    pub fn set_car_views(&self, id: &str, views: &Views) -> io::Result<()> {
        let path = self.views_path(id);
        if views.is_empty() {
            return remove(&path);
        }
        let text = views.to_json();
        if read_trimmed(&path)?.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        write_private(&path, &text)
    }

    /// The views measured of the head unit `id`: none when none are kept, and only those that can be read.
    pub fn car_views(&self, id: &str) -> io::Result<Views> {
        Ok(read_trimmed(&self.views_path(id))?
            .map(|text| Views::from_json(&text))
            .unwrap_or_default())
    }

    /// The head unit connected with last, while it is still paired, and the views measured of it.
    pub fn last_connected_views(&self) -> io::Result<Views> {
        match self.last_connected()? {
            Some(id) => self.car_views(&id),
            None => Ok(Views::default()),
        }
    }

    /// Keeps the kind of network the phone joined the head unit `id` on; written only when it changed.
    pub fn set_car_network(&self, id: &str, kind: NetworkType) -> io::Result<()> {
        let path = self.network_path(id);
        if read_trimmed(&path)?.as_deref() == Some(network_name(kind)) {
            return Ok(());
        }
        write_private(&path, network_name(kind))
    }

    /// The kind of network the phone joined the head unit `id` on last, if it is kept and readable.
    pub fn car_network(&self, id: &str) -> io::Result<Option<NetworkType>> {
        Ok(read_trimmed(&self.network_path(id))?.and_then(|name| parse_network(&name)))
    }

    /// Whether the phone paired with the head unit `id`.
    pub fn paired(&self, id: &str) -> io::Result<bool> {
        Ok(self.car_key(id)?.is_some())
    }

    /// The window mode the head unit connected with last cast in, while it is still paired and that is kept.
    pub fn last_connected_window(&self) -> io::Result<Option<WindowMode>> {
        let Some(id) = self.last_connected()? else {
            return Ok(None);
        };
        self.car_window(&id)
    }

    /// Keeps `id` as the head unit connected with last; written only when it changed.
    pub fn set_last_connected(&self, id: &str) -> io::Result<()> {
        let id = file_name(id);
        let path = self.last_connected_path();
        if read_trimmed(&path)?.as_deref() == Some(id.as_str()) {
            return Ok(());
        }
        write_private(&path, &id)
    }

    /// Whether `id` is the head unit connected with last.
    pub fn connected_last(&self, id: &str) -> io::Result<bool> {
        Ok(read_trimmed(&self.last_connected_path())?.as_deref() == Some(file_name(id).as_str()))
    }

    /// The head unit connected with last, while it is still paired.
    pub fn last_connected(&self) -> io::Result<Option<String>> {
        let Some(id) = read_trimmed(&self.last_connected_path())?.filter(|id| !id.is_empty())
        else {
            return Ok(None);
        };
        Ok(self.car_key(&id)?.map(|_| id))
    }

    /// The head unit connected with last, while it is still paired, and what its certificate named it, if that is
    /// kept: what names CarPlay's car tile before any head unit connects.
    pub fn last_connected_tile(&self) -> io::Result<Option<(String, HeadUnitIdentity)>> {
        let Some(id) = self.last_connected()? else {
            return Ok(None);
        };
        Ok(self.car_tile(&id)?.map(|identity| (id, identity)))
    }

    /// The head unit connected with last, while it is still paired, and the picture its configuration asked for, if
    /// that is kept: what CarPlay's picture follows before any head unit connects.
    pub fn last_connected_picture(&self) -> io::Result<Option<(String, HeadUnitPicture)>> {
        let Some(id) = self.last_connected()? else {
            return Ok(None);
        };
        Ok(self.car_picture(&id)?.map(|picture| (id, picture)))
    }

    /// Forgets `key`, kept for `id` or as the last head unit's.
    pub fn forget_car_key(&self, id: &str, key: &PublicKey) -> io::Result<()> {
        for path in [self.head_unit_path(id), self.last_path()] {
            if Store::read_key(&path)?.as_ref() == Some(key) {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Forgets the head unit `id`: its key, its name, its tile, its picture, its window mode, its settings, its views,
    /// its network, its key as the last head unit's, and that it connected last. Returns whether it was kept. It pairs
    /// again with the PIN it shows.
    pub fn forget_head_unit(&self, id: &str) -> io::Result<bool> {
        let path = self.head_unit_path(id);
        let Some(key) = Store::read_key(&path)? else {
            return Ok(false);
        };
        if Store::read_key(&self.last_path())? == Some(key) {
            remove(&self.last_path())?;
        }
        if self.connected_last(id)? {
            remove(&self.last_connected_path())?;
        }
        remove(&self.name_path(id))?;
        remove(&self.tile_path(id))?;
        remove(&self.picture_path(id))?;
        remove(&self.window_path(id))?;
        remove(&self.settings_path(id))?;
        remove(&self.views_path(id))?;
        remove(&self.network_path(id))?;
        remove(&self.alias_path(id))?;
        // The head units that went by its key go with it.
        let owner = file_name(id);
        for entry in std::fs::read_dir(self.dir.join("head-units"))? {
            let alias = entry?.path();
            if alias.extension().is_some_and(|ext| ext == "alias")
                && read_trimmed(&alias)?
                    .map(|named| file_name(&named))
                    .as_deref()
                    == Some(owner.as_str())
            {
                remove(&alias)?;
            }
        }
        remove(&path)?;
        Ok(true)
    }

    /// The head units the phone paired with, the latest paired first.
    pub fn head_units(&self) -> io::Result<Vec<SavedHeadUnit>> {
        let last = Store::read_key(&self.last_path())?;
        let mut units = Vec::new();
        for entry in std::fs::read_dir(self.dir.join("head-units"))? {
            let path = entry?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".pub"))
                .filter(|id| *id != "last")
            else {
                continue;
            };
            let Some(key) = Store::read_key(&path)? else {
                continue;
            };
            let paired = std::fs::metadata(&path)?
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            units.push(SavedHeadUnit {
                id: id.to_owned(),
                name: match read_trimmed(&self.name_path(id))?.filter(|name| !name.is_empty()) {
                    Some(name) => Some(name),
                    None => self
                        .car_tile(id)?
                        .and_then(|tile| tile.short_name.or(tile.product_name)),
                },
                paired,
                last: last == Some(key),
                picture: self.car_picture(id)?,
                settings: self.car_settings(id)?,
                views: self.car_views(id)?,
            });
        }
        units.sort_by(|a, b| b.paired.cmp(&a.paired).then_with(|| a.id.cmp(&b.id)));
        Ok(units)
    }
}

/// The name a head unit's files are kept under: its ID's letters and digits, lower-case.
pub fn file_name(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// Removes a file that may not be there.
fn remove(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

fn read_trimmed(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text.trim().to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("new");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        writeln!(file, "{text}")?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
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
            std::env::temp_dir().join(format!("carbridge-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn identity_is_kept() {
        let dir = temp_dir("identity");
        let store = Store::open(&dir).unwrap();
        let id = store.phone_id().unwrap();
        assert_eq!(id.len(), 6);
        assert_eq!(store.phone_id().unwrap(), id);
        let key = store.phone_key().unwrap();
        assert_eq!(
            Store::open(&dir).unwrap().phone_key().unwrap().public_key(),
            key.public_key()
        );
        let mode = std::fs::metadata(dir.join("phone.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn head_unit_keys() {
        let dir = temp_dir("cars");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        assert_eq!(store.car_key("AA:BB:CC:DD:EE:FF").unwrap(), None);
        assert_eq!(store.last_car_key().unwrap(), None);
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert_eq!(store.car_key("aa:bb:cc:dd:ee:ff").unwrap(), Some(key));
        assert_eq!(store.last_car_key().unwrap(), Some(key));
        let other = AuthKey::generate().public_key();
        store.forget_car_key("AA:BB:CC:DD:EE:FF", &other).unwrap();
        assert_eq!(store.car_key("AA:BB:CC:DD:EE:FF").unwrap(), Some(key));
        store.forget_car_key("11:22:33:44:55:66", &key).unwrap();
        assert_eq!(store.last_car_key().unwrap(), None);
        assert_eq!(store.car_key("AA:BB:CC:DD:EE:FF").unwrap(), Some(key));
        store.forget_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert_eq!(store.car_key("AA:BB:CC:DD:EE:FF").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_head_unit_goes_by_its_alias_whatever_pairs_next() {
        let dir = temp_dir("aliases");
        let store = Store::open(&dir).unwrap();
        let tablet = AuthKey::generate().public_key();
        let car = AuthKey::generate().public_key();
        // The key of one head unit, which another ID took as the key paired with last.
        store.set_car_key("a1b2c3d4e5f6", &tablet).unwrap();
        assert_eq!(
            store.key_owner("020000000303").unwrap().as_deref(),
            Some("a1b2c3d4e5f6")
        );
        assert_eq!(store.known_key("020000000303").unwrap(), None);
        store
            .set_alias("02:00:00:00:03:03", "a1b2c3d4e5f6")
            .unwrap();
        // Another head unit pairs: the alias still names the first key.
        store.set_car_key("304ac43f341b", &car).unwrap();
        assert_eq!(store.last_car_key().unwrap(), Some(car));
        assert_eq!(
            store.known_key("020000000303").unwrap(),
            Some((tablet, "a1b2c3d4e5f6".to_owned()))
        );
        assert_eq!(
            store.key_owner("020000000303").unwrap().as_deref(),
            Some("a1b2c3d4e5f6")
        );
        // An alias of an ID to itself is not kept; the list of head units ignores aliases.
        store.set_alias("304ac43f341b", "304ac43f341b").unwrap();
        assert_eq!(store.alias("304ac43f341b").unwrap(), None);
        assert_eq!(store.head_units().unwrap().len(), 2);
        // Dropping the alias leaves the key.
        store.forget_alias("020000000303").unwrap();
        assert_eq!(store.known_key("020000000303").unwrap(), None);
        assert_eq!(store.car_key("a1b2c3d4e5f6").unwrap(), Some(tablet));
        // Forgetting the head unit removes the aliases that name it.
        store.set_alias("020000000303", "a1b2c3d4e5f6").unwrap();
        assert!(store.forget_head_unit("a1b2c3d4e5f6").unwrap());
        assert!(!dir.join("head-units/020000000303.alias").exists());
        assert_eq!(store.alias("020000000303").unwrap(), None);
        assert_eq!(store.car_key("304ac43f341b").unwrap(), Some(car));
    }

    #[test]
    fn an_alias_to_a_forgotten_key_is_none() {
        let dir = temp_dir("stale-alias");
        let store = Store::open(&dir).unwrap();
        store.set_alias("aabbccddeeff", "112233445566").unwrap();
        assert_eq!(store.alias("aabbccddeeff").unwrap(), None);
        assert_eq!(store.known_key("aabbccddeeff").unwrap(), None);
    }

    #[test]
    fn list_and_forget_head_units() {
        let dir = temp_dir("list");
        let store = Store::open(&dir).unwrap();
        assert!(store.head_units().unwrap().is_empty());
        let first = AuthKey::generate().public_key();
        let second = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &first).unwrap();
        store.set_car_name("AA:BB:CC:DD:EE:FF", "C10").unwrap();
        store.set_car_key("11:22:33:44:55:66", &second).unwrap();

        let units = store.head_units().unwrap();
        assert_eq!(units.len(), 2);
        let unit = |id: &str| units.iter().find(|u| u.id == id).unwrap().clone();
        assert_eq!(unit("aabbccddeeff").name.as_deref(), Some("C10"));
        assert!(!unit("aabbccddeeff").last);
        assert_eq!(unit("112233445566").name, None);
        assert!(unit("112233445566").last);

        assert!(store.forget_head_unit("112233445566").unwrap());
        assert!(!store.forget_head_unit("112233445566").unwrap());
        assert_eq!(store.last_car_key().unwrap(), None);
        assert_eq!(store.car_key("AA:BB:CC:DD:EE:FF").unwrap(), Some(first));
        assert!(store.forget_head_unit("AA:BB:CC:DD:EE:FF").unwrap());
        assert!(store.head_units().unwrap().is_empty());
        assert!(!dir.join("head-units/aabbccddeeff.name").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn c10() -> HeadUnitIdentity {
        HeadUnitIdentity {
            short_name: Some("零跑C10".into()),
            product_name: Some("零跑C10 2024款".into()),
            vendor_id: Some("0028".into()),
        }
    }

    #[test]
    fn listed_by_tile_without_a_name() {
        let dir = temp_dir("tile-names");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("aabbccddeeff", &key).unwrap();
        let name = |store: &Store| store.head_units().unwrap()[0].name.clone();
        assert_eq!(name(&store), None);
        store.set_car_tile("aabbccddeeff", &c10()).unwrap();
        assert_eq!(name(&store).as_deref(), Some("零跑C10"));
        let product_only = HeadUnitIdentity {
            short_name: None,
            ..c10()
        };
        store.set_car_tile("aabbccddeeff", &product_only).unwrap();
        assert_eq!(name(&store).as_deref(), Some("零跑C10 2024款"));
        store.set_car_name("aabbccddeeff", "C10").unwrap();
        assert_eq!(name(&store).as_deref(), Some("C10"));
    }

    #[test]
    fn tiles_are_kept_and_forgotten() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp_dir("tiles");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert_eq!(store.car_tile("aabbccddeeff").unwrap(), None);
        store.set_car_tile("AA:BB:CC:DD:EE:FF", &c10()).unwrap();
        assert_eq!(store.car_tile("aabbccddeeff").unwrap(), Some(c10()));
        let path = dir.join("head-units/aabbccddeeff.tile");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // The same tile again leaves the file as it is; another replaces it.
        let inode = std::fs::metadata(&path).unwrap().ino();
        store.set_car_tile("aabbccddeeff", &c10()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let unnamed = HeadUnitIdentity {
            vendor_id: Some("0028".into()),
            ..HeadUnitIdentity::default()
        };
        store.set_car_tile("aabbccddeeff", &unnamed).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(store.car_tile("aabbccddeeff").unwrap(), Some(unnamed));

        // A tile file that is not one is no tile.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(store.car_tile("aabbccddeeff").unwrap(), None);

        store.set_car_tile("aabbccddeeff", &c10()).unwrap();
        store.set_last_connected("aabbccddeeff").unwrap();
        // Listing head units does not take the tile or the marker for one.
        assert_eq!(store.head_units().unwrap().len(), 1);
        assert!(store.forget_head_unit("AA:BB:CC:DD:EE:FF").unwrap());
        assert!(!path.exists());
        assert!(!dir.join("head-units/last-connected").exists());
        assert_eq!(store.car_tile("aabbccddeeff").unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tablet() -> HeadUnitPicture {
        HeadUnitPicture {
            width: 2458,
            height: 1484,
            dpi: 320,
            real_dpi: 206,
            fps: 30,
            screen_width: 2560,
            screen_height: 1600,
        }
    }

    #[test]
    fn pictures_are_kept_and_forgotten() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp_dir("pictures");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert_eq!(store.car_picture("aabbccddeeff").unwrap(), None);
        assert_eq!(store.last_connected_picture().unwrap(), None);
        store
            .set_car_picture("AA:BB:CC:DD:EE:FF", &tablet())
            .unwrap();
        assert_eq!(store.car_picture("aabbccddeeff").unwrap(), Some(tablet()));
        let path = dir.join("head-units/aabbccddeeff.picture");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        // The same picture again leaves the file as it is; another replaces it.
        let inode = std::fs::metadata(&path).unwrap().ino();
        store.set_car_picture("aabbccddeeff", &tablet()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let fast = HeadUnitPicture {
            fps: 60,
            ..tablet()
        };
        store.set_car_picture("aabbccddeeff", &fast).unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(store.car_picture("aabbccddeeff").unwrap(), Some(fast));

        // A picture file that is not one is no picture.
        std::fs::write(&path, "2458x1484").unwrap();
        assert_eq!(store.car_picture("aabbccddeeff").unwrap(), None);

        // The head unit connected last gives the picture at startup, until it is forgotten.
        store.set_car_picture("aabbccddeeff", &tablet()).unwrap();
        store.set_last_connected("aabbccddeeff").unwrap();
        assert_eq!(
            Store::open(&dir).unwrap().last_connected_picture().unwrap(),
            Some(("aabbccddeeff".to_owned(), tablet()))
        );
        assert_eq!(store.head_units().unwrap().len(), 1);
        assert!(store.forget_head_unit("AA:BB:CC:DD:EE:FF").unwrap());
        assert!(!path.exists());
        assert_eq!(store.last_connected_picture().unwrap(), None);

        // A marker left for a head unit whose key is gone gives none either.
        store.set_car_key("aabbccddeeff", &key).unwrap();
        store.set_car_picture("aabbccddeeff", &tablet()).unwrap();
        store.set_last_connected("aabbccddeeff").unwrap();
        store.forget_car_key("aabbccddeeff", &key).unwrap();
        assert_eq!(store.last_connected_picture().unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn window_modes_are_kept_and_forgotten() {
        let dir = temp_dir("windows");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert_eq!(store.car_window("aabbccddeeff").unwrap(), None);
        store
            .set_car_window("AA:BB:CC:DD:EE:FF", WindowMode::Simple)
            .unwrap();
        assert_eq!(
            store.car_window("aabbccddeeff").unwrap(),
            Some(WindowMode::Simple)
        );
        let path = dir.join("head-units/aabbccddeeff.window");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "simple");
        // Before it connected last, it gives no window mode at startup.
        assert_eq!(store.last_connected_window().unwrap(), None);
        store.set_last_connected("aabbccddeeff").unwrap();
        store
            .set_car_window("aabbccddeeff", WindowMode::Mirror)
            .unwrap();
        assert_eq!(
            Store::open(&dir).unwrap().last_connected_window().unwrap(),
            Some(WindowMode::Mirror)
        );
        // A file that names no mode is none.
        std::fs::write(&path, "sideways").unwrap();
        assert_eq!(store.car_window("aabbccddeeff").unwrap(), None);
        assert!(store.forget_head_unit("aabbccddeeff").unwrap());
        assert!(!path.exists());
        assert_eq!(store.last_connected_window().unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_are_kept_and_forgotten() {
        use crate::options::Hevc;
        use crate::settings::FullScreen;
        use std::os::unix::fs::MetadataExt;
        let dir = temp_dir("settings");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert!(store.paired("aabbccddeeff").unwrap());
        assert!(!store.paired("112233445566").unwrap());
        assert!(store.car_settings("aabbccddeeff").unwrap().is_default());
        let own = HeadUnitSettings {
            full_screen: Some(FullScreen::Custom(2432, 1484)),
            hevc: Some(Hevc::Off),
            ..HeadUnitSettings::default()
        };
        store.set_car_settings("AA:BB:CC:DD:EE:FF", &own).unwrap();
        assert_eq!(store.car_settings("aabbccddeeff").unwrap(), own);
        let path = dir.join("head-units/aabbccddeeff.settings");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            r#"{"full_screen":"2432x1484","hevc":"off"}"#
        );

        // The same settings again leave the file as it is.
        let inode = std::fs::metadata(&path).unwrap().ino();
        store.set_car_settings("aabbccddeeff", &own).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);

        // The list of head units carries them, with the picture the head unit asked for.
        store.set_car_picture("aabbccddeeff", &tablet()).unwrap();
        let units = store.head_units().unwrap();
        assert_eq!(units[0].settings, own);
        assert_eq!(units[0].picture, Some(tablet()));

        // A file that cannot be read gives what it can.
        std::fs::write(&path, r#"{"full_screen":"odd","hevc":"off","later":true}"#).unwrap();
        assert_eq!(
            store.car_settings("aabbccddeeff").unwrap(),
            HeadUnitSettings {
                hevc: Some(Hevc::Off),
                ..HeadUnitSettings::default()
            }
        );

        // None of its own removes the file.
        store
            .set_car_settings("aabbccddeeff", &HeadUnitSettings::default())
            .unwrap();
        assert!(!path.exists());
        store.set_car_settings("aabbccddeeff", &own).unwrap();
        assert!(store.forget_head_unit("AA:BB:CC:DD:EE:FF").unwrap());
        assert!(!path.exists());
        assert!(store.car_settings("aabbccddeeff").unwrap().is_default());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn views_are_kept_and_forgotten() {
        use crate::views::Measured;
        use carbridge_carlink_session::display::PictureSize;
        use std::os::unix::fs::MetadataExt;
        let dir = temp_dir("views");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert!(store.car_views("aabbccddeeff").unwrap().is_empty());
        let size = |width, height| PictureSize { width, height };
        let views = Views {
            landscape_window: Some(Measured {
                view: size(1712, 1034),
                picture: size(2458, 1484),
            }),
            ..Views::default()
        };
        store.set_car_views("AA:BB:CC:DD:EE:FF", &views).unwrap();
        assert_eq!(store.car_views("aabbccddeeff").unwrap(), views);
        let path = dir.join("head-units/aabbccddeeff.views");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            r#"{"landscape_window":{"picture":"2458x1484","view":"1712x1034"}}"#
        );
        // The same views again leave the file as it is.
        let inode = std::fs::metadata(&path).unwrap().ino();
        store.set_car_views("aabbccddeeff", &views).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        // The list of head units carries them; the head unit connected last gives them at startup.
        assert_eq!(store.head_units().unwrap()[0].views, views);
        assert!(store.last_connected_views().unwrap().is_empty());
        store.set_last_connected("aabbccddeeff").unwrap();
        assert_eq!(store.last_connected_views().unwrap(), views);
        // None measured removes the file.
        store
            .set_car_views("aabbccddeeff", &Views::default())
            .unwrap();
        assert!(!path.exists());
        store.set_car_views("aabbccddeeff", &views).unwrap();
        assert!(store.forget_head_unit("AA:BB:CC:DD:EE:FF").unwrap());
        assert!(!path.exists());
        assert!(store.car_views("aabbccddeeff").unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn networks_are_kept_and_forgotten() {
        use std::os::unix::fs::MetadataExt;
        let dir = temp_dir("networks");
        let store = Store::open(&dir).unwrap();
        let key = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &key).unwrap();
        assert_eq!(store.car_network("aabbccddeeff").unwrap(), None);
        store
            .set_car_network("AA:BB:CC:DD:EE:FF", NetworkType::SoftAp)
            .unwrap();
        assert_eq!(
            store.car_network("aabbccddeeff").unwrap(),
            Some(NetworkType::SoftAp)
        );
        let path = dir.join("head-units/aabbccddeeff.network");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "access-point"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // The same kind again leaves the file as it is; the other replaces it.
        let inode = std::fs::metadata(&path).unwrap().ino();
        store
            .set_car_network("aabbccddeeff", NetworkType::SoftAp)
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        store
            .set_car_network("aabbccddeeff", NetworkType::P2pGroup)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "wifi-direct"
        );
        // A file that names no kind is none.
        std::fs::write(&path, "bluetooth").unwrap();
        assert_eq!(store.car_network("aabbccddeeff").unwrap(), None);
        // Listing head units does not take it; forgetting the head unit removes it.
        assert_eq!(store.head_units().unwrap().len(), 1);
        assert!(store.forget_head_unit("AA:BB:CC:DD:EE:FF").unwrap());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn key_owner_of_a_connection() {
        let dir = temp_dir("owner");
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.key_owner("AA:BB:CC:DD:EE:FF").unwrap(), None);
        let first = AuthKey::generate().public_key();
        let second = AuthKey::generate().public_key();
        store.set_car_key("AA:BB:CC:DD:EE:FF", &first).unwrap();
        store.set_car_key("11:22:33:44:55:66", &second).unwrap();
        // A head unit with a key of its own owns it, the last paired or not.
        assert_eq!(
            store.key_owner("AA:BB:CC:DD:EE:FF").unwrap().as_deref(),
            Some("aabbccddeeff")
        );
        assert_eq!(
            store.key_owner("11:22:33:44:55:66").unwrap().as_deref(),
            Some("112233445566")
        );
        // One that advertises another car ID each time connects with the key paired last.
        assert_eq!(
            store.key_owner("02:00:00:00:04:04").unwrap().as_deref(),
            Some("112233445566")
        );
        assert!(store.forget_head_unit("112233445566").unwrap());
        assert_eq!(store.key_owner("02:00:00:00:04:04").unwrap(), None);
        assert_eq!(
            store.key_owner("AA:BB:CC:DD:EE:FF").unwrap().as_deref(),
            Some("aabbccddeeff")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tile_at_startup() {
        let dir = temp_dir("startup");
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.last_connected_tile().unwrap(), None);
        let first = AuthKey::generate().public_key();
        let second = AuthKey::generate().public_key();
        store.set_car_key("aabbccddeeff", &first).unwrap();
        store.set_car_key("112233445566", &second).unwrap();
        store.set_car_tile("aabbccddeeff", &c10()).unwrap();

        // The head unit connected last names it, also when it is not the one paired last.
        store.set_last_connected("AA:BB:CC:DD:EE:FF").unwrap();
        assert!(store.connected_last("aabbccddeeff").unwrap());
        assert!(!store.connected_last("112233445566").unwrap());
        assert_eq!(
            Store::open(&dir).unwrap().last_connected_tile().unwrap(),
            Some(("aabbccddeeff".to_owned(), c10()))
        );
        // One whose certificate named nothing kept names none.
        store.set_last_connected("112233445566").unwrap();
        assert_eq!(store.last_connected_tile().unwrap(), None);

        // Forgetting another head unit leaves it; forgetting it falls back to none.
        store.set_last_connected("aabbccddeeff").unwrap();
        assert!(store.forget_head_unit("112233445566").unwrap());
        assert_eq!(
            store.last_connected_tile().unwrap(),
            Some(("aabbccddeeff".to_owned(), c10()))
        );
        assert!(store.forget_head_unit("aabbccddeeff").unwrap());
        assert_eq!(store.last_connected_tile().unwrap(), None);

        // A marker left for a head unit whose key is gone names none either.
        store.set_car_key("aabbccddeeff", &first).unwrap();
        store.set_car_tile("aabbccddeeff", &c10()).unwrap();
        store.set_last_connected("aabbccddeeff").unwrap();
        store.forget_car_key("aabbccddeeff", &first).unwrap();
        assert_eq!(store.last_connected_tile().unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
