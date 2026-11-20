// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The picture CarPlay renders for the head unit: its size, frame rate and density, from the settings and from what
//! the head unit asks for.
//!
//! With the automatic size, CarPlay is drawn for the area the head unit shows the cast in, which its configuration
//! names (`videoDisplayWidth` × `videoDisplayHeight`, also the first two numbers of its authentication reply's screen
//! info), rather than its whole screen; the head unit shows a landscape cast over that area in full screen and window
//! cast modes alike. The small window size the screen info may add is not used. The head unit shows full screen casts
//! on a view of its own, a little smaller than that area, which it sizes by the area's shape whatever the picture:
//! once its touches have measured that view ([`crate::views`]) for a picture of the area's shape, CarPlay is drawn at
//! the view's size, pixel for pixel on the screen. A head unit can have a full screen
//! size of its own, which comes before the settings' size. With the automatic frame rate, CarPlay runs at the rate the
//! head unit asks for, within 30 to 60; at 30 when it asks for 30 or less or names none. Until a head unit is known,
//! the fixed size and 30 frames a second apply.
//!
//! The picture's density gives its physical size, which CarPlay lays its controls out by. It starts from the density
//! of the head unit's screen ([`DensitySetting`]): by default its real density as the head unit reports it, when that
//! is within [`MIN_DPI`] to [`MAX_DPI`], else the density the head unit gives the phone to lay out with; or that one
//! first; or a density set for the head unit. In each mode, full screen or the landscape or portrait window, the head
//! unit shows the picture on a view of its own size; once that view is measured ([`crate::views`]), the picture's
//! density is the screen's scaled by the picture's size over the view's ([`calibrated_dpi`]), so that CarPlay's
//! controls keep their size on the screen in every mode. Until it is, the screen's density applies as it is.

use std::fmt;

use carbridge_carlink_proto::auth::ScreenInfo;
use carbridge_carlink_proto::pb::GetUCarConfigResponse;
use carbridge_carlink_session::display::{PictureSize, Shape};
use carbridge_carlink_wfd::source::VideoMode;
use carbridge_carplay::picture::{Picture, MAX_DPI, MIN_DPI};
use serde::{Deserialize, Serialize};

use crate::views::{calibrated_dpi, Views};

/// The frame rate when the head unit names none.
pub const DEFAULT_FPS: u32 = 30;

/// How CarPlay's picture size is chosen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SizeSetting {
    /// The area the head unit shows the cast in.
    #[default]
    Auto,
    /// The fixed size of the settings.
    Fixed,
}

/// How CarPlay's frame rate is chosen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RateSetting {
    /// The rate the head unit asks for.
    #[default]
    Auto,
    /// This many frames a second.
    Fixed(u32),
}

/// How the head unit shows the cast, as far as the picture's density goes: full screen, or in its landscape
/// (mirror) or portrait (simple launcher) window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    FullScreen,
    LandscapeWindow,
    PortraitWindow,
}

impl Mode {
    /// The mode for `window`, the shape of the head unit's window while it shows the cast there, or `None` in full
    /// screen.
    pub fn of(window: Option<Shape>) -> Mode {
        match window {
            None => Mode::FullScreen,
            Some(Shape::Landscape) => Mode::LandscapeWindow,
            Some(Shape::Portrait) => Mode::PortraitWindow,
        }
    }

    /// A picture of `size` as the head unit shows it in the mode: as it is full screen, turned to the window's
    /// shape in a window.
    pub fn shown(self, size: PictureSize) -> PictureSize {
        match self {
            Mode::FullScreen => size,
            Mode::LandscapeWindow => size.turned(Shape::Landscape),
            Mode::PortraitWindow => size.turned(Shape::Portrait),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::FullScreen => "full screen",
            Mode::LandscapeWindow => "landscape window",
            Mode::PortraitWindow => "portrait window",
        })
    }
}

/// The density of the head unit's screen the picture's density is worked out from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DensitySetting {
    /// The screen's real density as the head unit reports it, else the one it gives the phone to lay out with
    /// ([`HeadUnitPicture::density`]).
    #[default]
    Real,
    /// The density the head unit gives the phone to lay out with, else its screen's real one
    /// ([`HeadUnitPicture::configured_density`]).
    Configured,
    /// This many dots per inch.
    Custom(u32),
}

