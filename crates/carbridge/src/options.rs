// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Command-line options.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use carbridge_carlink_session::display::{Layout, Shape};
use carbridge_carlink_wfd::Codec;

use crate::picture::{DensitySetting, RateSetting, Settings, SizeSetting};
use crate::sound::{parse_nav_audio, NavAudio};

pub const USAGE: &str = "\
usage: carbridge [options]

  --adapter NAME       Bluetooth adapter (default hci0)
  --iface NAME         Wi-Fi interface that joins the head unit's network (default carlink0)
  --state-dir DIR      where the phone's keys and the head units' keys are kept (default /etc/carbridge/carlink)
  --pin-file FILE      where the head unit's PIN is written for a first pairing (default /tmp/run/carbridge/pin)
  --name NAME          the name the head unit shows (default CarBridge)
  --clip FILE          H.264 Annex B clip cast while CarPlay shows nothing (default /usr/share/carbridge/waiting.h264)
  --hevc off|auto|force  H.265 to the head unit (default auto). off: cast H.264; auto: H.265 to head units that
                       offer it; force: have the iPhone send H.265 and the head unit decode it, even when the head unit
                       does not offer it. An iPhone keeps the codec it connected with until it connects again.
  --hevc-clip FILE     H.265 Annex B clip cast instead of --clip in casts in H.265
                       (default /usr/share/carbridge/waiting.h265)
  --portrait-clip FILE  H.264 clip cast instead of --clip while the head unit shows a portrait window
                       (default /usr/share/carbridge/waiting-portrait.h264); --clip when it is missing
  --portrait-hevc-clip FILE  the same in H.265 (default /usr/share/carbridge/waiting-portrait.h265)
  --calibration-dir DIR  where the clips of the calibration of a head unit's window are: calibrate.h264 asking for a
                       touch and calibrated.h264 once it came, with calibrate-portrait.h264 and calibrated-portrait.h264
                       for a portrait window, each also in H.265 (.h265) (default /usr/share/carbridge); the waiting
                       clip stands in for one that is missing
  --screen-dir DIR     where the clips of the screens that tell what CarPlay is doing are: unpaired, pairing,
                       connecting, unavailable-mfi, unavailable-bluetooth, unavailable-network and unavailable, each
                       .h264 and .h265, with -portrait ones for a portrait window (default /usr/share/carbridge); the
                       waiting clip stands in for one that is missing
  --lan-iface NAME     interface the iPhone reaches CarPlay on (default br-lan)
  --carplay-address IP address the CarPlay receiver listens on (default the first of --lan-iface)
  --carplay-port PORT  AirPlay control port (default 7000)
  --carplay-picture auto|fixed  the size of the picture the iPhone renders (default auto). auto: the area the head
                       unit shows the cast in, from its configuration, kept for the next start; fixed: --carplay-size.
                       An iPhone given another picture than the head unit's connects to CarPlay again.
  --carplay-size WxH   the fixed picture size, also used with auto until a head unit is known (default 1280x720)
  --carplay-fps auto|FPS  CarPlay's frame rate, 30 to 60 (default auto: the head unit's, 30 when it asks for 30 or
                       names none)
  --carplay-start fullscreen|window  how the cast shows when a head unit with window modes connects: full screen
                       (the default) or in the head unit's window, landscape in its mirror mode and portrait in its
                       simple mode; the head unit's window button switches between the two
  --nav-audio tts|media|call|assistant  the kind of sound the iPhone's navigation prompts play as on the head unit
                       (default tts, navigation speech); the head unit treats them as such: their volume, how other
                       sound is lowered, and audio focus
  --carplay-state-dir DIR  where the AirPlay identity and the paired iPhones are kept (default /etc/carbridge/airplay)
  --mfi-dir DIR        the MFi identity: identity.pk8 and certificate.p7b (default /etc/carbridge/mfi)
  --mfi-request-file FILE  a file whose appearance installs the MFi identity uploaded next to it, mfi-identity.pk8 and
                       mfi-certificate.p7b: install for a missing or unusable identity, replace for any
                       (default /tmp/run/carbridge/mfi-install)
  --oem-icon-dir DIR   head unit makers' icons for the CarPlay car tile, VENDOR.png (default /etc/carbridge/oem-icons)
  --ap-iface NAME      the access point the iPhone joins for CarPlay (default wlan0)
  --pairing-window SECS how long iPhones can pair after start, 0 for only on request (default 0)
  --no-carplay         cast the clip only
  --status-file FILE   where the state is written for the web interface (default /tmp/run/carbridge/status.json)
  --pair-file FILE     a file whose appearance opens pairing for iPhones (default /tmp/run/carbridge/pair)
  --forget-file FILE   a file naming head units to forget, one ID a line (default /tmp/run/carbridge/forget)
  --forget-iphone-file FILE
                       a file naming iPhones to unpair, one Bluetooth address a line
                       (default /tmp/run/carbridge/forget-iphone)
  --settings-file FILE a file of head units' own settings to apply, one JSON object a line
                       (default /tmp/run/carbridge/settings)
  --log-file FILE      where the log is also written, empty for nowhere (default /tmp/run/carbridge/carbridge.log)
  --detail-log-dir DIR where the detailed log is written while it is on, in files of 1 MiB, 10 MiB in all, the oldest
                       removed first (default /opt/carbridge/log)
  --detail-switch-file FILE  a file whose presence turns the detailed log on, looked at every few seconds
                       (default /tmp/run/carbridge/detail-log)
  --once               connect once and exit
  --check-mfi          tell whether the MFi identity in --mfi-dir is usable, and what its certificate says, and exit
  --install-mfi KEY CERTIFICATE
                       check the private key and the certificate and install them in --mfi-dir, then exit; an
                       identity there that is usable already is replaced only with --replace
  --version            print the version and exit
