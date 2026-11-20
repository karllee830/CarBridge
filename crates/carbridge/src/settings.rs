// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! A head unit's own settings, which come before the options' defaults for it: its full screen picture size, its
//! frame rate, its H.265 mode, how CarPlay starts on it, the density of its screen, which CarPlay's density in each
//! of its modes is worked out from, and the kind of sound the iPhone's navigation plays as on it. Each is unset unless the owner set it, and then the option's default applies; for
//! the density, the screen's real one as the head unit reports it ([`DensitySetting::Real`]).
//!
//! They are kept as a JSON object with a string for each setting set: `full_screen` (`auto` or `WxH`), `fps`
//! (`auto` or 30 to 60), `hevc` (`off`, `auto` or `force`), `start` (`fullscreen` or `window`), and `dpi`
//! (`configured`, the density the head unit gives the phone to lay out with, or dots per inch, [`MIN_DPI`] to
//! [`MAX_DPI`]), and `nav_audio` (`tts`, `media`, `call` or `assistant`). Keys and values that are not known are ignored when the settings are read.
//!
//! The web interface asks for changes with one JSON object a line ([`Request::parse`]): the head unit's `id` and the
//! settings to change, `default` (or `null`) for a setting to go back to the default. Settings it does not name stay.
//! `views` with `default` clears the views measured of the head unit ([`crate::views`]), for its window modes to be
//! calibrated again.

use std::fmt;

use carbridge_carlink_session::display::Layout;
use carbridge_carplay::picture::{Picture, MAX_DPI, MIN_DPI};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};

use crate::options::{
    hevc_name, layout_name, parse_hevc, parse_layout, parse_rate, parse_size, rate_name, Hevc,
    Options,
};
use crate::picture::{self, DensitySetting, RateSetting, SizeSetting};
use crate::sound::{nav_audio_name, parse_nav_audio, NavAudio};

/// A head unit's own full screen picture size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FullScreen {
    /// The area the head unit shows the cast in.
    Auto,
    /// This width and height in pixels.
    Custom(u32, u32),
}

/// A head unit's own settings; `None` for each it takes from the options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeadUnitSettings {
    pub full_screen: Option<FullScreen>,
    pub fps: Option<RateSetting>,
    pub hevc: Option<Hevc>,
    pub start: Option<Layout>,
    /// The density of the head unit's screen, other than its real one.
    pub dpi: Option<DensitySetting>,
    /// The kind of sound navigation plays as.
    pub nav_audio: Option<NavAudio>,
}

const FULL_SCREEN: &str = "full_screen";
const FPS: &str = "fps";
const HEVC: &str = "hevc";
const START: &str = "start";
const DPI: &str = "dpi";
const NAV_AUDIO: &str = "nav_audio";
const VIEWS: &str = "views";

/// Whether a full screen size is one CarPlay takes as it is: even sides within the bounds of
/// [`Picture::fitted`].
pub fn valid_size(width: u32, height: u32) -> bool {
    let fitted = Picture::new(width, height, picture::DEFAULT_FPS).fitted();
    (fitted.width, fitted.height) == (width, height)
}

fn parse_full_screen(text: &str) -> Option<FullScreen> {
    if text == "auto" {
        return Some(FullScreen::Auto);
    }
    let (width, height) = parse_size(text)?;
    valid_size(width, height).then_some(FullScreen::Custom(width, height))
}

/// The density of a head unit's screen: `configured`, or dots per inch within [`MIN_DPI`] to [`MAX_DPI`].
fn parse_dpi(text: &str) -> Option<DensitySetting> {
    if text == "configured" {
        return Some(DensitySetting::Configured);
    }
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse()
        .ok()
        .filter(|dpi| (MIN_DPI..=MAX_DPI).contains(dpi))
        .map(DensitySetting::Custom)
}