/// The picture settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub size: SizeSetting,
    /// The fixed size, also used with the automatic size until a head unit is known.
    pub fixed_size: (u32, u32),
    pub rate: RateSetting,
    /// The full screen size set for the head unit itself, which comes before `size`.
    pub custom_size: Option<(u32, u32)>,
    /// The density of the head unit's screen.
    pub density: DensitySetting,
}

/// What a head unit asks of the picture: the size of the area it shows the cast in, the density it gives the phone to
/// lay out with, its screen's real density and its frame rate, and the size of its whole screen, 0 for what it does
/// not name. Kept for each head unit in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadUnitPicture {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub dpi: u32,
    #[serde(default)]
    pub real_dpi: u32,
    #[serde(default)]
    pub fps: u32,
    #[serde(default)]
    pub screen_width: u32,
    #[serde(default)]
    pub screen_height: u32,
}

/// Which of a head unit's densities the picture has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadUnitDensity {
    /// Its screen's real density.
    Real,
    /// The density it gives the phone to lay out with.
    Layout,
}

impl HeadUnitPicture {
    /// The head unit's density for the picture: its screen's real one when that is within [`MIN_DPI`] to
    /// [`MAX_DPI`], else the one it gives the phone to lay out with; none when it names neither.
    pub fn density(&self) -> Option<(u32, HeadUnitDensity)> {
        self.real().or(self.layout())
    }

    /// The density the head unit gives the phone to lay out with, else its screen's real one when that is within
    /// [`MIN_DPI`] to [`MAX_DPI`]; none when it names neither.
    pub fn configured_density(&self) -> Option<(u32, HeadUnitDensity)> {
        self.layout().or(self.real())
    }

    fn real(&self) -> Option<(u32, HeadUnitDensity)> {
        (MIN_DPI..=MAX_DPI)
            .contains(&self.real_dpi)
            .then_some((self.real_dpi, HeadUnitDensity::Real))
    }

    fn layout(&self) -> Option<(u32, HeadUnitDensity)> {
        (self.dpi > 0).then_some((self.dpi, HeadUnitDensity::Layout))
    }

    /// From the head unit's configuration: the size its cast is shown at, or its whole screen when it names none;
    /// the density it gives the phone to lay out with and its screen's real one; its frame rate.
    pub fn from_config(config: &GetUCarConfigResponse) -> Option<HeadUnitPicture> {
        let (width, height) = match (
            positive(config.video_display_width),
            positive(config.video_display_height),
        ) {
            (Some(width), Some(height)) => (width, height),
            _ => (
                positive(config.screen_width)?,
                positive(config.screen_height)?,
            ),
        };
        Some(HeadUnitPicture {
            width,
            height,
            dpi: positive(config.dpi).unwrap_or(0),
            real_dpi: config.real_dpi.and_then(positive).unwrap_or(0),
            fps: positive(config.fps).unwrap_or(0),
            screen_width: positive(config.screen_width).unwrap_or(0),
            screen_height: positive(config.screen_height).unwrap_or(0),
        })
    }

    /// From the screen info of the head unit's authentication reply, whose density is taken as the one to lay out
    /// with, and which names no real density or frame rate.
    pub fn from_screen_info(info: &ScreenInfo) -> Option<HeadUnitPicture> {
        (info.width > 0 && info.height > 0).then_some(HeadUnitPicture {
            width: info.width,
            height: info.height,
            dpi: info.dpi,
            real_dpi: 0,
            fps: 0,
            screen_width: 0,
            screen_height: 0,
        })
    }
}

fn positive(value: i32) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value > 0)
}

/// Where a head unit's picture comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The configuration of the head unit connected now.
    Config,
    /// The screen info in the authentication reply of the head unit connected now.
    ScreenInfo,
    /// Kept from the head unit's last connection.
    Kept,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Config => "the head unit's configuration",
            Source::ScreenInfo => "the head unit's authentication reply",
            Source::Kept => "the head unit's last connection",
        })
    }
}

