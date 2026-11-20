// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! How the head unit shows the cast: full screen, as its screen is, or in its window, whose shape follows its window
//! mode.
//!
//! A head unit in a window work mode starts the virtual app of the mode it casts in: 10 for the mirror window, whose
//! window is landscape, 11 for the simple launcher window, whose window is portrait. The mode menu of its window starts
//! the other mode's app. Its window button, between full screen and the window, comes back as `InvokeApp` with the
//! UPDATE state; the display mode the request carries is a flag of the head unit's own view, not the mode it wants,
//! so the button switches. The phone reports the display mode in the visible region of the app's state, and the head
//! unit shows the cast full screen or in its window as that mode says, sized by the region.

use std::fmt;

use carbridge_carlink_proto::pb::DisplayVisibleRegion;

/// The display modes of a visible region.
pub const PORTRAIT_WINDOW: i32 = 0;
pub const LANDSCAPE_WINDOW: i32 = 1;
pub const PORTRAIT_FULL_SCREEN: i32 = 2;
pub const LANDSCAPE_FULL_SCREEN: i32 = 3;

/// The work mode bits of the two window casts, as the head unit names the one it picks in its authentication reply.
pub const WORK_MODE_MIRROR_WINDOW: i32 = 256;
pub const WORK_MODE_SIMPLE_WINDOW: i32 = 512;

/// The virtual apps a head unit starts for its window modes.
pub const MIRROR_WINDOW_APP: i32 = 10;
pub const SIMPLE_WINDOW_APP: i32 = 11;

/// Landscape or portrait; a square picture counts as landscape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Landscape,
    Portrait,
}

impl Shape {
    /// The shape of a picture `width` × `height`.
    pub fn of(width: u32, height: u32) -> Shape {
        if height > width {
            Shape::Portrait
        } else {
            Shape::Landscape
        }
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Shape::Landscape => "landscape",
            Shape::Portrait => "portrait",
        })
    }
}

/// Full screen or in the head unit's window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    FullScreen,
    Window,
}

impl Layout {
    /// The other one, as the head unit's window button switches.
    pub fn switched(self) -> Layout {
        match self {
            Layout::FullScreen => Layout::Window,
            Layout::Window => Layout::FullScreen,
        }
    }
}

/// The head unit's window mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMode {
    /// The mirror window cast: a landscape window.
    Mirror,
    /// The simple launcher window cast: a portrait window.
    Simple,
}

impl WindowMode {
    /// The window mode among the work modes a head unit picked: the mirror window when it names it, the simple
    /// launcher window when it names that alone.
    pub fn from_work_mode(bits: i32) -> Option<WindowMode> {
        if bits & WORK_MODE_MIRROR_WINDOW != 0 {
            Some(WindowMode::Mirror)
        } else if bits & WORK_MODE_SIMPLE_WINDOW != 0 {
            Some(WindowMode::Simple)
        } else {
            None
        }
    }

    /// The window mode whose virtual app is `app`.
    pub fn from_app(app: i32) -> Option<WindowMode> {
        match app {
            MIRROR_WINDOW_APP => Some(WindowMode::Mirror),
            SIMPLE_WINDOW_APP => Some(WindowMode::Simple),
            _ => None,
        }
    }

    /// The shape of the window.
    pub fn shape(self) -> Shape {
        match self {
            WindowMode::Mirror => Shape::Landscape,
            WindowMode::Simple => Shape::Portrait,
        }
    }

    /// The name kept for the mode.
    pub fn name(self) -> &'static str {
        match self {
            WindowMode::Mirror => "mirror",
            WindowMode::Simple => "simple",
        }
    }

    /// The mode a name kept with [`WindowMode::name`] stands for.
    pub fn from_name(name: &str) -> Option<WindowMode> {
        match name {
            "mirror" => Some(WindowMode::Mirror),
            "simple" => Some(WindowMode::Simple),
            _ => None,
        }
    }
}

/// How the head unit shows the cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub layout: Layout,
    /// The head unit's window mode, when it casts in one.
    pub window_mode: Option<WindowMode>,
    /// The shape of the head unit's screen.
    pub screen: Shape,
}

