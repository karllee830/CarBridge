// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The head unit's view of CarPlay in each of its modes: the part of its screen it shows CarPlay's picture on, in its
//! screen's pixels, as each of its touches reports it, and the density CarPlay is laid out at from it.
//!
//! A head unit sizes the view itself and names its size nowhere else: the small window size of its screen info is a
//! fixed value, not the window's. A view is kept with the picture it was measured for, as the head unit shows it in
//! that mode (turned upright in the portrait window). The head unit fits the picture into the area of its mode keeping
//! the picture's shape, so a picture of the same shape (sides in the same ratio, within [`ASPECT_TOLERANCE`]) gets
//! the same view, whatever its size; for a picture of another shape the view is not known until it is measured again.
//!
//! The density CarPlay is laid out at ([`calibrated_dpi`]) makes CarPlay's controls as large on the view as they are
//! on a screen of the head unit's density: the picture's `width` pixels show on the view's `view width` pixels of a
//! screen of `density` dots per inch, so the picture's physical width is `view width / density` inches, and its
//! density `density × width / view width`; by its height alike. The head unit rounds each side of the view on its
//! own, so the density takes both sides: `density × √(width × height / (view width × view height))`.
//!
//! The views of a head unit are kept as a JSON object with an entry for each mode measured, `full_screen`,
//! `landscape_window` and `portrait_window`, each `{"view":"WxH","picture":"WxH"}`. Entries that cannot be read are
//! ignored.

use std::fmt;

use carbridge_carlink_session::display::PictureSize;
use carbridge_carplay::picture::{MAX_DPI, MIN_DPI};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};

use crate::options::parse_size;
use crate::picture::Mode;

/// How far the ratio of a picture's sides may be from that of the picture a view was measured for, as a fraction,
/// for the view to be taken as the picture's.
pub const ASPECT_TOLERANCE: f64 = 0.01;

/// A view of the head unit's and the picture it was measured for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measured {
    pub view: PictureSize,
    pub picture: PictureSize,
}

impl Measured {
    /// The view of a picture of `picture`, as the head unit shows it in the mode: the one measured, when the picture
    /// has the shape of the one it was measured for; `None` when it has another.
    pub fn view_for(&self, picture: PictureSize) -> Option<PictureSize> {
        let ratio = |size: PictureSize| f64::from(size.width) / f64::from(size.height.max(1));
        let (now, then) = (ratio(picture), ratio(self.picture));
        (picture.width > 0 && picture.height > 0 && (now / then - 1.0).abs() <= ASPECT_TOLERANCE)
            .then_some(self.view)
    }
}

impl fmt::Display for Measured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} for the picture {}", self.view, self.picture)
    }
}

/// The density CarPlay is laid out at for a picture of `picture` pixels the head unit shows on `view` pixels of a
/// screen of `density` dots per inch, within [`MIN_DPI`] to [`MAX_DPI`].
pub fn calibrated_dpi(density: u32, picture: PictureSize, view: PictureSize) -> u32 {
    let area = |size: PictureSize| f64::from(size.width.max(1)) * f64::from(size.height.max(1));
    let scale = (area(picture) / area(view)).sqrt();
    ((f64::from(density) * scale).round() as u32).clamp(MIN_DPI, MAX_DPI)
}

/// The views measured in each mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Views {
    pub full_screen: Option<Measured>,
    pub landscape_window: Option<Measured>,
    pub portrait_window: Option<Measured>,
}

const MODES: [(Mode, &str); 3] = [
    (Mode::FullScreen, "full_screen"),
    (Mode::LandscapeWindow, "landscape_window"),
    (Mode::PortraitWindow, "portrait_window"),
];

impl Views {
    /// The view measured in `mode`, if one is.
    pub fn of(&self, mode: Mode) -> Option<Measured> {
        match mode {
            Mode::FullScreen => self.full_screen,
            Mode::LandscapeWindow => self.landscape_window,
            Mode::PortraitWindow => self.portrait_window,
        }
    }

    /// The view in `mode` of a picture of `picture`, as the head unit shows it there, if it is known
    /// ([`Measured::view_for`]).
    pub fn view_for(&self, mode: Mode, picture: PictureSize) -> Option<PictureSize> {
        self.of(mode)?.view_for(picture)
    }

    pub fn set(&mut self, mode: Mode, measured: Option<Measured>) {
        match mode {
            Mode::FullScreen => self.full_screen = measured,
            Mode::LandscapeWindow => self.landscape_window = measured,
            Mode::PortraitWindow => self.portrait_window = measured,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Views::default()
    }

    /// The views as kept, ignoring what cannot be read; none from text that is no JSON object.
    pub fn from_json(text: &str) -> Views {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
            return Views::default();
        };
        let mut views = Views::default();
        for (mode, key) in MODES {
            views.set(mode, read(&map, key));
        }
        views
    }

    pub fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }
}