/// The picture CarPlay renders, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// Fitted to what AirPlay takes.
    pub picture: Picture,
    pub reason: String,
}

impl fmt::Display for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.picture, self.reason)
    }
}

/// The picture for `settings` and the head unit's picture, if one is known, in `mode`: the head unit's own full
/// screen size if it has one, else the size the settings choose. With a size of the head unit's, its own or its
/// custom one, or with a density set for it, the picture has a density: the screen's density the settings choose,
/// calibrated by the view measured in `mode` for the picture as the head unit shows it there, if `views` has one.
pub fn choose(
    settings: &Settings,
    head_unit: Option<(&HeadUnitPicture, Source)>,
    views: &Views,
    mode: Mode,
) -> Choice {
    let (width, height) = settings.fixed_size;
    let ((width, height, unit), size) = match (settings.custom_size, settings.size, head_unit) {
        (Some((width, height)), _, _) => (
            (width, height, head_unit),
            "custom size for this head unit".to_owned(),
        ),
        (None, SizeSetting::Auto, Some((unit, source))) => {
            let area = PictureSize {
                width: unit.width,
                height: unit.height,
            };
            match views.view_for(Mode::FullScreen, area) {
                Some(view) => (
                    (view.width, view.height, head_unit),
                    format!(
                        "size of the full screen view measured for the area {}x{} from {source}",
                        unit.width, unit.height
                    ),
                ),
                None => (
                    (unit.width, unit.height, head_unit),
                    format!("size from {source}"),
                ),
            }
        }
        (None, SizeSetting::Auto, None) => (
            (width, height, None),
            "fixed size until a head unit is known".to_owned(),
        ),
        (None, SizeSetting::Fixed, _) => ((width, height, None), "fixed size".to_owned()),
    };
    // The screen's density: what kind it is, the number and where it comes from.
    let density = match settings.density {
        DensitySetting::Custom(dpi) => Some(("custom dpi", dpi, " for this head unit".to_owned())),
        setting => unit.and_then(|(unit, source)| {
            let found = match setting {
                DensitySetting::Configured => unit.configured_density(),
                _ => unit.density(),
            };
            found.map(|(dpi, which)| {
                let kind = match which {
                    HeadUnitDensity::Real => "real dpi",
                    HeadUnitDensity::Layout => "dpi",
                };
                (kind, dpi, format!(" from {source}"))
            })
        }),
    };
    let (fps, rate) = match (settings.rate, head_unit) {
        (RateSetting::Fixed(fps), _) => (fps, "fixed frame rate".to_owned()),
        (RateSetting::Auto, Some((unit, source))) if unit.fps > 0 => {
            (unit.fps, format!("frame rate from {source}"))
        }
        (RateSetting::Auto, Some((_, source))) => (
            DEFAULT_FPS,
            format!("{DEFAULT_FPS} fps as {source} names no frame rate"),
        ),
        (RateSetting::Auto, None) => (
            DEFAULT_FPS,
            format!("{DEFAULT_FPS} fps until a head unit is known"),
        ),
    };
    let mut picture = Picture::new(width, height, fps).fitted();
    let shown = mode.shown(PictureSize {
        width: picture.width,
        height: picture.height,
    });
    let density = density.map(|(kind, dpi, from)| match views.view_for(mode, shown) {
        Some(view) => (
            calibrated_dpi(dpi, shown, view),
            format!("dpi calibrated from the {mode}'s view {view} at the {kind} {dpi}{from}"),
        ),
        None => (dpi, format!("{kind}{from}")),
    });
    picture.dpi = density
        .as_ref()
        .map(|(dpi, _)| (*dpi).clamp(MIN_DPI, MAX_DPI));
    Choice {
        picture,
        reason: match density {
            Some((_, density)) => format!("{size}, {rate}, {density}"),
            None => format!("{size}, {rate}"),
        },
    }
}