impl Default for Display {
    fn default() -> Display {
        Display {
            layout: Layout::FullScreen,
            window_mode: None,
            screen: Shape::Landscape,
        }
    }
}

impl Display {
    /// The shape of the picture the head unit shows: the screen's in full screen, the window's in the window. A full
    /// screen of another shape than the screen's is not offered, as a head unit stretches it over the whole screen.
    pub fn shape(&self) -> Shape {
        match (self.layout, self.window_mode) {
            (Layout::Window, Some(mode)) => mode.shape(),
            _ => self.screen,
        }
    }

    /// The display mode the visible region names.
    pub fn mode(&self) -> i32 {
        match (self.layout, self.shape()) {
            (Layout::Window, Shape::Portrait) => PORTRAIT_WINDOW,
            (Layout::Window, Shape::Landscape) => LANDSCAPE_WINDOW,
            (Layout::FullScreen, Shape::Portrait) => PORTRAIT_FULL_SCREEN,
            (Layout::FullScreen, Shape::Landscape) => LANDSCAPE_FULL_SCREEN,
        }
    }

    /// The picture the head unit shows for a picture of `size`, of whichever shape: `size` turned to [`Display::shape`].
    pub fn picture(&self, size: PictureSize) -> PictureSize {
        size.turned(self.shape())
    }

    /// The region of the head unit's view the phone's picture fills: all of the picture of `size` turned to the
    /// display's shape, in the display's mode.
    pub fn region(&self, size: PictureSize) -> DisplayVisibleRegion {
        DisplayVisibleRegion {
            mode: self.mode(),
            ..visible_region(self.picture(size))
        }
    }
}

impl fmt::Display for Display {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.layout, self.window_mode) {
            (Layout::FullScreen, _) => write!(f, "full screen, {}", self.shape()),
            (Layout::Window, Some(WindowMode::Mirror)) => {
                write!(f, "mirror window, {}", self.shape())
            }
            (Layout::Window, Some(WindowMode::Simple)) => {
                write!(f, "simple launcher window, {}", self.shape())
            }
            (Layout::Window, None) => write!(f, "window, {}", self.shape()),
        }
    }
}

/// The size of the phone's picture, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PictureSize {
    pub width: u32,
    pub height: u32,
}

impl PictureSize {
    pub fn shape(&self) -> Shape {
        Shape::of(self.width, self.height)
    }

    /// The picture turned to `shape`: its sides swapped when its shape is the other one.
    pub fn turned(self, shape: Shape) -> PictureSize {
        if self.shape() == shape || self.width == self.height {
            self
        } else {
            PictureSize {
                width: self.height,
                height: self.width,
            }
        }
    }
}