fn dpi_name(dpi: DensitySetting) -> String {
    match dpi {
        DensitySetting::Real => "real".to_owned(),
        DensitySetting::Configured => "configured".to_owned(),
        DensitySetting::Custom(dpi) => dpi.to_string(),
    }
}

fn full_screen_name(full_screen: FullScreen) -> String {
    match full_screen {
        FullScreen::Auto => "auto".to_owned(),
        FullScreen::Custom(width, height) => format!("{width}x{height}"),
    }
}

/// One setting's change: `None` to go back to the default.
type Change<T> = Option<Option<T>>;

/// The value of a setting in a request: `None` for the default, an error for one that is not a value of it.
fn requested<T>(value: &Value, parse: impl Fn(&str) -> Option<T>) -> Result<Option<T>, ()> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) if text == "default" => Ok(None),
        Value::String(text) => parse(text).map(Some).ok_or(()),
        _ => Err(()),
    }
}

/// The value of a setting kept, if it is one.
fn kept<T>(map: &Map<String, Value>, key: &str, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    map.get(key).and_then(Value::as_str).and_then(parse)
}

impl HeadUnitSettings {
    /// The settings as kept, ignoring what is not known; none from text that is no JSON object.
    pub fn from_json(text: &str) -> HeadUnitSettings {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
            return HeadUnitSettings::default();
        };
        HeadUnitSettings {
            full_screen: kept(&map, FULL_SCREEN, parse_full_screen),
            fps: kept(&map, FPS, parse_rate),
            hevc: kept(&map, HEVC, parse_hevc),
            start: kept(&map, START, parse_layout),
            dpi: kept(&map, DPI, parse_dpi),
            nav_audio: kept(&map, NAV_AUDIO, parse_nav_audio),
        }
    }

    pub fn to_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }

    /// Whether every setting is the default.
    pub fn is_default(&self) -> bool {
        *self == HeadUnitSettings::default()
    }
}

impl Serialize for HeadUnitSettings {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(full_screen) = self.full_screen {
            map.serialize_entry(FULL_SCREEN, &full_screen_name(full_screen))?;
        }
        if let Some(fps) = self.fps {
            map.serialize_entry(FPS, &rate_name(fps))?;
        }
        if let Some(hevc) = self.hevc {
            map.serialize_entry(HEVC, hevc_name(hevc))?;
        }
        if let Some(start) = self.start {
            map.serialize_entry(START, layout_name(start))?;
        }
        if let Some(dpi) = self.dpi {
            map.serialize_entry(DPI, &dpi_name(dpi))?;
        }
        if let Some(nav) = self.nav_audio {
            map.serialize_entry(NAV_AUDIO, nav_audio_name(nav))?;
        }
        map.end()
    }
}

impl fmt::Display for HeadUnitSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_default() {
            return f.write_str("none of its own");
        }
        let mut parts = Vec::new();
        if let Some(full_screen) = self.full_screen {
            parts.push(format!("full screen {}", full_screen_name(full_screen)));
        }
        if let Some(fps) = self.fps {
            parts.push(format!("frame rate {}", rate_name(fps)));
        }
        if let Some(hevc) = self.hevc {
            parts.push(format!("H.265 {}", hevc_name(hevc)));
        }
        if let Some(start) = self.start {
            parts.push(format!("start {}", layout_name(start)));
        }
        match self.dpi {
            Some(DensitySetting::Custom(dpi)) => parts.push(format!("screen {dpi} dpi")),
            Some(DensitySetting::Configured) => parts.push("screen dpi configured".to_owned()),
            Some(DensitySetting::Real) | None => {}
        }
        if let Some(nav) = self.nav_audio {
            parts.push(format!("navigation as {}", nav_audio_name(nav)));
        }
        f.write_str(&parts.join(", "))
    }
}

/// A change to a head unit's settings, from the web interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The head unit, as the list of head units names it.
    pub id: String,
    full_screen: Change<FullScreen>,
    fps: Change<RateSetting>,
    hevc: Change<Hevc>,
    start: Change<Layout>,
    dpi: Change<DensitySetting>,
    nav_audio: Change<NavAudio>,
    /// The views measured of the head unit are cleared.
    clear_views: bool,
}

