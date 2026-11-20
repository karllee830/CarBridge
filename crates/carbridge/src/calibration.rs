// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Learning the head unit's views ([`crate::views`]) from its touches, and the one-time calibration of its window
//! modes.
//!
//! Every touch names the size of the view it was made on; it is kept as the view of the mode the head unit shows the
//! cast in, for the picture it shows there. Full screen, the view is learned from ordinary touches, which go on to
//! the iPhone. When the head unit switches to a window, landscape or portrait, whose view for the picture is not
//! known, the window is calibrated: CarPlay is not shown there, a screen asks for one touch ([`Screen::Calibrate`]),
//! and the first touch that starts on it gives the view. That touch, to its end, does not go to the iPhone. Then a
//! screen says the calibration is done ([`Screen::Calibrated`]) until CarPlay, connected again if the picture's
//! density changed with the view, shows in the window. The calibration screen stays until the touch comes; when the
//! head unit switches to another mode before, that mode is followed, and calibrated if it needs it.

use std::fmt;

use carbridge_carlink_session::display::PictureSize;

use crate::picture::Mode;
use crate::views::{Measured, Views};

/// Android's motion event action of a first finger down.
const ACTION_DOWN: i64 = 0;

/// What the cast shows the head unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// CarPlay, or the waiting screen while it shows nothing.
    #[default]
    CarPlay,
    /// The screen that asks for a touch to measure the window.
    Calibrate,
    /// The screen that says the window is calibrated, until CarPlay shows in it.
    Calibrated,
}

impl fmt::Display for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Screen::CarPlay => "CarPlay",
            Screen::Calibrate => "the calibration screen",
            Screen::Calibrated => "the calibration done screen",
        })
    }
}

/// A view measured from a touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub mode: Mode,
    pub measured: Measured,
    /// It calibrated the mode: the touch came on the calibration screen.
    pub calibrated: bool,
}

/// What becomes of a touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Touched {
    /// It goes to the iPhone.
    pub forward: bool,
    /// The view it gives, when that is new.
    pub measured: Option<Measurement>,
}

/// The views of the head unit connected now and the calibration of its mode.
#[derive(Clone, Debug)]
pub struct Calibration {
    views: Views,
    mode: Mode,
    /// The picture as the head unit shows it in `mode`, which a touch's view is measured for; `None` while there is
    /// none, without CarPlay.
    picture: Option<PictureSize>,
    calibrating: bool,
    /// The rest of the touch that calibrated a mode is held back, until the next touch starts.
    held: bool,
}

impl Calibration {
    /// For a head unit with `views` that shows the cast in `mode`, at `picture` as it shows it there; calibrating
    /// already when `mode` needs it.
    pub fn new(views: Views, mode: Mode, picture: Option<PictureSize>) -> Calibration {
        let mut calibration = Calibration {
            views,
            mode,
            picture,
            calibrating: false,
            held: false,
        };
        calibration.calibrating = calibration.needs(mode);
        calibration
    }