impl fmt::Display for PictureSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// The region of the head unit's view the phone's picture fills: all of a picture of `size`, in full screen,
/// landscape or portrait by its shape.
pub fn visible_region(size: PictureSize) -> DisplayVisibleRegion {
    let side = |pixels: u32| i32::try_from(pixels).unwrap_or(i32::MAX);
    let (width, height) = (side(size.width), side(size.height));
    DisplayVisibleRegion {
        mode: if size.shape() == Shape::Portrait {
            PORTRAIT_FULL_SCREEN
        } else {
            LANDSCAPE_FULL_SCREEN
        },
        origin_width: width,
        origin_height: height,
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLET: PictureSize = PictureSize {
        width: 2458,
        height: 1484,
    };
    const UPRIGHT: PictureSize = PictureSize {
        width: 1484,
        height: 2458,
    };

    fn display(layout: Layout, window_mode: Option<WindowMode>, screen: Shape) -> Display {
        Display {
            layout,
            window_mode,
            screen,
        }
    }

    #[test]
    fn window_modes_and_their_apps() {
        assert_eq!(WindowMode::from_work_mode(256), Some(WindowMode::Mirror));
        assert_eq!(WindowMode::from_work_mode(512), Some(WindowMode::Simple));
        assert_eq!(WindowMode::from_work_mode(768), Some(WindowMode::Mirror));
        assert_eq!(WindowMode::from_work_mode(1), None);
        assert_eq!(WindowMode::from_work_mode(0), None);
        assert_eq!(WindowMode::from_app(10), Some(WindowMode::Mirror));
        assert_eq!(WindowMode::from_app(11), Some(WindowMode::Simple));
        assert_eq!(WindowMode::from_app(12), None);
        for mode in [WindowMode::Mirror, WindowMode::Simple] {
            assert_eq!(WindowMode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(WindowMode::from_name("portrait"), None);
    }

    #[test]
    fn display_modes_and_shapes() {
        let landscape = Shape::Landscape;
        let full = display(Layout::FullScreen, Some(WindowMode::Simple), landscape);
        assert_eq!(
            (full.shape(), full.mode()),
            (landscape, LANDSCAPE_FULL_SCREEN)
        );
        let mirror = display(Layout::Window, Some(WindowMode::Mirror), landscape);
        assert_eq!(
            (mirror.shape(), mirror.mode()),
            (landscape, LANDSCAPE_WINDOW)
        );
        let simple = display(Layout::Window, Some(WindowMode::Simple), landscape);
        assert_eq!(
            (simple.shape(), simple.mode()),
            (Shape::Portrait, PORTRAIT_WINDOW)
        );
        // A window with no window mode keeps the screen's shape.
        let bare = display(Layout::Window, None, landscape);
        assert_eq!((bare.shape(), bare.mode()), (landscape, LANDSCAPE_WINDOW));
        // A portrait screen: full screen portrait, the mirror window landscape.
        let tall = display(
            Layout::FullScreen,
            Some(WindowMode::Mirror),
            Shape::Portrait,
        );
        assert_eq!(tall.mode(), PORTRAIT_FULL_SCREEN);
        let tall_mirror = Display {
            layout: Layout::Window,
            ..tall
        };
        assert_eq!(tall_mirror.mode(), LANDSCAPE_WINDOW);
        assert_eq!(Layout::FullScreen.switched(), Layout::Window);
        assert_eq!(Layout::Window.switched(), Layout::FullScreen);
    }

    #[test]
    fn regions_follow_the_display() {
        let full = display(
            Layout::FullScreen,
            Some(WindowMode::Mirror),
            Shape::Landscape,
        );
        assert_eq!(full.region(TABLET), visible_region(TABLET));
        // The picture the iPhone renders may be of either shape; the region is the display's.
        assert_eq!(full.region(UPRIGHT), visible_region(TABLET));
        let mirror = Display {
            layout: Layout::Window,
            ..full
        };
        assert_eq!(
            mirror.region(TABLET),
            DisplayVisibleRegion {
                mode: LANDSCAPE_WINDOW,
                ..visible_region(TABLET)
            }
        );
        let simple = Display {
            window_mode: Some(WindowMode::Simple),
            ..mirror
        };
        assert_eq!(
            simple.region(TABLET),
            DisplayVisibleRegion {
                mode: PORTRAIT_WINDOW,
                origin_width: 1484,
                origin_height: 2458,
                left: 0,
                top: 0,
                right: 1484,
                bottom: 2458,
            }
        );
        assert_eq!(simple.picture(TABLET), UPRIGHT);
        assert_eq!(simple.to_string(), "simple launcher window, portrait");
        assert_eq!(mirror.to_string(), "mirror window, landscape");
        assert_eq!(full.to_string(), "full screen, landscape");
    }

    #[test]
    fn pictures_turn() {
        assert_eq!(TABLET.turned(Shape::Portrait), UPRIGHT);
        assert_eq!(TABLET.turned(Shape::Landscape), TABLET);
        assert_eq!(UPRIGHT.turned(Shape::Landscape), TABLET);
        let square = PictureSize {
            width: 1200,
            height: 1200,
        };
        assert_eq!(square.turned(Shape::Portrait), square);
        assert_eq!(square.shape(), Shape::Landscape);
    }
}
