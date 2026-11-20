// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The picture an iPhone renders for CarPlay's main screen: its size, frame rate and density.
//!
//! Each session is given the picture of the moment when the iPhone connects ([`crate::receiver::Receiver::set_picture`]),
//! fitted to what AirPlay takes ([`Picture::fitted`]).

use std::fmt;

use carbridge_airplay::info::{self, Display};

/// The bounds of a picture's sides in pixels, of a landscape one as they are and of a portrait one turned: the shortest
/// and longest of its long side, and of its short side.
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 3840;
pub const MIN_HEIGHT: u32 = 240;
pub const MAX_HEIGHT: u32 = 2160;
/// The lowest and highest density the picture's physical size is worked out from, in dots per inch.
pub const MIN_DPI: u32 = 72;
pub const MAX_DPI: u32 = 640;
/// Millimetres in an inch.
const MM_PER_INCH: f64 = 25.4;

/// The picture of CarPlay's main screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    /// Frames a second.
    pub fps: u32,
    /// The screen's density, which gives the picture's physical size; without it AirPlay's default size is reported.
    pub dpi: Option<u32>,
}

impl Picture {
    pub fn new(width: u32, height: u32, fps: u32) -> Picture {
        Picture {
            width,
            height,
            fps,
            dpi: None,
        }
    }

    /// The picture as AirPlay takes it: a picture larger than [`MAX_WIDTH`] × [`MAX_HEIGHT`], or a portrait one larger
    /// than [`MAX_HEIGHT`] × [`MAX_WIDTH`], scaled down to fit, keeping its shape; each side at least the smallest and
    /// rounded down to an even number of pixels; the frame rate within 30 to 60 in steps of 5; the density within
    /// [`MIN_DPI`] to [`MAX_DPI`]. A picture turned and then fitted is the fitted one turned.
    pub fn fitted(self) -> Picture {
        let (width, height) = (self.width.max(1), self.height.max(1));
        let (long, short) = (width.max(height), width.min(height));
        let (mut long, mut short) = (f64::from(long), f64::from(short));
        let scale = (f64::from(MAX_WIDTH) / long)
            .min(f64::from(MAX_HEIGHT) / short)
            .min(1.0);
        long *= scale;
        short *= scale;
        let side = |value: f64, min: u32, max: u32| ((value.round() as u32).clamp(min, max)) & !1;
        let (long, short) = (
            side(long, MIN_WIDTH, MAX_WIDTH),
            side(short, MIN_HEIGHT, MAX_HEIGHT),
        );
        let (width, height) = if height > width {
            (short, long)
        } else {
            (long, short)
        };
        Picture {
            width,
            height,
            fps: info::sanitize_fps(self.fps),
            dpi: self.dpi.map(|dpi| dpi.clamp(MIN_DPI, MAX_DPI)),
        }
    }

    /// Whether the picture is taller than it is wide.
    pub fn is_portrait(&self) -> bool {
        self.height > self.width
    }

    /// The picture turned upright when `portrait`, or on its side when not: its sides swapped when it is the other
    /// way. Its frame rate and density stay.
    pub fn turned(self, portrait: bool) -> Picture {
        if self.is_portrait() == portrait || self.width == self.height {
            self
        } else {
            Picture {
                width: self.height,
                height: self.width,
                ..self
            }
        }
    }

    /// The picture's physical width and height in millimetres, from its density.
    pub fn physical_mm(&self) -> Option<(u32, u32)> {
        let dpi = f64::from(self.dpi?.max(1));
        let mm = |pixels: u32| ((f64::from(pixels) * MM_PER_INCH / dpi).round() as u32).max(1);
        Some((mm(self.width), mm(self.height)))
    }

    /// Sets AirPlay's main display to this picture.
    pub fn apply(&self, main: &mut Display) {
        main.width = self.width;
        main.height = self.height;
        main.fps = self.fps;
        let physical = self.physical_mm();
        main.width_mm = physical.map(|(w, _)| w);
        main.height_mm = physical.map(|(_, h)| h);
    }
}

