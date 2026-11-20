// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The car tile on the CarPlay home screen, the one that leads back to the car: its label and its icon.
//!
//! The tile is named after the head unit: its short name, else its product name, else [`DEFAULT_LABEL`]. Its icon is
//! CarBridge's car icon, except for the makers in [`OEM_ICON_VENDORS`], whose head units get `<vendor id>.png` from
//! the OEM icon directory when that file is in place and is a usable icon ([`check_png`]).

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The tile's label before any head unit has named it, and for a head unit that gives no name.
pub const DEFAULT_LABEL: &str = "车辆";
/// CarBridge's car icon: square, opaque, 192 pixels.
pub const CARBRIDGE_ICON: &[u8] = include_bytes!("../assets/car-icon.png");
/// The CarBridge icon's side in pixels.
const CARBRIDGE_ICON_SIDE: u32 = 192;
/// The head unit makers whose own icon is used when it is in place: Leapmotor.
pub const OEM_ICON_VENDORS: &[&str] = &["0028"];
/// The smallest and largest side of an icon file, in pixels.
pub const MIN_SIDE: u32 = 64;
pub const MAX_SIDE: u32 = 512;
/// The largest icon file.
pub const MAX_BYTES: u64 = 256 * 1024;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
/// PNG colour types with an alpha channel: grey with alpha, and RGB with alpha.
const COLOUR_GREY_ALPHA: u8 = 4;
const COLOUR_RGB_ALPHA: u8 = 6;

/// Where the tile's icon comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IconSource {
    CarBridge,
    /// An OEM icon file.
    File(PathBuf),
}

/// A square PNG icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icon {
    pub png: Arc<[u8]>,
    /// Width and height in pixels.
    pub side: u32,
    pub source: IconSource,
}

impl Icon {
    pub fn carbridge() -> Icon {
        Icon {
            png: CARBRIDGE_ICON.into(),
            side: CARBRIDGE_ICON_SIDE,
            source: IconSource::CarBridge,
        }
    }
}

impl fmt::Display for Icon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            IconSource::CarBridge => f.write_str("CarBridge's icon"),
            IconSource::File(path) => write!(f, "{} ({}x{})", path.display(), self.side, self.side),
        }
    }
}

/// The car tile: its label and its icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CarTile {
    pub label: String,
    pub icon: Icon,
}

impl CarTile {
    /// Whether an iPhone given this tile shows `other` as well: the same label and the same icon picture, wherever
    /// the icon comes from.
    pub fn looks_like(&self, other: &CarTile) -> bool {
        self.label == other.label
            && self.icon.side == other.icon.side
            && self.icon.png == other.icon.png
    }
}

impl Default for CarTile {
    /// The tile before any head unit has named it.
    fn default() -> CarTile {
        CarTile {
            label: DEFAULT_LABEL.into(),
            icon: Icon::carbridge(),
        }
    }
}

/// Why an OEM icon file is not used.
#[derive(Debug, PartialEq, Eq)]
pub enum IconError {
    /// There is no such file.
    Missing,
    Unreadable(io::ErrorKind),
    /// The file is larger than [`MAX_BYTES`]; its size in bytes.
    TooLarge(u64),
    NotPng,
    /// The PNG ends before its last chunk.
    Truncated,
    /// Width and height differ.
    NotSquare {
        width: u32,
        height: u32,
    },
    /// The side is outside [`MIN_SIDE`] to [`MAX_SIDE`].
    Side(u32),
    /// The PNG has an alpha channel.
    Alpha,
    /// The PNG has a transparent colour or palette entries (a `tRNS` chunk).
    Transparency,
}

impl fmt::Display for IconError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IconError::Missing => f.write_str("not in place"),
            IconError::Unreadable(kind) => write!(f, "unreadable: {kind}"),
            IconError::TooLarge(bytes) => write!(f, "{bytes} bytes, more than {MAX_BYTES}"),
            IconError::NotPng => f.write_str("not a PNG"),
            IconError::Truncated => f.write_str("a truncated PNG"),
            IconError::NotSquare { width, height } => write!(f, "not square: {width}x{height}"),
            IconError::Side(side) => {
                write!(f, "{side}x{side}, not {MIN_SIDE} to {MAX_SIDE} pixels")
            }
            IconError::Alpha => f.write_str("has an alpha channel; it must be opaque"),
            IconError::Transparency => {
                f.write_str("has transparent colours (tRNS); it must be opaque")
            }
        }
    }
}