fn read(map: &Map<String, Value>, key: &str) -> Option<Measured> {
    let entry = map.get(key)?.as_object()?;
    let size = |name: &str| {
        let (width, height) = parse_size(entry.get(name)?.as_str()?)?;
        Some(PictureSize { width, height })
    };
    Some(Measured {
        view: size("view")?,
        picture: size("picture")?,
    })
}

impl Serialize for Views {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (mode, key) in MODES {
            if let Some(measured) = self.of(mode) {
                let entry = serde_json::json!({
                    "view": measured.view.to_string(),
                    "picture": measured.picture.to_string(),
                });
                map.serialize_entry(key, &entry)?;
            }
        }
        map.end()
    }
}

impl fmt::Display for Views {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none measured");
        }
        let parts: Vec<String> = MODES
            .iter()
            .filter_map(|(mode, _)| Some(format!("{mode} {}", self.of(*mode)?)))
            .collect();
        f.write_str(&parts.join(", "))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> PictureSize {
        PictureSize { width, height }
    }

    /// The C10's views, measured for its 2432x1164 picture.
    pub(crate) fn c10() -> Views {
        Views {
            full_screen: Some(Measured {
                view: size(2432, 1164),
                picture: size(2432, 1164),
            }),
            landscape_window: Some(Measured {
                view: size(1712, 819),
                picture: size(2432, 1164),
            }),
            portrait_window: Some(Measured {
                view: size(528, 1104),
                picture: size(1164, 2432),
            }),
        }
    }

    #[test]
    fn density_from_the_view() {
        // The same size: the head unit's density as it is.
        assert_eq!(calibrated_dpi(201, size(2432, 1164), size(2432, 1164)), 201);
        // The C10's windows.
        assert_eq!(calibrated_dpi(201, size(2432, 1164), size(1712, 819)), 286);
        assert_eq!(calibrated_dpi(201, size(1164, 2432), size(528, 1104)), 443);
        // The tablet's.
        assert_eq!(calibrated_dpi(206, size(2458, 1484), size(2432, 1468)), 208);
        assert_eq!(calibrated_dpi(206, size(2458, 1484), size(1712, 1034)), 296);
        assert_eq!(calibrated_dpi(206, size(1484, 2458), size(852, 1412)), 359);
        // Within the bounds CarPlay takes.
        assert_eq!(
            calibrated_dpi(320, size(2432, 1164), size(400, 190)),
            MAX_DPI
        );
        assert_eq!(calibrated_dpi(72, size(800, 480), size(1600, 960)), MIN_DPI);
        assert_eq!(calibrated_dpi(201, size(2432, 1164), size(0, 0)), MAX_DPI);
    }

    #[test]
    fn views_follow_the_pictures_shape() {
        let window = c10().landscape_window.unwrap();
        assert_eq!(window.view_for(size(2432, 1164)), Some(size(1712, 819)));
        // The picture the C10 asks for itself has the same shape: the same view.
        assert_eq!(window.view_for(size(2458, 1176)), Some(size(1712, 819)));
        // Another shape needs measuring again.
        assert_eq!(window.view_for(size(1920, 1080)), None);
        assert_eq!(window.view_for(size(1164, 2432)), None);
        assert_eq!(window.view_for(size(0, 0)), None);
        // The portrait window's view is of the upright picture.
        let views = c10();
        assert_eq!(
            views.view_for(Mode::PortraitWindow, size(1176, 2458)),
            Some(size(528, 1104))
        );
        assert_eq!(views.view_for(Mode::PortraitWindow, size(2432, 1164)), None);
        assert_eq!(
            Views::default().view_for(Mode::FullScreen, size(2432, 1164)),
            None
        );
    }

    #[test]
    fn kept_as_json() {
        let text = c10().to_json();
        assert_eq!(
            text,
            r#"{"full_screen":{"picture":"2432x1164","view":"2432x1164"},"landscape_window":{"picture":"2432x1164","view":"1712x819"},"portrait_window":{"picture":"1164x2432","view":"528x1104"}}"#
        );
        assert_eq!(Views::from_json(&text), c10());
        assert_eq!(Views::default().to_json(), "{}");
        // Entries that cannot be read are passed over.
        let read = Views::from_json(
            r#"{"full_screen":{"view":"2432x1164"},"landscape_window":{"view":"1712x819","picture":"2432x1164"},"portrait_window":"528x1104","later":1}"#,
        );
        assert_eq!(
            read,
            Views {
                landscape_window: c10().landscape_window,
                ..Views::default()
            }
        );
        for text in [
            "",
            "not json",
            "[1]",
            r#"{"full_screen":{"view":"0x10","picture":"1x1"}}"#,
        ] {
            assert!(Views::from_json(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn described_for_the_log() {
        assert_eq!(Views::default().to_string(), "none measured");
        let views = Views {
            full_screen: None,
            ..c10()
        };
        assert_eq!(
            views.to_string(),
            "landscape window 1712x819 for the picture 2432x1164, portrait window 528x1104 for the picture 1164x2432"
        );
    }
}