";

/// When the cast is in H.265.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hevc {
    /// H.264 only.
    Off,
    /// H.265 to head units that offer it in their answer to M3.
    #[default]
    Auto,
    /// H.265 to every head unit: the iPhone is offered H.265, and M4 tells the head unit to decode it whether or not
    /// it offers it.
    Force,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub adapter: String,
    pub iface: String,
    pub state_dir: PathBuf,
    pub pin_file: PathBuf,
    pub name: String,
    pub clip: PathBuf,
    pub hevc: Hevc,
    pub hevc_clip: PathBuf,
    pub portrait_clip: PathBuf,
    pub portrait_hevc_clip: PathBuf,
    pub calibration_dir: PathBuf,
    pub screen_dir: PathBuf,
    pub once: bool,
    pub carplay: bool,
    pub lan_iface: String,
    pub carplay_address: Option<IpAddr>,
    pub carplay_port: u16,
    pub carplay_picture: SizeSetting,
    pub carplay_size: (u32, u32),
    pub carplay_fps: RateSetting,
    pub carplay_start: Layout,
    pub nav_audio: NavAudio,
    pub carplay_state_dir: PathBuf,
    pub mfi_dir: PathBuf,
    pub mfi_request_file: PathBuf,
    pub oem_icon_dir: PathBuf,
    pub ap_iface: String,
    pub pairing_window: u32,
    pub status_file: PathBuf,
    pub pair_file: PathBuf,
    pub forget_file: PathBuf,
    pub forget_iphone_file: PathBuf,
    pub settings_file: PathBuf,
    pub log_file: Option<PathBuf>,
    pub detail_log_dir: PathBuf,
    pub detail_switch_file: PathBuf,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            adapter: "hci0".into(),
            iface: "carlink0".into(),
            state_dir: "/etc/carbridge/carlink".into(),
            pin_file: "/tmp/run/carbridge/pin".into(),
            name: "CarBridge".into(),
            clip: "/usr/share/carbridge/waiting.h264".into(),
            hevc: Hevc::Auto,
            hevc_clip: "/usr/share/carbridge/waiting.h265".into(),
            portrait_clip: "/usr/share/carbridge/waiting-portrait.h264".into(),
            portrait_hevc_clip: "/usr/share/carbridge/waiting-portrait.h265".into(),
            calibration_dir: "/usr/share/carbridge".into(),
            screen_dir: "/usr/share/carbridge".into(),
            once: false,
            carplay: true,
            lan_iface: "br-lan".into(),
            carplay_address: None,
            carplay_port: 7000,
            carplay_picture: SizeSetting::Auto,
            carplay_size: (1280, 720),
            carplay_fps: RateSetting::Auto,
            carplay_start: Layout::FullScreen,
            nav_audio: NavAudio::Tts,
            carplay_state_dir: "/etc/carbridge/airplay".into(),
            mfi_dir: "/etc/carbridge/mfi".into(),
            mfi_request_file: "/tmp/run/carbridge/mfi-install".into(),
            oem_icon_dir: "/etc/carbridge/oem-icons".into(),
            ap_iface: "wlan0".into(),
            pairing_window: 0,
            status_file: "/tmp/run/carbridge/status.json".into(),
            pair_file: "/tmp/run/carbridge/pair".into(),
            forget_file: "/tmp/run/carbridge/forget".into(),
            forget_iphone_file: "/tmp/run/carbridge/forget-iphone".into(),
            settings_file: "/tmp/run/carbridge/settings".into(),
            log_file: Some("/tmp/run/carbridge/carbridge.log".into()),
            detail_log_dir: "/opt/carbridge/log".into(),
            detail_switch_file: "/tmp/run/carbridge/detail-log".into(),
        }
    }
}