/// The tile's label: the head unit's short name, else its product name, else [`DEFAULT_LABEL`]. Blank names count
/// as none.
pub fn label(short_name: Option<&str>, product_name: Option<&str>) -> String {
    [short_name, product_name]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty())
        .unwrap_or(DEFAULT_LABEL)
        .to_owned()
}

/// The OEM icon file for a head unit maker, if the maker has one.
pub fn oem_icon_path(vendor_id: Option<&str>, oem_icon_dir: &Path) -> Option<PathBuf> {
    let vendor = vendor_id?.trim();
    OEM_ICON_VENDORS
        .contains(&vendor)
        .then(|| oem_icon_dir.join(format!("{vendor}.png")))
}

/// Checks that `png` can be the tile's icon: a PNG of at most [`MAX_BYTES`], square, [`MIN_SIDE`] to [`MAX_SIDE`]
/// pixels, and opaque. Opaque means its colour type has no alpha channel and it has no `tRNS` chunk, which gives
/// grey, RGB or palette images transparent colours; the pixels are not decoded. Returns the side in pixels.
pub fn check_png(png: &[u8]) -> Result<u32, IconError> {
    if png.len() as u64 > MAX_BYTES {
        return Err(IconError::TooLarge(png.len() as u64));
    }
    let chunks = png.strip_prefix(PNG_SIGNATURE).ok_or(IconError::NotPng)?;
    let mut header = None;
    let mut transparency = false;
    let mut rest = chunks;
    loop {
        let (kind, data, next) = chunk(rest).ok_or(match header {
            None => IconError::NotPng,
            Some(_) => IconError::Truncated,
        })?;
        match (header, kind) {
            (None, b"IHDR") if data.len() == 13 => header = Some(data),
            (None, _) => return Err(IconError::NotPng),
            (Some(_), b"tRNS") => transparency = true,
            (Some(_), b"IEND") => break,
            _ => {}
        }
        rest = next;
    }
    let header = header.ok_or(IconError::NotPng)?;
    let width = u32::from_be_bytes(header[0..4].try_into().unwrap());
    let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let colour_type = header[9];
    if width != height {
        return Err(IconError::NotSquare { width, height });
    }
    if !(MIN_SIDE..=MAX_SIDE).contains(&width) {
        return Err(IconError::Side(width));
    }
    if matches!(colour_type, COLOUR_GREY_ALPHA | COLOUR_RGB_ALPHA) {
        return Err(IconError::Alpha);
    }
    if transparency {
        return Err(IconError::Transparency);
    }
    Ok(width)
}

/// The first chunk of `bytes`: its type, its data, and what follows it.
fn chunk(bytes: &[u8]) -> Option<(&[u8; 4], &[u8], &[u8])> {
    let length = u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?) as usize;
    let kind = bytes.get(4..8)?.try_into().ok()?;
    let data = bytes.get(8..8usize.checked_add(length)?)?;
    // The data is followed by its CRC.
    let next = bytes.get(8 + length + 4..)?;
    Some((kind, data, next))
}

/// Reads an icon file and checks it ([`check_png`]).
pub fn read_icon(path: &Path) -> Result<Icon, IconError> {
    let file = File::open(path).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => IconError::Missing,
        kind => IconError::Unreadable(kind),
    })?;
    let size = file
        .metadata()
        .map_err(|err| IconError::Unreadable(err.kind()))?
        .len();
    if size > MAX_BYTES {
        return Err(IconError::TooLarge(size));
    }
    let mut png = Vec::with_capacity(size as usize);
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut png)
        .map_err(|err| IconError::Unreadable(err.kind()))?;
    let side = check_png(&png)?;
    Ok(Icon {
        png: png.into(),
        side,
        source: IconSource::File(path.to_owned()),
    })
}

/// The tile for a head unit, and why its maker's icon file was not used, if it has one.
#[derive(Debug)]
pub struct Choice {
    pub tile: CarTile,
    pub refused: Option<(PathBuf, IconError)>,
}

impl Choice {
    /// Chooses the tile for a head unit from what its certificate says, with the OEM icons in `oem_icon_dir`.
    pub fn for_head_unit(
        short_name: Option<&str>,
        product_name: Option<&str>,
        vendor_id: Option<&str>,
        oem_icon_dir: &Path,
    ) -> Choice {
        let label = label(short_name, product_name);
        let (icon, refused) = match oem_icon_path(vendor_id, oem_icon_dir) {
            Some(path) => match read_icon(&path) {
                Ok(icon) => (icon, None),
                Err(err) => (Icon::carbridge(), Some((path, err))),
            },
            None => (Icon::carbridge(), None),
        };
        Choice {
            tile: CarTile { label, icon },
            refused,
        }
    }
}