impl fmt::Display for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{} at {} fps", self.width, self.height, self.fps)?;
        if let Some(dpi) = self.dpi {
            write!(f, ", {dpi} dpi")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_are_fitted_to_airplay() {
        // The head unit's sizes stay as they are.
        let tablet = Picture {
            dpi: Some(320),
            ..Picture::new(2458, 1484, 30)
        };
        assert_eq!(tablet.fitted(), tablet);
        assert_eq!(
            Picture::new(1280, 720, 60).fitted(),
            Picture::new(1280, 720, 60)
        );
        // Odd sides are made even.
        assert_eq!(
            Picture::new(1281, 721, 30).fitted(),
            Picture::new(1280, 720, 30)
        );
        // Too large is scaled down with its shape, too small is raised.
        assert_eq!(
            Picture::new(7680, 2160, 30).fitted(),
            Picture::new(3840, 1080, 30)
        );
        assert_eq!(
            Picture::new(100, 50, 30).fitted(),
            Picture::new(320, 240, 30)
        );
        assert_eq!(Picture::new(0, 0, 30).fitted(), Picture::new(320, 240, 30));
        // Frame rates within 30 to 60, in steps of 5.
        assert_eq!(Picture::new(1280, 720, 25).fitted().fps, 30);
        assert_eq!(Picture::new(1280, 720, 0).fitted().fps, 30);
        assert_eq!(Picture::new(1280, 720, 48).fitted().fps, 50);
        assert_eq!(Picture::new(1280, 720, 120).fitted().fps, 60);
        // Densities within the bounds.
        let dense = Picture {
            dpi: Some(1000),
            ..Picture::new(1280, 720, 30)
        };
        assert_eq!(dense.fitted().dpi, Some(MAX_DPI));
        // Fitting twice changes nothing more.
        let odd = Picture {
            dpi: Some(10),
            ..Picture::new(5001, 999, 33)
        };
        assert_eq!(odd.fitted().fitted(), odd.fitted());
    }

    #[test]
    fn portrait_pictures_are_fitted_as_landscape_ones_turned() {
        let upright = Picture {
            dpi: Some(320),
            ..Picture::new(1484, 2458, 30)
        };
        assert_eq!(upright.fitted(), upright);
        assert!(upright.is_portrait());
        for (width, height) in [
            (2458, 1484),
            (7680, 2160),
            (5001, 999),
            (100, 50),
            (1281, 721),
            (8000, 8000),
            (4000, 4000),
        ] {
            let picture = Picture::new(width, height, 30);
            assert_eq!(
                picture.turned(true).fitted(),
                picture.fitted().turned(true),
                "{width}x{height}"
            );
            assert_eq!(
                picture.turned(true).fitted().turned(false),
                picture.fitted(),
                "{width}x{height}"
            );
        }
        assert_eq!(
            Picture::new(2160, 7680, 30).fitted(),
            Picture::new(1080, 3840, 30)
        );
        assert_eq!(
            Picture::new(50, 100, 30).fitted(),
            Picture::new(240, 320, 30)
        );
    }

    #[test]
    fn pictures_turn() {
        let tablet = Picture {
            dpi: Some(320),
            ..Picture::new(2458, 1484, 60)
        };
        let upright = tablet.turned(true);
        assert_eq!(
            upright,
            Picture {
                dpi: Some(320),
                ..Picture::new(1484, 2458, 60)
            }
        );
        assert_eq!(tablet.turned(false), tablet);
        assert_eq!(upright.turned(false), tablet);
        assert_eq!(upright.turned(true), upright);
        let square = Picture::new(1200, 1200, 30);
        assert_eq!(square.turned(true), square);
    }

    #[test]
    fn physical_size_from_density() {
        let picture = Picture {
            dpi: Some(320),
            ..Picture::new(2458, 1176, 30)
        };
        assert_eq!(picture.physical_mm(), Some((195, 93)));
        assert_eq!(Picture::new(1280, 720, 30).physical_mm(), None);

        let mut main = Display::new(800, 480);
        picture.apply(&mut main);
        assert_eq!((main.width, main.height, main.fps), (2458, 1176, 30));
        assert_eq!((main.width_mm, main.height_mm), (Some(195), Some(93)));
        Picture::new(1280, 720, 60).apply(&mut main);
        assert_eq!((main.width_mm, main.fps), (None, 60));
    }

    #[test]
    fn shown_in_the_log() {
        assert_eq!(
            Picture::new(1280, 720, 30).to_string(),
            "1280x720 at 30 fps"
        );
        let picture = Picture {
            dpi: Some(320),
            ..Picture::new(2458, 1484, 30)
        };
        assert_eq!(picture.to_string(), "2458x1484 at 30 fps, 320 dpi");
    }
}