    pub fn views(&self) -> &Views {
        &self.views
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn calibrating(&self) -> bool {
        self.calibrating
    }

    /// What the cast shows while CarPlay is not to show, or [`Screen::CarPlay`].
    pub fn screen(&self) -> Screen {
        if self.calibrating {
            Screen::Calibrate
        } else {
            Screen::CarPlay
        }
    }

    /// Whether `mode` is calibrated before CarPlay shows in it: a window whose view of the picture is not known.
    pub fn needs(&self, mode: Mode) -> bool {
        mode != Mode::FullScreen
            && self
                .picture
                .is_some_and(|picture| self.views.view_for(mode, picture).is_none())
    }

    /// The head unit shows the cast in `mode` now, at `picture` as it shows it there. Returns whether the mode is
    /// calibrated now.
    pub fn display(&mut self, mode: Mode, picture: Option<PictureSize>) -> bool {
        self.mode = mode;
        self.picture = picture;
        self.calibrating = self.needs(mode);
        self.calibrating
    }

    /// The head unit shows the cast at `picture` now, in the same mode. A calibration that goes on goes on; none
    /// starts, as the view of the new picture is learned from the next touch.
    pub fn picture(&mut self, picture: Option<PictureSize>) {
        self.picture = picture;
    }

    /// Takes `views` as the head unit's, as when they were cleared to be measured again. A calibration starts only
    /// when the head unit next switches to a window that needs it.
    pub fn set_views(&mut self, views: Views) {
        self.views = views;
    }

    /// A touch: its Android motion event `action` and the size of the view it was made on, `width` × `height`.
    pub fn touch(&mut self, action: i64, width: i64, height: i64) -> Touched {
        let starts = action & 0xff == ACTION_DOWN;
        let side = |value: i64| u32::try_from(value).ok().filter(|value| *value > 0);
        let measured = match (side(width), side(height), self.picture) {
            (Some(width), Some(height), Some(picture)) => Some(Measured {
                view: PictureSize { width, height },
                picture,
            }),
            _ => None,
        };
        if self.calibrating {
            let Some(measured) = measured.filter(|_| starts) else {
                return Touched {
                    forward: false,
                    measured: None,
                };
            };
            self.views.set(self.mode, Some(measured));
            self.calibrating = false;
            self.held = true;
            return Touched {
                forward: false,
                measured: Some(Measurement {
                    mode: self.mode,
                    measured,
                    calibrated: true,
                }),
            };
        }
        if self.held {
            if !starts {
                return Touched {
                    forward: false,
                    measured: None,
                };
            }
            self.held = false;
        }
        let new = measured.filter(|measured| self.views.of(self.mode) != Some(*measured));
        if let Some(measured) = new {
            self.views.set(self.mode, Some(measured));
        }
        Touched {
            forward: true,
            measured: new.map(|measured| Measurement {
                mode: self.mode,
                measured,
                calibrated: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOWN: i64 = 0;
    const UP: i64 = 1;
    const MOVE: i64 = 2;

    fn size(width: u32, height: u32) -> PictureSize {
        PictureSize { width, height }
    }

    const TABLET: PictureSize = PictureSize {
        width: 2458,
        height: 1484,
    };
    const UPRIGHT: PictureSize = PictureSize {
        width: 1484,
        height: 2458,
    };

    fn forwarded() -> Touched {
        Touched {
            forward: true,
            measured: None,
        }
    }

    fn held() -> Touched {
        Touched {
            forward: false,
            measured: None,
        }
    }

    #[test]
    fn full_screen_is_learned_from_ordinary_touches() {
        let mut calibration = Calibration::new(Views::default(), Mode::FullScreen, Some(TABLET));
        assert!(!calibration.calibrating());
        assert_eq!(calibration.screen(), Screen::CarPlay);
        let touched = calibration.touch(DOWN, 2432, 1468);
        assert!(touched.forward);
        let measured = Measured {
            view: size(2432, 1468),
            picture: TABLET,
        };
        assert_eq!(
            touched.measured,
            Some(Measurement {
                mode: Mode::FullScreen,
                measured,
                calibrated: false
            })
        );
        assert_eq!(calibration.views().full_screen, Some(measured));
        // The same view again is nothing new; the rest of the touch goes on.
        assert_eq!(calibration.touch(MOVE, 2432, 1468), forwarded());
        assert_eq!(calibration.touch(UP, 2432, 1468), forwarded());
        // A touch without a view's size measures nothing.
        assert_eq!(calibration.touch(DOWN, 0, 0), forwarded());
    }

    #[test]
    fn a_window_without_its_view_is_calibrated_by_one_touch() {
        let mut calibration = Calibration::new(Views::default(), Mode::FullScreen, Some(TABLET));
        assert!(calibration.display(Mode::LandscapeWindow, Some(TABLET)));
        assert_eq!(calibration.screen(), Screen::Calibrate);
        // The calibration waits for a touch that starts, with a view's size: the rest of one that started before
        // goes nowhere.
        assert_eq!(calibration.touch(MOVE, 1712, 1034), held());
        assert_eq!(calibration.touch(UP, 1712, 1034), held());
        assert_eq!(calibration.touch(DOWN, 0, 0), held());
        let measured = Measured {
            view: size(1712, 1034),
            picture: TABLET,
        };
        assert_eq!(
            calibration.touch(DOWN, 1712, 1034),
            Touched {
                forward: false,
                measured: Some(Measurement {
                    mode: Mode::LandscapeWindow,
                    measured,
                    calibrated: true
                })
            }
        );
        assert!(!calibration.calibrating());
        assert_eq!(calibration.views().landscape_window, Some(measured));
        // The rest of that touch does not go to the iPhone either; the next one does.
        assert_eq!(calibration.touch(MOVE, 1712, 1034), held());
        assert_eq!(calibration.touch(UP, 1712, 1034), held());
        assert_eq!(calibration.touch(DOWN, 1712, 1034), forwarded());
        // Known now: switching back does not calibrate again.
        assert!(!calibration.display(Mode::FullScreen, Some(TABLET)));
        assert!(!calibration.display(Mode::LandscapeWindow, Some(TABLET)));
    }

    #[test]
    fn the_portrait_window_is_calibrated_for_the_upright_picture() {
        let mut calibration = Calibration::new(Views::default(), Mode::FullScreen, Some(TABLET));
        assert!(calibration.display(Mode::PortraitWindow, Some(UPRIGHT)));
        let touched = calibration.touch(DOWN, 852, 1412);
        assert_eq!(
            touched
                .measured
                .map(|m| (m.mode, m.measured.picture, m.calibrated)),
            Some((Mode::PortraitWindow, UPRIGHT, true))
        );
        assert!(!calibration.needs(Mode::PortraitWindow));
        assert!(calibration.needs(Mode::LandscapeWindow));
        assert!(!calibration.needs(Mode::FullScreen));
    }

    #[test]
    fn a_switch_during_the_calibration_follows_the_new_mode() {
        let mut calibration =
            Calibration::new(Views::default(), Mode::LandscapeWindow, Some(TABLET));
        assert!(calibration.calibrating());
        // To the portrait window, not known either: that one is calibrated.
        assert!(calibration.display(Mode::PortraitWindow, Some(UPRIGHT)));
        assert_eq!(calibration.mode(), Mode::PortraitWindow);
        // To full screen: no calibration, and touches go to the iPhone again.
        assert!(!calibration.display(Mode::FullScreen, Some(TABLET)));
        assert_eq!(calibration.screen(), Screen::CarPlay);
        assert!(calibration.touch(DOWN, 2432, 1468).forward);
        assert!(calibration.views().landscape_window.is_none());
        assert!(calibration.views().portrait_window.is_none());
    }

    #[test]
    fn known_views_need_no_calibration() {
        let views = crate::views::tests::c10();
        let c10 = size(2432, 1164);
        let mut calibration = Calibration::new(views, Mode::FullScreen, Some(c10));
        for mode in [Mode::LandscapeWindow, Mode::PortraitWindow] {
            assert!(!calibration.display(mode, Some(mode.shown(c10))), "{mode}");
        }
        // Nor for a picture of the same shape.
        let own = size(2458, 1176);
        assert!(!calibration.display(Mode::LandscapeWindow, Some(own)));
        // One of another shape does.
        assert!(calibration.display(Mode::LandscapeWindow, Some(size(1920, 1080))));
        // Without a picture, there is nothing to calibrate for.
        assert!(!calibration.display(Mode::PortraitWindow, None));
        let mut none = Calibration::new(Views::default(), Mode::LandscapeWindow, None);
        assert!(!none.calibrating());
        assert_eq!(none.touch(DOWN, 1712, 819), forwarded());
    }

    #[test]
    fn a_window_whose_picture_changes_is_learned_without_a_calibration() {
        let views = crate::views::tests::c10();
        let mut calibration =
            Calibration::new(views, Mode::LandscapeWindow, Some(size(2432, 1164)));
        calibration.picture(Some(size(1920, 1080)));
        assert!(!calibration.calibrating());
        let touched = calibration.touch(DOWN, 1456, 819);
        assert!(touched.forward);
        assert_eq!(
            touched.measured.map(|m| (m.measured.view, m.calibrated)),
            Some((size(1456, 819), false))
        );
    }

    #[test]
    fn views_cleared_are_calibrated_at_the_next_switch() {
        let views = crate::views::tests::c10();
        let c10 = size(2432, 1164);
        let mut calibration = Calibration::new(views, Mode::LandscapeWindow, Some(c10));
        calibration.set_views(Views::default());
        assert!(!calibration.calibrating());
        assert!(calibration.display(Mode::LandscapeWindow, Some(c10)));
    }
}