impl fmt::Display for Choice {
    /// One line for the log: the label, the icon, and why the maker's icon was not used.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "car tile \"{}\" with {}",
            self.tile.label, self.tile.icon
        )?;
        if let Some((path, err)) = &self.refused {
            write!(f, "; {} is {err}", path.display())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG of `width` by `height` in `colour_type`, with `extra` chunks before its image data, padded to `bytes`
    /// with another chunk. CRCs are left zero: they are not checked.
    fn png(width: u32, height: u32, colour_type: u8, extra: &[&[u8; 4]], bytes: usize) -> Vec<u8> {
        fn push(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(kind);
            out.extend(data);
            out.extend([0; 4]);
        }
        let mut out = PNG_SIGNATURE.to_vec();
        let mut header = Vec::new();
        header.extend(width.to_be_bytes());
        header.extend(height.to_be_bytes());
        header.extend([8, colour_type, 0, 0, 0]);
        push(&mut out, b"IHDR", &header);
        for kind in extra {
            push(&mut out, kind, &[0; 3]);
        }
        push(&mut out, b"IDAT", &[0; 16]);
        let padding = bytes.saturating_sub(out.len() + 12 + 12);
        if padding > 0 {
            push(&mut out, b"zTXt", &vec![0; padding]);
        }
        push(&mut out, b"IEND", &[]);
        out
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("carbridge-tile-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn label_choice() {
        assert_eq!(label(Some("C10"), Some("Leapmotor HU")), "C10");
        assert_eq!(label(None, Some("Leapmotor HU")), "Leapmotor HU");
        assert_eq!(label(Some(" "), Some(" Leapmotor HU ")), "Leapmotor HU");
        assert_eq!(label(None, None), DEFAULT_LABEL);
        assert_eq!(label(Some(""), Some("")), DEFAULT_LABEL);
        assert_eq!(CarTile::default().label, "车辆");
        assert_eq!(CarTile::default().icon.source, IconSource::CarBridge);
    }

    #[test]
    fn tiles_that_look_alike() {
        let dir = temp_dir("alike");
        let tile = |label: &str, icon: Icon| CarTile {
            label: label.into(),
            icon,
        };
        let carbridge = tile("零跑C10", Icon::carbridge());
        assert!(carbridge.looks_like(&carbridge.clone()));
        assert!(!carbridge.looks_like(&tile("车辆", Icon::carbridge())));

        let path = dir.join("0028.png");
        std::fs::write(&path, png(192, 192, 2, &[], 0)).unwrap();
        let oem = tile("零跑C10", read_icon(&path).unwrap());
        assert!(!carbridge.looks_like(&oem));
        assert!(!oem.looks_like(&carbridge));
        // The same picture from another file looks the same.
        let copy = dir.join("copy.png");
        std::fs::copy(&path, &copy).unwrap();
        assert!(oem.looks_like(&tile("零跑C10", read_icon(&copy).unwrap())));
        // Another picture in the same file does not.
        std::fs::write(&path, png(192, 192, 2, &[b"PLTE"], 0)).unwrap();
        assert!(!oem.looks_like(&tile("零跑C10", read_icon(&path).unwrap())));
        std::fs::write(&path, png(256, 256, 2, &[], 0)).unwrap();
        assert!(!oem.looks_like(&tile("零跑C10", read_icon(&path).unwrap())));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn carbridge_icon_is_usable() {
        assert_eq!(check_png(CARBRIDGE_ICON), Ok(CARBRIDGE_ICON_SIDE));
    }

    #[test]
    fn png_checks() {
        assert_eq!(check_png(&png(192, 192, 2, &[], 0)), Ok(192));
        assert_eq!(check_png(&png(64, 64, 0, &[], 0)), Ok(64));
        assert_eq!(check_png(&png(512, 512, 3, &[b"PLTE"], 0)), Ok(512));
        assert_eq!(
            check_png(&png(200, 192, 2, &[], 0)),
            Err(IconError::NotSquare {
                width: 200,
                height: 192
            })
        );
        assert_eq!(check_png(&png(63, 63, 2, &[], 0)), Err(IconError::Side(63)));
        assert_eq!(
            check_png(&png(513, 513, 2, &[], 0)),
            Err(IconError::Side(513))
        );
        assert_eq!(check_png(&png(192, 192, 6, &[], 0)), Err(IconError::Alpha));
        assert_eq!(check_png(&png(192, 192, 4, &[], 0)), Err(IconError::Alpha));
        assert_eq!(
            check_png(&png(192, 192, 3, &[b"PLTE", b"tRNS"], 0)),
            Err(IconError::Transparency)
        );
        assert_eq!(
            check_png(&png(192, 192, 2, &[b"tRNS"], 0)),
            Err(IconError::Transparency)
        );
    }

    #[test]
    fn png_size_limit() {
        let at_limit = png(192, 192, 2, &[], MAX_BYTES as usize);
        assert_eq!(at_limit.len() as u64, MAX_BYTES);
        assert_eq!(check_png(&at_limit), Ok(192));
        let over = png(192, 192, 2, &[], MAX_BYTES as usize + 1);
        assert_eq!(check_png(&over), Err(IconError::TooLarge(MAX_BYTES + 1)));
    }

    #[test]
    fn not_png_or_truncated() {
        assert_eq!(check_png(b""), Err(IconError::NotPng));
        assert_eq!(check_png(b"GIF89a"), Err(IconError::NotPng));
        let good = png(192, 192, 2, &[], 0);
        // A first chunk that is not the header.
        let mut swapped = good.clone();
        swapped[12..16].copy_from_slice(b"IDAT");
        assert_eq!(check_png(&swapped), Err(IconError::NotPng));
        assert_eq!(check_png(&good[..20]), Err(IconError::NotPng));
        assert_eq!(
            check_png(&good[..good.len() - 1]),
            Err(IconError::Truncated)
        );
        assert_eq!(
            check_png(&good[..good.len() - 12]),
            Err(IconError::Truncated)
        );
    }

    #[test]
    fn oem_icon_vendors() {
        let dir = Path::new("/etc/carbridge/oem-icons");
        assert_eq!(
            oem_icon_path(Some("0028"), dir),
            Some(PathBuf::from("/etc/carbridge/oem-icons/0028.png"))
        );
        assert_eq!(oem_icon_path(Some("0029"), dir), None);
        assert_eq!(oem_icon_path(Some("../0028"), dir), None);
        assert_eq!(oem_icon_path(None, dir), None);
    }

    #[test]
    fn choice_for_head_units() {
        let dir = temp_dir("choice");
        // Leapmotor without its icon in place.
        let choice = Choice::for_head_unit(Some("C10"), Some("Leapmotor HU"), Some("0028"), &dir);
        assert_eq!(choice.tile.label, "C10");
        assert_eq!(choice.tile.icon, Icon::carbridge());
        let path = dir.join("0028.png");
        assert_eq!(choice.refused, Some((path.clone(), IconError::Missing)));
        assert_eq!(
            choice.to_string(),
            format!(
                "car tile \"C10\" with CarBridge's icon; {} is not in place",
                path.display()
            )
        );

        // Leapmotor with a usable icon.
        let icon = png(192, 192, 2, &[], 0);
        std::fs::write(&path, &icon).unwrap();
        let choice = Choice::for_head_unit(None, Some("Leapmotor HU"), Some("0028"), &dir);
        assert_eq!(choice.tile.label, "Leapmotor HU");
        assert_eq!(choice.tile.icon.side, 192);
        assert_eq!(&*choice.tile.icon.png, &icon[..]);
        assert_eq!(choice.tile.icon.source, IconSource::File(path.clone()));
        assert!(choice.refused.is_none());
        assert_eq!(
            choice.to_string(),
            format!(
                "car tile \"Leapmotor HU\" with {} (192x192)",
                path.display()
            )
        );

        // Leapmotor with an icon that has an alpha channel.
        std::fs::write(&path, png(192, 192, 6, &[], 0)).unwrap();
        let choice = Choice::for_head_unit(None, None, Some("0028"), &dir);
        assert_eq!(choice.tile, CarTile::default());
        assert_eq!(choice.refused, Some((path.clone(), IconError::Alpha)));

        // Too large a file is not read.
        std::fs::write(&path, vec![0; MAX_BYTES as usize + 1]).unwrap();
        let choice = Choice::for_head_unit(None, None, Some("0028"), &dir);
        assert_eq!(
            choice.refused,
            Some((path, IconError::TooLarge(MAX_BYTES + 1)))
        );

        // Another maker gets CarBridge's icon, with nothing to explain.
        let choice = Choice::for_head_unit(Some("Car"), None, Some("0001"), &dir);
        assert_eq!(choice.tile.icon, Icon::carbridge());
        assert!(choice.refused.is_none());
        assert_eq!(choice.to_string(), "car tile \"Car\" with CarBridge's icon");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