impl Request {
    /// A request line: a JSON object with the head unit's `id` and the settings to change, and `views` to clear its
    /// views. A line that is not one, names no head unit, names a setting that is not known or gives a setting a value
    /// it does not take is refused, with the reason.
    pub fn parse(line: &str) -> Result<Request, String> {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(line) else {
            return Err("not a JSON object".to_owned());
        };
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or("no head unit id")?
            .to_owned();
        let mut request = Request {
            id,
            full_screen: None,
            fps: None,
            hevc: None,
            start: None,
            dpi: None,
            nav_audio: None,
            clear_views: false,
        };
        for (key, value) in &map {
            let bad = || format!("bad {key} {value}");
            match key.as_str() {
                "id" => {}
                FULL_SCREEN => {
                    request.full_screen =
                        Some(requested(value, parse_full_screen).map_err(|_| bad())?)
                }
                FPS => request.fps = Some(requested(value, parse_rate).map_err(|_| bad())?),
                HEVC => request.hevc = Some(requested(value, parse_hevc).map_err(|_| bad())?),
                START => request.start = Some(requested(value, parse_layout).map_err(|_| bad())?),
                DPI => request.dpi = Some(requested(value, parse_dpi).map_err(|_| bad())?),
                NAV_AUDIO => {
                    request.nav_audio = Some(requested(value, parse_nav_audio).map_err(|_| bad())?)
                }
                VIEWS => {
                    requested(value, |_| None::<()>).map_err(|_| bad())?;
                    request.clear_views = true;
                }
                _ => return Err(format!("unknown setting {key}")),
            }
        }
        Ok(request)
    }

    /// The settings with this change made.
    pub fn applied_to(&self, mut settings: HeadUnitSettings) -> HeadUnitSettings {
        if let Some(full_screen) = self.full_screen {
            settings.full_screen = full_screen;
        }
        if let Some(fps) = self.fps {
            settings.fps = fps;
        }
        if let Some(hevc) = self.hevc {
            settings.hevc = hevc;
        }
        if let Some(start) = self.start {
            settings.start = start;
        }
        if let Some(dpi) = self.dpi {
            settings.dpi = dpi;
        }
        if let Some(nav) = self.nav_audio {
            settings.nav_audio = nav;
        }
        settings
    }

    /// Whether the request clears the views measured of the head unit.
    pub fn clears_views(&self) -> bool {
        self.clear_views
    }
}

/// What a connection to a head unit goes by: its own settings where it has them, else the options'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effective {
    pub picture: picture::Settings,
    pub hevc: Hevc,
    pub start: Layout,
    pub nav_audio: NavAudio,
}