// Used by the connection, which runs on Linux only.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Options {
    /// The clip cast while CarPlay shows nothing, in `codec`, for a view of `shape` on the head unit.
    pub fn waiting_clip(&self, codec: Codec, shape: Shape) -> &Path {
        match (codec, shape) {
            (Codec::H264, Shape::Landscape) => &self.clip,
            (Codec::H265, Shape::Landscape) => &self.hevc_clip,
            (Codec::H264, Shape::Portrait) => &self.portrait_clip,
            (Codec::H265, Shape::Portrait) => &self.portrait_hevc_clip,
        }
    }

    /// The clip cast while a head unit's window is calibrated, in `codec`, for a view of `shape`: the one that asks for
    /// a touch, or with `done` the one that follows it.
    pub fn calibration_clip(&self, done: bool, codec: Codec, shape: Shape) -> PathBuf {
        let name = if done { "calibrated" } else { "calibrate" };
        let shape = match shape {
            Shape::Landscape => "",
            Shape::Portrait => "-portrait",
        };
        let extension = match codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        };
        self.calibration_dir
            .join(format!("{name}{shape}.{extension}"))
    }

    /// The clip of the screen `name` that tells what CarPlay is doing, in `codec`, for a view of `shape`.
    pub fn screen_clip(&self, name: &str, codec: Codec, shape: Shape) -> PathBuf {
        let shape = match shape {
            Shape::Landscape => "",
            Shape::Portrait => "-portrait",
        };
        let extension = match codec {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        };
        self.screen_dir.join(format!("{name}{shape}.{extension}"))
    }

    /// How CarPlay's picture is chosen for head units without settings of their own.
    pub fn picture(&self) -> Settings {
        Settings {
            size: self.carplay_picture,
            fixed_size: self.carplay_size,
            rate: self.carplay_fps,
            custom_size: None,
            density: DensitySetting::Real,
        }
    }
}

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Box<Options>),
    /// Tell of the MFi identity in the options' directory.
    CheckMfi(Box<Options>),
    /// Install the private key and the certificate at these paths in the options' directory; with `replace`, also in
    /// place of a usable identity.
    InstallMfi {
        options: Box<Options>,
        key: PathBuf,
        certificate: PathBuf,
        replace: bool,
    },
    Version,
    Help,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    let (mut check_mfi, mut install_mfi, mut replace) = (false, None, false);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--adapter" => options.adapter = value(&arg)?,
            "--iface" => options.iface = value(&arg)?,
            "--state-dir" => options.state_dir = value(&arg)?.into(),
            "--pin-file" => options.pin_file = value(&arg)?.into(),
            "--name" => options.name = value(&arg)?,
            "--clip" => options.clip = value(&arg)?.into(),
            "--hevc" => {
                options.hevc = parse_hevc(&value(&arg)?).ok_or("bad --hevc: off, auto or force")?
            }
            "--hevc-clip" => options.hevc_clip = value(&arg)?.into(),
            "--portrait-clip" => options.portrait_clip = value(&arg)?.into(),
            "--portrait-hevc-clip" => options.portrait_hevc_clip = value(&arg)?.into(),
            "--calibration-dir" => options.calibration_dir = value(&arg)?.into(),
            "--screen-dir" => options.screen_dir = value(&arg)?.into(),
            "--once" => options.once = true,
            "--no-carplay" => options.carplay = false,
            "--lan-iface" => options.lan_iface = value(&arg)?,
            "--carplay-address" => {
                options.carplay_address =
                    Some(value(&arg)?.parse().map_err(|_| "bad --carplay-address")?)
            }
            "--carplay-port" => {
                options.carplay_port = value(&arg)?.parse().map_err(|_| "bad --carplay-port")?
            }
            "--carplay-picture" => {
                options.carplay_picture = match value(&arg)?.as_str() {
                    "auto" => SizeSetting::Auto,
                    "fixed" => SizeSetting::Fixed,
                    _ => return Err("bad --carplay-picture: auto or fixed".into()),
                }
            }
            "--carplay-size" => {
                options.carplay_size = parse_size(&value(&arg)?).ok_or("bad --carplay-size")?
            }
            "--carplay-fps" => {
                options.carplay_fps =
                    parse_rate(&value(&arg)?).ok_or("bad --carplay-fps: auto or 30 to 60")?
            }
            "--carplay-start" => {
                options.carplay_start = parse_layout(&value(&arg)?)
                    .ok_or("bad --carplay-start: fullscreen or window")?
            }
            "--nav-audio" => {
                options.nav_audio = parse_nav_audio(&value(&arg)?)
                    .ok_or("bad --nav-audio: tts, media, call or assistant")?
            }
            "--carplay-state-dir" => options.carplay_state_dir = value(&arg)?.into(),
            "--mfi-dir" => options.mfi_dir = value(&arg)?.into(),
            "--mfi-request-file" => options.mfi_request_file = value(&arg)?.into(),
            "--check-mfi" => check_mfi = true,
            "--install-mfi" => {
                let key = PathBuf::from(value(&arg)?);
                let certificate = PathBuf::from(value(&arg)?);
                install_mfi = Some((key, certificate));
            }
            "--replace" => replace = true,
            "--oem-icon-dir" => options.oem_icon_dir = value(&arg)?.into(),
            "--ap-iface" => options.ap_iface = value(&arg)?,
            "--status-file" => options.status_file = value(&arg)?.into(),
            "--pair-file" => options.pair_file = value(&arg)?.into(),
            "--forget-file" => options.forget_file = value(&arg)?.into(),
            "--forget-iphone-file" => options.forget_iphone_file = value(&arg)?.into(),
            "--settings-file" => options.settings_file = value(&arg)?.into(),
            "--log-file" => {
                let path = value(&arg)?;
                options.log_file = (!path.is_empty()).then(|| path.into());
            }
            "--detail-log-dir" => options.detail_log_dir = value(&arg)?.into(),
            "--detail-switch-file" => options.detail_switch_file = value(&arg)?.into(),
            "--pairing-window" => {
                options.pairing_window = value(&arg)?.parse().map_err(|_| "bad --pairing-window")?
            }
            "--version" => return Ok(Command::Version),
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let options = Box::new(options);
    Ok(match (check_mfi, install_mfi) {
        (true, Some(_)) => return Err("--check-mfi and --install-mfi do not go together".into()),
        (true, None) => Command::CheckMfi(options),
        (false, Some((key, certificate))) => Command::InstallMfi {
            options,
            key,
            certificate,
            replace,
        },
        (false, None) if replace => return Err("--replace goes with --install-mfi".into()),
        (false, None) => Command::Run(options),
    })
}