/// What the cast claims in M4 for CarPlay's picture: its timing and an H.264 level that holds it.
pub fn video_mode(picture: &Picture) -> VideoMode {
    VideoMode {
        width: picture.width,
        height: picture.height,
        fps: picture.fps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: Settings = Settings {
        size: SizeSetting::Auto,
        fixed_size: (1280, 720),
        rate: RateSetting::Auto,
        custom_size: None,
        density: DensitySetting::Real,
    };

    const NONE: Views = Views {
        full_screen: None,
        landscape_window: None,
        portrait_window: None,
    };

    /// The picture without views measured.
    fn choose(
        settings: &Settings,
        head_unit: Option<(&HeadUnitPicture, Source)>,
        mode: Mode,
    ) -> Choice {
        super::choose(settings, head_unit, &NONE, mode)
    }

    fn tablet() -> GetUCarConfigResponse {
        GetUCarConfigResponse {
            screen_width: 2560,
            screen_height: 1600,
            dpi: 320,
            video_display_width: 2458,
            video_display_height: 1484,
            fps: 30,
            real_dpi: Some(206),
            ..GetUCarConfigResponse::default()
        }
    }

    #[test]
    fn head_unit_picture_from_its_configuration() {
        assert_eq!(
            HeadUnitPicture::from_config(&tablet()),
            Some(HeadUnitPicture {
                width: 2458,
                height: 1484,
                dpi: 320,
                real_dpi: 206,
                fps: 30,
                screen_width: 2560,
                screen_height: 1600,
            })
        );
        // Without the size the cast is shown at, the whole screen.
        let screen_only = GetUCarConfigResponse {
            video_display_width: 0,
            dpi: 0,
            fps: 0,
            ..tablet()
        };
        assert_eq!(
            HeadUnitPicture::from_config(&screen_only),
            Some(HeadUnitPicture {
                width: 2560,
                height: 1600,
                dpi: 0,
                real_dpi: 206,
                fps: 0,
                screen_width: 2560,
                screen_height: 1600,
            })
        );
        assert_eq!(
            HeadUnitPicture::from_config(&GetUCarConfigResponse::default()),
            None
        );
        let negative = GetUCarConfigResponse {
            video_display_height: -1,
            screen_height: -1,
            ..tablet()
        };
        assert_eq!(HeadUnitPicture::from_config(&negative), None);
    }

    #[test]
    fn the_real_density_comes_first() {
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        assert_eq!(unit.density(), Some((206, HeadUnitDensity::Real)));
        // A real density that is missing or out of bounds gives way to the one to lay out with.
        for real_dpi in [0, 71, 641, 5000] {
            let odd = HeadUnitPicture { real_dpi, ..unit };
            assert_eq!(
                odd.density(),
                Some((320, HeadUnitDensity::Layout)),
                "{real_dpi}"
            );
        }
        let edge = HeadUnitPicture {
            real_dpi: 72,
            ..unit
        };
        assert_eq!(edge.density(), Some((72, HeadUnitDensity::Real)));
        let none = HeadUnitPicture {
            dpi: 0,
            real_dpi: 0,
            ..unit
        };
        assert_eq!(none.density(), None);
        let choice = choose(
            &AUTO,
            Some((
                &HeadUnitPicture {
                    real_dpi: 0,
                    ..unit
                },
                Source::Config,
            )),
            Mode::FullScreen,
        );
        assert_eq!(choice.picture.dpi, Some(320));
        assert!(choice
            .reason
            .ends_with(", dpi from the head unit's configuration"));
    }

    #[test]
    fn head_unit_picture_from_its_screen_info() {
        let info = ScreenInfo {
            width: 2458,
            height: 1484,
            dpi: 206,
            window: Some((1280, 2560)),
        };
        assert_eq!(
            HeadUnitPicture::from_screen_info(&info),
            Some(HeadUnitPicture {
                width: 2458,
                height: 1484,
                dpi: 206,
                real_dpi: 0,
                fps: 0,
                screen_width: 0,
                screen_height: 0,
            })
        );
        let none = ScreenInfo { width: 0, ..info };
        assert_eq!(HeadUnitPicture::from_screen_info(&none), None);
    }

    #[test]
    fn automatic_follows_the_head_unit() {
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        let choice = choose(&AUTO, Some((&unit, Source::Config)), Mode::FullScreen);
        assert_eq!(
            choice.picture,
            Picture {
                dpi: Some(206),
                ..Picture::new(2458, 1484, 30)
            }
        );
        assert_eq!(
            choice.reason,
            "size from the head unit's configuration, frame rate from the head unit's configuration, real dpi from the head unit's configuration"
        );
        // A head unit that asks for more gets it, within what CarPlay takes.
        let fast = HeadUnitPicture { fps: 60, ..unit };
        assert_eq!(
            choose(&AUTO, Some((&fast, Source::Kept)), Mode::FullScreen)
                .picture
                .fps,
            60
        );
        let faster = HeadUnitPicture { fps: 120, ..unit };
        assert_eq!(
            choose(&AUTO, Some((&faster, Source::Kept)), Mode::FullScreen)
                .picture
                .fps,
            60
        );
        let slow = HeadUnitPicture { fps: 25, ..unit };
        assert_eq!(
            choose(&AUTO, Some((&slow, Source::Kept)), Mode::FullScreen)
                .picture
                .fps,
            30
        );
        // One that names no rate or density.
        let bare = HeadUnitPicture {
            width: 1921,
            height: 721,
            dpi: 0,
            real_dpi: 0,
            fps: 0,
            screen_width: 0,
            screen_height: 0,
        };
        let choice = choose(&AUTO, Some((&bare, Source::ScreenInfo)), Mode::FullScreen);
        assert_eq!(choice.picture, Picture::new(1920, 720, 30));
        assert_eq!(
            choice.reason,
            "size from the head unit's authentication reply, 30 fps as the head unit's authentication reply names no frame rate"
        );
    }

    #[test]
    fn without_a_head_unit_and_fixed() {
        let choice = choose(&AUTO, None, Mode::FullScreen);
        assert_eq!(choice.picture, Picture::new(1280, 720, 30));
        assert_eq!(
            choice.to_string(),
            "1280x720 at 30 fps (fixed size until a head unit is known, 30 fps until a head unit is known)"
        );
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        let fixed = Settings {
            size: SizeSetting::Fixed,
            fixed_size: (1920, 1080),
            rate: RateSetting::Fixed(60),
            custom_size: None,
            density: DensitySetting::Real,
        };
        for head_unit in [None, Some((&unit, Source::Config))] {
            let choice = choose(&fixed, head_unit, Mode::FullScreen);
            assert_eq!(choice.picture, Picture::new(1920, 1080, 60));
            assert_eq!(choice.reason, "fixed size, fixed frame rate");
        }
        // Each setting on its own.
        let size_only = Settings {
            rate: RateSetting::Auto,
            ..fixed
        };
        assert_eq!(
            choose(&size_only, Some((&unit, Source::Config)), Mode::FullScreen).picture,
            Picture::new(1920, 1080, 30)
        );
        let rate_only = Settings {
            size: SizeSetting::Auto,
            ..fixed
        };
        assert_eq!(
            choose(&rate_only, Some((&unit, Source::Config)), Mode::FullScreen).picture,
            Picture {
                dpi: Some(206),
                ..Picture::new(2458, 1484, 60)
            }
        );
        // A fixed size out of bounds is fitted.
        let huge = Settings {
            fixed_size: (8000, 8000),
            ..fixed
        };
        assert_eq!(
            choose(&huge, None, Mode::FullScreen).picture,
            Picture::new(2160, 2160, 60)
        );
    }

    #[test]
    fn the_head_units_own_size_comes_first() {
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        let custom = Settings {
            custom_size: Some((2432, 1484)),
            ..AUTO
        };
        // Before the head unit's area, keeping its density and frame rate.
        let choice = choose(&custom, Some((&unit, Source::Config)), Mode::FullScreen);
        assert_eq!(
            choice.picture,
            Picture {
                dpi: Some(206),
                ..Picture::new(2432, 1484, 30)
            }
        );
        assert_eq!(
            choice.reason,
            "custom size for this head unit, frame rate from the head unit's configuration, real dpi from the head unit's configuration"
        );
        // Before a fixed size too, with a fixed frame rate.
        let fixed = Settings {
            size: SizeSetting::Fixed,
            fixed_size: (1920, 1080),
            rate: RateSetting::Fixed(60),
            custom_size: Some((2432, 1484)),
            density: DensitySetting::Real,
        };
        let choice = choose(&fixed, Some((&unit, Source::Kept)), Mode::FullScreen);
        assert_eq!(
            choice.picture,
            Picture {
                dpi: Some(206),
                ..Picture::new(2432, 1484, 60)
            }
        );
        assert_eq!(
            choice.to_string(),
            "2432x1484 at 60 fps, 206 dpi (custom size for this head unit, fixed frame rate, real dpi from the head unit's last connection)"
        );
        // Before any head unit is known.
        let choice = choose(&custom, None, Mode::FullScreen);
        assert_eq!(choice.picture, Picture::new(2432, 1484, 30));
        // Without one, the settings' size again.
        assert_eq!(
            choose(&AUTO, Some((&unit, Source::Config)), Mode::FullScreen)
                .picture
                .width,
            2458
        );
        assert_eq!(
            choose(
                &Settings {
                    custom_size: None,
                    ..fixed
                },
                Some((&unit, Source::Config)),
                Mode::FullScreen
            )
            .picture,
            Picture::new(1920, 1080, 60)
        );
    }

    #[test]
    fn modes_of_the_window() {
        assert_eq!(Mode::of(None), Mode::FullScreen);
        assert_eq!(Mode::of(Some(Shape::Landscape)), Mode::LandscapeWindow);
        assert_eq!(Mode::of(Some(Shape::Portrait)), Mode::PortraitWindow);
        assert_eq!(Mode::default(), Mode::FullScreen);
        assert_eq!(Mode::PortraitWindow.to_string(), "portrait window");
    }

    fn c10() -> HeadUnitPicture {
        HeadUnitPicture {
            width: 2458,
            height: 1176,
            dpi: 320,
            real_dpi: 201,
            fps: 30,
            screen_width: 2560,
            screen_height: 1440,
        }
    }

    fn c10_settings() -> Settings {
        Settings {
            custom_size: Some((2432, 1164)),
            rate: RateSetting::Fixed(60),
            ..AUTO
        }
    }

    #[test]
    fn the_density_setting() {
        let unit = c10();
        let head_unit = Some((&unit, Source::Config));
        let real = choose(&c10_settings(), head_unit, Mode::FullScreen);
        assert_eq!(real.picture.dpi, Some(201));
        assert!(real
            .reason
            .ends_with(", real dpi from the head unit's configuration"));
        // The density the head unit lays out with, first.
        let configured = Settings {
            density: DensitySetting::Configured,
            ..c10_settings()
        };
        let choice = choose(&configured, head_unit, Mode::FullScreen);
        assert_eq!(choice.picture.dpi, Some(320));
        assert!(choice
            .reason
            .ends_with(", dpi from the head unit's configuration"));
        // Without it, the real one.
        let no_layout = HeadUnitPicture { dpi: 0, ..unit };
        assert_eq!(
            choose(
                &configured,
                Some((&no_layout, Source::Config)),
                Mode::FullScreen
            )
            .picture
            .dpi,
            Some(201)
        );
        // Without the real one, the default takes the one it lays out with.
        let no_real = HeadUnitPicture {
            real_dpi: 0,
            ..unit
        };
        assert_eq!(
            choose(
                &c10_settings(),
                Some((&no_real, Source::Config)),
                Mode::FullScreen
            )
            .picture
            .dpi,
            Some(320)
        );
        // A density set for the head unit, also with a fixed size and before any head unit is known.
        let custom = Settings {
            density: DensitySetting::Custom(220),
            ..c10_settings()
        };
        let choice = choose(&custom, head_unit, Mode::FullScreen);
        assert_eq!(choice.picture.dpi, Some(220));
        assert_eq!(
            choice.to_string(),
            "2432x1164 at 60 fps, 220 dpi (custom size for this head unit, fixed frame rate, custom dpi for this head unit)"
        );
        let fixed = Settings {
            size: SizeSetting::Fixed,
            custom_size: None,
            ..custom
        };
        assert_eq!(
            choose(&fixed, head_unit, Mode::FullScreen).picture.dpi,
            Some(220)
        );
        assert_eq!(
            choose(&custom, None, Mode::FullScreen).picture.dpi,
            Some(220)
        );
        // A fixed size takes no density of the head unit's.
        let fixed = Settings {
            density: DensitySetting::Real,
            ..fixed
        };
        assert_eq!(
            choose(&fixed, head_unit, Mode::FullScreen).picture.dpi,
            None
        );
        // A density the head unit lays out with out of bounds is brought within them.
        let odd = HeadUnitPicture {
            dpi: 1000,
            real_dpi: 0,
            ..unit
        };
        assert_eq!(
            choose(
                &c10_settings(),
                Some((&odd, Source::Config)),
                Mode::FullScreen
            )
            .picture
            .dpi,
            Some(MAX_DPI)
        );
    }

    #[test]
    fn without_views_every_mode_has_the_screens_density() {
        let unit = c10();
        for mode in [
            Mode::FullScreen,
            Mode::LandscapeWindow,
            Mode::PortraitWindow,
        ] {
            let choice = choose(&c10_settings(), Some((&unit, Source::Config)), mode);
            assert_eq!(choice.picture.dpi, Some(201), "{mode}");
        }
    }

    #[test]
    fn automatic_size_is_the_full_screen_view_once_measured() {
        let unit = c10();
        let head_unit = Some((&unit, Source::Config));
        let size = |views: &Views| {
            let picture = super::choose(&AUTO, head_unit, views, Mode::FullScreen).picture;
            (picture.width, picture.height)
        };
        // Until the view is measured: the area the head unit names.
        assert_eq!(size(&Views::default()), (2458, 1176));
        // Measured for the area's own picture, or for the view's size, which has the area's shape: the view.
        let measured = |picture| Views {
            full_screen: Some(crate::views::Measured {
                view: PictureSize {
                    width: 2432,
                    height: 1164,
                },
                picture,
            }),
            ..Views::default()
        };
        let area = PictureSize {
            width: 2458,
            height: 1176,
        };
        let view = PictureSize {
            width: 2432,
            height: 1164,
        };
        assert_eq!(size(&measured(area)), (2432, 1164));
        assert_eq!(size(&measured(view)), (2432, 1164));
        let choice = super::choose(&AUTO, head_unit, &measured(area), Mode::FullScreen);
        assert!(choice
            .reason
            .starts_with("size of the full screen view measured for the area 2458x1176"));
        // At the view's size the full screen density is the screen's as it is.
        assert_eq!(choice.picture.dpi, Some(201));
        // Measured for a picture of another shape: the area again.
        let other = PictureSize {
            width: 1920,
            height: 1080,
        };
        assert_eq!(size(&measured(other)), (2458, 1176));
        // A size set for the head unit comes first.
        let custom = super::choose(
            &c10_settings(),
            head_unit,
            &measured(area),
            Mode::FullScreen,
        )
        .picture;
        assert_eq!((custom.width, custom.height), (2432, 1164));
    }

    #[test]
    fn views_calibrate_each_mode() {
        let views = crate::views::tests::c10();
        let unit = c10();
        let head_unit = Some((&unit, Source::Config));
        let dpi = |settings: &Settings, mode| {
            super::choose(settings, head_unit, &views, mode).picture.dpi
        };
        let settings = c10_settings();
        assert_eq!(dpi(&settings, Mode::FullScreen), Some(201));
        assert_eq!(dpi(&settings, Mode::LandscapeWindow), Some(286));
        assert_eq!(dpi(&settings, Mode::PortraitWindow), Some(443));
        let landscape = super::choose(&settings, head_unit, &views, Mode::LandscapeWindow);
        assert_eq!(
            landscape.to_string(),
            "2432x1164 at 60 fps, 286 dpi (custom size for this head unit, fixed frame rate, dpi calibrated from the landscape window's view 1712x819 at the real dpi 201 from the head unit's configuration)"
        );
        // The picture stays landscape: the portrait window's is turned by CarPlay's side.
        assert_eq!(
            (landscape.picture.width, landscape.picture.height),
            (2432, 1164)
        );
        // Without a size of its own the picture is the full screen view measured: the same densities.
        let own = Settings {
            custom_size: None,
            ..settings
        };
        assert_eq!(dpi(&own, Mode::LandscapeWindow), Some(286));
        assert_eq!(dpi(&own, Mode::PortraitWindow), Some(443));
        // With the full screen view not measured, it is the area the head unit names, of the same shape: the windows'
        // views calibrate it alike.
        let windows = Views {
            full_screen: None,
            ..views
        };
        let windows_dpi = |mode| super::choose(&own, head_unit, &windows, mode).picture.dpi;
        assert_eq!(windows_dpi(Mode::LandscapeWindow), Some(289));
        assert_eq!(windows_dpi(Mode::PortraitWindow), Some(448));
        // Another density of the screen scales all of them.
        let custom = Settings {
            density: DensitySetting::Custom(160),
            ..settings
        };
        assert_eq!(dpi(&custom, Mode::FullScreen), Some(160));
        assert_eq!(dpi(&custom, Mode::LandscapeWindow), Some(227));
        // A picture of another shape has no view yet: the screen's density as it is.
        let wide = Settings {
            custom_size: Some((1920, 720)),
            ..settings
        };
        assert_eq!(dpi(&wide, Mode::LandscapeWindow), Some(201));
        // A view with no density to start from gives none.
        let fixed = Settings {
            size: SizeSetting::Fixed,
            custom_size: None,
            fixed_size: (2432, 1164),
            ..settings
        };
        assert_eq!(dpi(&fixed, Mode::LandscapeWindow), None);
    }

    #[test]
    fn pictures_as_each_mode_shows_them() {
        let size = PictureSize {
            width: 2432,
            height: 1164,
        };
        let upright = PictureSize {
            width: 1164,
            height: 2432,
        };
        assert_eq!(Mode::FullScreen.shown(size), size);
        assert_eq!(Mode::FullScreen.shown(upright), upright);
        assert_eq!(Mode::LandscapeWindow.shown(upright), size);
        assert_eq!(Mode::PortraitWindow.shown(size), upright);
    }

    #[test]
    fn same_head_unit_same_choice() {
        // The kept picture and the configuration that gave it choose the same picture: no reconnection.
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        assert_eq!(
            choose(&AUTO, Some((&unit, Source::Kept)), Mode::FullScreen).picture,
            choose(&AUTO, Some((&unit, Source::Config)), Mode::FullScreen).picture
        );
    }

    #[test]
    fn m4_follows_the_picture() {
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        let mode =
            video_mode(&choose(&AUTO, Some((&unit, Source::Config)), Mode::FullScreen).picture);
        assert_eq!(mode.timing(), 30);
        assert_eq!(mode.level().1, "5");
        let fast = HeadUnitPicture { fps: 60, ..unit };
        let mode =
            video_mode(&choose(&AUTO, Some((&fast, Source::Config)), Mode::FullScreen).picture);
        assert_eq!(mode.timing(), 60);
        assert_eq!(mode.level().1, "5.1");
        assert_eq!(
            video_mode(&choose(&AUTO, None, Mode::FullScreen).picture).timing(),
            30
        );
    }

    #[test]
    fn kept_as_json() {
        let unit = HeadUnitPicture::from_config(&tablet()).unwrap();
        let text = serde_json::to_string(&unit).unwrap();
        assert_eq!(
            text,
            r#"{"width":2458,"height":1484,"dpi":320,"real_dpi":206,"fps":30,"screen_width":2560,"screen_height":1600}"#
        );
        assert_eq!(
            serde_json::from_str::<HeadUnitPicture>(&text).unwrap(),
            unit
        );
        assert_eq!(
            serde_json::from_str::<HeadUnitPicture>(r#"{"width":800,"height":480}"#).unwrap(),
            HeadUnitPicture {
                width: 800,
                height: 480,
                dpi: 0,
                real_dpi: 0,
                fps: 0,
                screen_width: 0,
                screen_height: 0,
            }
        );
    }
}