impl Effective {
    pub fn new(options: &Options, own: &HeadUnitSettings) -> Effective {
        let mut picture = options.picture();
        match own.full_screen {
            Some(FullScreen::Auto) => picture.size = SizeSetting::Auto,
            Some(FullScreen::Custom(width, height)) => picture.custom_size = Some((width, height)),
            None => {}
        }
        if let Some(fps) = own.fps {
            picture.rate = fps;
        }
        picture.density = own.dpi.unwrap_or_default();
        Effective {
            picture,
            hevc: own.hevc.unwrap_or(options.hevc),
            start: own.start.unwrap_or(options.carplay_start),
            nav_audio: own.nav_audio.unwrap_or(options.nav_audio),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tablet() -> HeadUnitSettings {
        HeadUnitSettings {
            full_screen: Some(FullScreen::Custom(2432, 1484)),
            fps: Some(RateSetting::Auto),
            hevc: Some(Hevc::Off),
            start: Some(Layout::Window),
            ..HeadUnitSettings::default()
        }
    }

    fn c10() -> HeadUnitSettings {
        HeadUnitSettings {
            full_screen: Some(FullScreen::Custom(2432, 1164)),
            fps: Some(RateSetting::Fixed(60)),
            dpi: Some(DensitySetting::Custom(201)),
            ..HeadUnitSettings::default()
        }
    }

    #[test]
    fn kept_as_json() {
        let text = tablet().to_json();
        assert_eq!(
            text,
            r#"{"full_screen":"2432x1484","fps":"auto","hevc":"off","start":"window"}"#
        );
        assert_eq!(HeadUnitSettings::from_json(&text), tablet());
        assert_eq!(HeadUnitSettings::default().to_json(), "{}");
        assert!(HeadUnitSettings::from_json("{}").is_default());
    }

    #[test]
    fn read_tolerantly() {
        // Settings not known, and values a setting does not take, are passed over.
        let settings = HeadUnitSettings::from_json(
            r#"{"full_screen":"2433x1484","fps":60,"hevc":"auto","start":"sideways","later":{"x":1}}"#,
        );
        assert_eq!(
            settings,
            HeadUnitSettings {
                hevc: Some(Hevc::Auto),
                ..HeadUnitSettings::default()
            }
        );
        for text in ["", "not json", "[1]", "\"auto\""] {
            assert!(HeadUnitSettings::from_json(text).is_default(), "{text}");
        }
    }

    #[test]
    fn requests() {
        let request = Request::parse(
            r#"{"id":"a1b2c3d4e5f6","full_screen":"2432x1484","fps":"60","hevc":"default","start":null}"#,
        )
        .unwrap();
        assert_eq!(request.id, "a1b2c3d4e5f6");
        let before = HeadUnitSettings {
            hevc: Some(Hevc::Force),
            start: Some(Layout::Window),
            ..HeadUnitSettings::default()
        };
        assert_eq!(
            request.applied_to(before),
            HeadUnitSettings {
                full_screen: Some(FullScreen::Custom(2432, 1484)),
                fps: Some(RateSetting::Fixed(60)),
                ..HeadUnitSettings::default()
            }
        );
        // Settings a request does not name stay.
        let request = Request::parse(r#"{"id":"a1b2c3d4e5f6","full_screen":"auto"}"#).unwrap();
        assert_eq!(
            request.applied_to(tablet()),
            HeadUnitSettings {
                full_screen: Some(FullScreen::Auto),
                ..tablet()
            }
        );
        let request = Request::parse(r#"{"id":" a1b2c3d4e5f6 "}"#).unwrap();
        assert_eq!(request.id, "a1b2c3d4e5f6");
        assert_eq!(request.applied_to(tablet()), tablet());
    }

    #[test]
    fn navigation_sound() {
        let request = Request::parse(r#"{"id":"a1b2c3d4e5f6","nav_audio":"media"}"#).unwrap();
        let own = request.applied_to(tablet());
        assert_eq!(own.nav_audio, Some(NavAudio::Media));
        assert!(own.to_json().ends_with(r#","nav_audio":"media"}"#));
        assert_eq!(HeadUnitSettings::from_json(&own.to_json()), own);
        assert!(own.to_string().ends_with(", navigation as media"));
        let options = Options::default();
        assert_eq!(Effective::new(&options, &own).nav_audio, NavAudio::Media);
        assert_eq!(
            Effective::new(&options, &HeadUnitSettings::default()).nav_audio,
            NavAudio::Tts
        );
        let options = Options {
            nav_audio: NavAudio::Assistant,
            ..Options::default()
        };
        assert_eq!(
            Effective::new(&options, &HeadUnitSettings::default()).nav_audio,
            NavAudio::Assistant
        );
        let back = Request::parse(r#"{"id":"a1b2c3d4e5f6","nav_audio":"default"}"#).unwrap();
        assert_eq!(back.applied_to(own).nav_audio, None);
        assert!(Request::parse(r#"{"id":"a1b2c3d4e5f6","nav_audio":"music"}"#).is_err());
    }

    #[test]
    fn bad_requests() {
        for line in [
            "",
            "a1b2c3d4e5f6",
            "[]",
            r#"{"full_screen":"auto"}"#,
            r#"{"id":"","full_screen":"auto"}"#,
            r#"{"id":7,"full_screen":"auto"}"#,
            r#"{"id":"a1b2c3d4e5f6","full_screen":"2433x1484"}"#,
            r#"{"id":"a1b2c3d4e5f6","full_screen":"8000x8000"}"#,
            r#"{"id":"a1b2c3d4e5f6","full_screen":"100x100"}"#,
            r#"{"id":"a1b2c3d4e5f6","full_screen":"2432"}"#,
            r#"{"id":"a1b2c3d4e5f6","fps":"25"}"#,
            r#"{"id":"a1b2c3d4e5f6","fps":60}"#,
            r#"{"id":"a1b2c3d4e5f6","hevc":"on"}"#,
            r#"{"id":"a1b2c3d4e5f6","start":"portrait"}"#,
            r#"{"id":"a1b2c3d4e5f6","colour":"red"}"#,
            r#"{"id":"5940a63aab3c","dpi":"71"}"#,
            r#"{"id":"5940a63aab3c","dpi":"641"}"#,
            r#"{"id":"5940a63aab3c","dpi":"0"}"#,
            r#"{"id":"5940a63aab3c","dpi":"auto"}"#,
            r#"{"id":"5940a63aab3c","dpi":"real"}"#,
            r#"{"id":"5940a63aab3c","dpi":"+352"}"#,
            r#"{"id":"5940a63aab3c","dpi":"352.5"}"#,
            r#"{"id":"5940a63aab3c","dpi":" 352"}"#,
            r#"{"id":"5940a63aab3c","dpi":""}"#,
            r#"{"id":"5940a63aab3c","dpi":352}"#,
            r#"{"id":"5940a63aab3c","full_screen_dpi":"201"}"#,
            r#"{"id":"5940a63aab3c","views":"1712x819"}"#,
            r#"{"id":"5940a63aab3c","views":{}}"#,
            r#"{"id":"5940a63aab3c","views":true}"#,
        ] {
            assert!(Request::parse(line).is_err(), "{line}");
        }
    }

    #[test]
    fn densities_kept_and_requested() {
        let text = c10().to_json();
        assert_eq!(
            text,
            r#"{"full_screen":"2432x1164","fps":"60","dpi":"201"}"#
        );
        assert_eq!(HeadUnitSettings::from_json(&text), c10());
        let configured = HeadUnitSettings {
            dpi: Some(DensitySetting::Configured),
            ..HeadUnitSettings::default()
        };
        assert_eq!(configured.to_json(), r#"{"dpi":"configured"}"#);
        assert_eq!(
            HeadUnitSettings::from_json(r#"{"dpi":"configured"}"#),
            configured
        );
        // Densities out of bounds, and the densities of each mode kept before, are passed over when read.
        for text in [r#"{"dpi":"10"}"#, r#"{"dpi":"641"}"#, r#"{"dpi":"real"}"#] {
            assert!(HeadUnitSettings::from_json(text).is_default(), "{text}");
        }
        let old = HeadUnitSettings::from_json(
            r#"{"full_screen":"2432x1164","fps":"60","full_screen_dpi":"201","landscape_window_dpi":"201","portrait_window_dpi":"312"}"#,
        );
        assert_eq!(old, HeadUnitSettings { dpi: None, ..c10() });
        assert_eq!(old.to_json(), r#"{"full_screen":"2432x1164","fps":"60"}"#);
        let request = Request::parse(
            r#"{"id":"5940a63aab3c","full_screen":"2432x1164","fps":"60","dpi":"201"}"#,
        )
        .unwrap();
        assert!(!request.clears_views());
        assert_eq!(request.applied_to(HeadUnitSettings::default()), c10());
        let request = Request::parse(r#"{"id":"5940a63aab3c","dpi":"configured"}"#).unwrap();
        assert_eq!(
            request.applied_to(c10()).dpi,
            Some(DensitySetting::Configured)
        );
        // Back to the screen's real density.
        let request = Request::parse(r#"{"id":"5940a63aab3c","dpi":"default"}"#).unwrap();
        assert_eq!(
            request.applied_to(c10()),
            HeadUnitSettings { dpi: None, ..c10() }
        );
        assert_eq!(
            c10().to_string(),
            "full screen 2432x1164, frame rate 60, screen 201 dpi"
        );
        assert_eq!(configured.to_string(), "screen dpi configured");
    }

    #[test]
    fn requests_to_calibrate_again() {
        for line in [
            r#"{"id":"5940a63aab3c","views":"default"}"#,
            r#"{"id":"5940a63aab3c","views":null}"#,
        ] {
            let request = Request::parse(line).unwrap();
            assert!(request.clears_views(), "{line}");
            // The settings stay.
            assert_eq!(request.applied_to(c10()), c10());
        }
        let both = Request::parse(r#"{"id":"5940a63aab3c","views":"default","fps":"30"}"#).unwrap();
        assert!(both.clears_views());
        assert_eq!(both.applied_to(c10()).fps, Some(RateSetting::Fixed(30)));
    }

    #[test]
    fn densities_go_to_the_picture() {
        let own = Effective::new(&Options::default(), &c10());
        assert_eq!(own.picture.density, DensitySetting::Custom(201));
        let none = Effective::new(&Options::default(), &tablet());
        assert_eq!(none.picture.density, DensitySetting::Real);
        // Another density is another picture, which the iPhone connects again for.
        let lower = HeadUnitSettings {
            dpi: Some(DensitySetting::Custom(160)),
            ..c10()
        };
        assert_ne!(Effective::new(&Options::default(), &lower), own);
    }

    #[test]
    fn sizes_carplay_takes() {
        assert!(valid_size(2432, 1484));
        assert!(valid_size(1484, 2432));
        assert!(valid_size(320, 240));
        assert!(valid_size(3840, 2160));
        assert!(!valid_size(2433, 1484));
        assert!(!valid_size(3842, 2160));
        assert!(!valid_size(318, 240));
        assert!(!valid_size(640, 238));
    }

    #[test]
    fn own_settings_before_the_options() {
        let options = Options {
            carplay_picture: SizeSetting::Fixed,
            carplay_fps: RateSetting::Fixed(60),
            hevc: Hevc::Force,
            ..Options::default()
        };
        let defaults = Effective::new(&options, &HeadUnitSettings::default());
        assert_eq!(defaults.picture, options.picture());
        assert_eq!(defaults.hevc, Hevc::Force);
        assert_eq!(defaults.start, Layout::FullScreen);

        let own = Effective::new(&options, &tablet());
        assert_eq!(own.picture.custom_size, Some((2432, 1484)));
        assert_eq!(own.picture.rate, RateSetting::Auto);
        assert_eq!(own.hevc, Hevc::Off);
        assert_eq!(own.start, Layout::Window);

        // The head unit's own automatic size comes before a fixed one.
        let auto = HeadUnitSettings {
            full_screen: Some(FullScreen::Auto),
            ..HeadUnitSettings::default()
        };
        let effective = Effective::new(&options, &auto);
        assert_eq!(effective.picture.size, SizeSetting::Auto);
        assert_eq!(effective.picture.custom_size, None);
        assert_eq!(effective.picture.rate, RateSetting::Fixed(60));
    }

    #[test]
    fn described_for_the_log() {
        assert_eq!(HeadUnitSettings::default().to_string(), "none of its own");
        assert_eq!(
            tablet().to_string(),
            "full screen 2432x1484, frame rate auto, H.265 off, start window"
        );
    }
}