/// A size as `WxH`, both sides above 0.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.split_once('x')?;
    let size = (w.parse().ok()?, h.parse().ok()?);
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// A frame rate: `auto`, or 30 to 60.
pub fn parse_rate(text: &str) -> Option<RateSetting> {
    if text == "auto" {
        return Some(RateSetting::Auto);
    }
    let fps: u32 = text.parse().ok()?;
    (30..=60).contains(&fps).then_some(RateSetting::Fixed(fps))
}

/// The name of a frame rate, as [`parse_rate`] takes it.
pub fn rate_name(rate: RateSetting) -> String {
    match rate {
        RateSetting::Auto => "auto".to_owned(),
        RateSetting::Fixed(fps) => fps.to_string(),
    }
}

/// When the cast is in H.265: `off`, `auto` or `force`.
pub fn parse_hevc(text: &str) -> Option<Hevc> {
    match text {
        "off" => Some(Hevc::Off),
        "auto" => Some(Hevc::Auto),
        "force" => Some(Hevc::Force),
        _ => None,
    }
}

/// The name of an H.265 mode, as [`parse_hevc`] takes it.
pub fn hevc_name(hevc: Hevc) -> &'static str {
    match hevc {
        Hevc::Off => "off",
        Hevc::Auto => "auto",
        Hevc::Force => "force",
    }
}

/// How CarPlay starts: `fullscreen` or `window`.
pub fn parse_layout(text: &str) -> Option<Layout> {
    match text {
        "fullscreen" => Some(Layout::FullScreen),
        "window" => Some(Layout::Window),
        _ => None,
    }
}

/// The name of a layout, as [`parse_layout`] takes it.
pub fn layout_name(layout: Layout) -> &'static str {
    match layout {
        Layout::FullScreen => "fullscreen",
        Layout::Window => "window",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(parse(args(&[])), Ok(Command::Run(Box::default())));
        let Ok(Command::Run(options)) =
            parse(args(&["--iface", "wlan1", "--once", "--name", "Car"]))
        else {
            panic!("run")
        };
        assert_eq!(
            (options.iface.as_str(), options.once, options.name.as_str()),
            ("wlan1", true, "Car")
        );
    }

    #[test]
    fn carplay_options() {
        let Ok(Command::Run(options)) = parse(args(&[
            "--carplay-size",
            "1920x720",
            "--carplay-address",
            "10.0.0.1",
            "--no-carplay",
            "--oem-icon-dir",
            "/tmp/icons",
        ])) else {
            panic!("run")
        };
        assert_eq!(options.carplay_size, (1920, 720));
        assert_eq!(options.oem_icon_dir, PathBuf::from("/tmp/icons"));
        assert_eq!(options.carplay_address, Some(IpAddr::from([10, 0, 0, 1])));
        assert!(!options.carplay);
        assert!(parse(args(&["--carplay-size", "0x720"])).is_err());
    }

    #[test]
    fn picture_options() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(
            options.picture(),
            Settings {
                size: SizeSetting::Auto,
                fixed_size: (1280, 720),
                rate: RateSetting::Auto,
                custom_size: None,
                density: DensitySetting::Real,
            }
        );
        let Ok(Command::Run(options)) = parse(args(&[
            "--carplay-picture",
            "fixed",
            "--carplay-size",
            "1920x1080",
            "--carplay-fps",
            "60",
        ])) else {
            panic!("run")
        };
        assert_eq!(
            options.picture(),
            Settings {
                size: SizeSetting::Fixed,
                fixed_size: (1920, 1080),
                rate: RateSetting::Fixed(60),
                custom_size: None,
                density: DensitySetting::Real,
            }
        );
        let Ok(Command::Run(options)) = parse(args(&["--carplay-fps", "auto"])) else {
            panic!("run")
        };
        assert_eq!(options.carplay_fps, RateSetting::Auto);
        for bad in [
            &["--carplay-picture", "native"][..],
            &["--carplay-fps", "25"],
            &["--carplay-fps", "61"],
            &["--carplay-fps", "x"],
            &["--carplay-fps"],
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hevc() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(options.hevc, Hevc::Auto);
        assert_eq!(
            options.waiting_clip(Codec::H264, Shape::Landscape),
            Path::new("/usr/share/carbridge/waiting.h264")
        );
        assert_eq!(
            options.waiting_clip(Codec::H265, Shape::Landscape),
            Path::new("/usr/share/carbridge/waiting.h265")
        );
        assert_eq!(
            options.waiting_clip(Codec::H264, Shape::Portrait),
            Path::new("/usr/share/carbridge/waiting-portrait.h264")
        );
        assert_eq!(
            options.waiting_clip(Codec::H265, Shape::Portrait),
            Path::new("/usr/share/carbridge/waiting-portrait.h265")
        );
        for (value, hevc) in [
            ("off", Hevc::Off),
            ("auto", Hevc::Auto),
            ("force", Hevc::Force),
        ] {
            let Ok(Command::Run(options)) = parse(args(&["--hevc", value])) else {
                panic!("run")
            };
            assert_eq!(options.hevc, hevc);
        }
        let Ok(Command::Run(options)) = parse(args(&["--hevc-clip", "/tmp/w.h265"])) else {
            panic!("run")
        };
        assert_eq!(
            options.waiting_clip(Codec::H265, Shape::Landscape),
            Path::new("/tmp/w.h265")
        );
        let Ok(Command::Run(options)) = parse(args(&[
            "--portrait-clip",
            "/tmp/p.h264",
            "--portrait-hevc-clip",
            "/tmp/p.h265",
        ])) else {
            panic!("run")
        };
        assert_eq!(
            options.waiting_clip(Codec::H264, Shape::Portrait),
            Path::new("/tmp/p.h264")
        );
        assert_eq!(
            options.waiting_clip(Codec::H265, Shape::Portrait),
            Path::new("/tmp/p.h265")
        );
        let Ok(Command::Run(options)) = parse(args(&["--hevc", "force", "--hevc", "off"])) else {
            panic!("run")
        };
        assert_eq!(options.hevc, Hevc::Off);
        assert!(parse(args(&["--hevc", "on"])).is_err());
        assert!(parse(args(&["--hevc"])).is_err());
    }

    #[test]
    fn calibration_clips() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        for (done, codec, shape, path) in [
            (
                false,
                Codec::H264,
                Shape::Landscape,
                "/usr/share/carbridge/calibrate.h264",
            ),
            (
                false,
                Codec::H265,
                Shape::Portrait,
                "/usr/share/carbridge/calibrate-portrait.h265",
            ),
            (
                true,
                Codec::H264,
                Shape::Portrait,
                "/usr/share/carbridge/calibrated-portrait.h264",
            ),
            (
                true,
                Codec::H265,
                Shape::Landscape,
                "/usr/share/carbridge/calibrated.h265",
            ),
        ] {
            assert_eq!(
                options.calibration_clip(done, codec, shape),
                PathBuf::from(path)
            );
        }
        let Ok(Command::Run(options)) = parse(args(&["--calibration-dir", "/tmp/c"])) else {
            panic!("run")
        };
        assert_eq!(
            options.calibration_clip(false, Codec::H264, Shape::Landscape),
            PathBuf::from("/tmp/c/calibrate.h264")
        );
        assert!(parse(args(&["--calibration-dir"])).is_err());
    }

    #[test]
    fn screen_clips() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(
            options.screen_clip("unpaired", Codec::H264, Shape::Landscape),
            PathBuf::from("/usr/share/carbridge/unpaired.h264")
        );
        let Ok(Command::Run(options)) = parse(args(&["--screen-dir", "/tmp/s"])) else {
            panic!("run")
        };
        assert_eq!(
            options.screen_clip("unavailable-mfi", Codec::H265, Shape::Portrait),
            PathBuf::from("/tmp/s/unavailable-mfi-portrait.h265")
        );
    }

    #[test]
    fn nav_audio() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(options.nav_audio, NavAudio::Tts);
        let Ok(Command::Run(options)) = parse(args(&["--nav-audio", "call"])) else {
            panic!("run")
        };
        assert_eq!(options.nav_audio, NavAudio::Call);
        assert!(parse(args(&["--nav-audio", "music"])).is_err());
    }

    #[test]
    fn mfi_commands() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(
            options.mfi_request_file,
            PathBuf::from("/tmp/run/carbridge/mfi-install")
        );
        let Ok(Command::CheckMfi(options)) = parse(args(&["--check-mfi", "--mfi-dir", "/tmp/m"]))
        else {
            panic!("check")
        };
        assert_eq!(options.mfi_dir, PathBuf::from("/tmp/m"));
        assert_eq!(
            parse(args(&["--install-mfi", "k.pk8", "c.p7b", "--replace"])),
            Ok(Command::InstallMfi {
                options: Box::default(),
                key: "k.pk8".into(),
                certificate: "c.p7b".into(),
                replace: true,
            })
        );
        assert!(parse(args(&["--install-mfi", "k.pk8"])).is_err());
        assert!(parse(args(&["--replace"])).is_err());
        assert!(parse(args(&["--check-mfi", "--install-mfi", "k", "c"])).is_err());
    }

    #[test]
    fn carplay_start() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(options.carplay_start, Layout::FullScreen);
        for (value, layout) in [
            ("fullscreen", Layout::FullScreen),
            ("window", Layout::Window),
        ] {
            let Ok(Command::Run(options)) = parse(args(&["--carplay-start", value])) else {
                panic!("run")
            };
            assert_eq!(options.carplay_start, layout);
        }
        assert!(parse(args(&["--carplay-start", "portrait"])).is_err());
        assert!(parse(args(&["--carplay-start"])).is_err());
    }

    #[test]
    fn names_parse_back() {
        for hevc in [Hevc::Off, Hevc::Auto, Hevc::Force] {
            assert_eq!(parse_hevc(hevc_name(hevc)), Some(hevc));
        }
        for layout in [Layout::FullScreen, Layout::Window] {
            assert_eq!(parse_layout(layout_name(layout)), Some(layout));
        }
        for rate in [
            RateSetting::Auto,
            RateSetting::Fixed(30),
            RateSetting::Fixed(60),
        ] {
            assert_eq!(parse_rate(&rate_name(rate)), Some(rate));
        }
        assert_eq!(parse_hevc("on"), None);
        assert_eq!(parse_layout("portrait"), None);
    }

    #[test]
    fn forget_iphone_file() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(
            options.forget_iphone_file,
            PathBuf::from("/tmp/run/carbridge/forget-iphone")
        );
        let Ok(Command::Run(options)) = parse(args(&["--forget-iphone-file", "/tmp/f"])) else {
            panic!("run")
        };
        assert_eq!(options.forget_iphone_file, PathBuf::from("/tmp/f"));
    }

    #[test]
    fn settings_file() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(
            options.settings_file,
            PathBuf::from("/tmp/run/carbridge/settings")
        );
        let Ok(Command::Run(options)) = parse(args(&["--settings-file", "/tmp/s"])) else {
            panic!("run")
        };
        assert_eq!(options.settings_file, PathBuf::from("/tmp/s"));
    }

    #[test]
    fn log_file() {
        let Ok(Command::Run(options)) = parse(args(&["--log-file", ""])) else {
            panic!("run")
        };
        assert_eq!(options.log_file, None);
        let Ok(Command::Run(options)) = parse(args(&["--log-file", "/tmp/cb.log"])) else {
            panic!("run")
        };
        assert_eq!(options.log_file, Some(PathBuf::from("/tmp/cb.log")));
    }

    #[test]
    fn detail_log() {
        let Ok(Command::Run(options)) = parse(args(&[])) else {
            panic!("run")
        };
        assert_eq!(options.detail_log_dir, PathBuf::from("/opt/carbridge/log"));
        assert_eq!(
            options.detail_switch_file,
            PathBuf::from("/tmp/run/carbridge/detail-log")
        );
        let Ok(Command::Run(options)) = parse(args(&[
            "--detail-log-dir",
            "/mnt/log",
            "--detail-switch-file",
            "/tmp/d",
        ])) else {
            panic!("run")
        };
        assert_eq!(options.detail_log_dir, PathBuf::from("/mnt/log"));
        assert_eq!(options.detail_switch_file, PathBuf::from("/tmp/d"));
        assert!(parse(args(&["--detail-log-dir"])).is_err());
    }

    #[test]
    fn errors() {
        assert!(parse(args(&["--iface"])).is_err());
        assert!(parse(args(&["--log-file"])).is_err());
        assert!(parse(args(&["--bogus"])).is_err());
        assert_eq!(parse(args(&["--version"])), Ok(Command::Version));
    }
}
