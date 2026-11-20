// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/AirPlayConfig.kt,
// airplay/AirPlayInfoPlist.kt and airplay/AirPlayDisplaySettings.kt (DiPlay commit 615b32c); main display only.

//! How the receiver describes itself: its configuration, and the /info plist the iPhone reads before it sets up
//! any stream. The plist must declare the display, the audio formats and latencies, the resource modes and the
//! HID devices, or the iPhone ends the session.

use crate::bplist::{Dict, Value};
use crate::hid;

/// The main display's UUID, which the HID devices and screen commands name.
pub const MAIN_DISPLAY: &str = "b7e6c5a0-1111-4000-8000-000000000001";

const MAIN_SCREEN: i64 = 110;
const DISPLAY_KNOBS: i64 = 0x02;
const DISPLAY_HIGH_FIDELITY_TOUCH: i64 = 0x08;
const FEATURES: u64 = 0x61_5653_aee2;
const AUDIO_FEATURES: u64 = 0x100_0454_0a00;

/// The frame rates a display may declare: 30 to 60 in steps of 5.
pub const MIN_FPS: u32 = 30;
pub const MAX_FPS: u32 = 60;
const FPS_STEP: u32 = 5;
/// The physical width declared when none is known, in millimetres.
pub const DEFAULT_WIDTH_MM: u32 = 200;
const MIN_REPORTED_MM: u32 = 1;
const MAX_REPORTED_MM: u32 = 2_000;

/// A frame rate snapped to the nearest declarable one.
pub fn sanitize_fps(fps: u32) -> u32 {
    let clamped = fps.clamp(MIN_FPS, MAX_FPS);
    (clamped - MIN_FPS + FPS_STEP / 2) / FPS_STEP * FPS_STEP + MIN_FPS
}

/// Margins of a display in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Insets {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

/// The display CarPlay draws on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub width: u32,
    pub height: u32,
    /// Its physical size in millimetres; the height follows the width's ratio when only the width is known, and
    /// [`DEFAULT_WIDTH_MM`] stands in for a width not known.
    pub width_mm: Option<u32>,
    pub height_mm: Option<u32>,
    pub fps: u32,
    /// The part CarPlay may draw in; the whole display when `None`.
    pub view_area: Option<Insets>,
    /// The part CarPlay keeps its controls in, within the view area.
    pub safe_area: Option<Insets>,
    /// Whether CarPlay may draw outside the safe area.
    pub draw_outside_safe_area: bool,
}

impl Display {
    pub fn new(width: u32, height: u32) -> Display {
        Display {
            width,
            height,
            width_mm: None,
            height_mm: None,
            fps: MAX_FPS,
            view_area: None,
            safe_area: None,
            draw_outside_safe_area: true,
        }
    }
}

/// An icon of the car's own, shown on CarPlay's home screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Icon {
    pub width: u32,
    pub height: u32,
    /// A PNG.
    pub png: Vec<u8>,
}

/// The receiver's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub name: String,
    /// The receiver's identifier, a MAC address in text.
    pub device_id: String,
    /// The Bluetooth adapter the iPhone paired with, in text.
    pub bluetooth_address: String,
    pub source_version: String,
    pub model: String,
    pub manufacturer: String,
    pub display: Display,
    pub right_hand_drive: bool,
    /// Whether H.265 is offered besides H.264.
    pub hevc: bool,
    /// Whether the receiver plays audio at all.
    pub audio_output: bool,
    /// The media sample rate, 48000 or 44100.
    pub media_sample_rate: u32,
    /// Whether the receiver has a microphone, and whether it encodes Opus for it.
    pub microphone: bool,
    pub microphone_opus: bool,
    pub icons: Vec<Icon>,
    /// The label under the car's icon.
    pub icon_label: String,
}

impl Config {
    pub fn new(
        name: &str,
        device_id: &str,
        bluetooth_address: &str,
        source_version: &str,
        display: Display,
    ) -> Config {
        Config {
            name: name.to_owned(),
            device_id: device_id.to_owned(),
            bluetooth_address: bluetooth_address.to_owned(),
            source_version: source_version.to_owned(),
            model: name.to_owned(),
            manufacturer: name.to_owned(),
            display,
            right_hand_drive: false,
            hevc: false,
            audio_output: true,
            media_sample_rate: 48_000,
            microphone: false,
            microphone_opus: true,
            icons: Vec::new(),
            icon_label: name.to_owned(),
        }
    }

    /// The feature bits Bonjour and /info both announce.
    pub fn features(&self) -> u64 {
        if self.audio_output {
            FEATURES
        } else {
            FEATURES & !AUDIO_FEATURES
        }
    }
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dict(entries.into_iter().collect())
}

/// The /info plist.
pub fn info(config: &Config) -> Value {
    let mut info = Dict::new()
        .with("sourceVersion", config.source_version.as_str())
        .with("features", config.features())
        .with("statusFlags", 4)
        .with("model", config.model.as_str())
        .with("manufacturer", config.manufacturer.as_str())
        .with("deviceID", config.device_id.as_str())
        .with(
            "bluetoothIDs",
            vec![Value::from(config.bluetooth_address.as_str())],
        )
        .with("name", config.name.as_str())
        .with("rightHandDrive", config.right_hand_drive)
        .with("keepAliveLowPower", false)
        .with("keepAliveSendStatsAsBody", false)
        .with("modes", modes());
    if config.audio_output {
        info.insert("audioLatencies", audio_latencies());
        info.insert("audioFormats", audio_formats(config));
    }
    info.insert(
        "extendedFeatures",
        vec![
            Value::from("vocoderInfo"),
            Value::from("enhancedRequestCarUI"),
        ],
    );
    info.insert("displays", vec![display_entry(&config.display)]);
    info.insert(
        "hidDevices",
        hid::devices(config.display.width, config.display.height, MAIN_DISPLAY),
    );
    if !config.icons.is_empty() {
        info.insert("oemIconVisible", true);
        info.insert("oemIconLabel", config.icon_label.as_str());
        let icons: Vec<Value> = config
            .icons
            .iter()
            .map(|icon| {
                dict(vec![
                    ("imageData", icon.png.clone().into()),
                    ("widthPixels", icon.width.into()),
                    ("heightPixels", icon.height.into()),
                    ("prerendered", true.into()),
                ])
            })
            .collect();
        info.insert("oemIcons", icons);
    }
    if config.hevc {
        info.insert("hevcInfo", Dict::new());
    }
    Value::Dict(info)
}

fn modes() -> Value {
    let resource = |id: i64| {
        dict(vec![
            ("resourceID", id.into()),
            ("transferType", 1.into()),
            ("transferPriority", 100.into()),
            ("takeConstraint", 100.into()),
            ("borrowConstraint", 100.into()),
            ("unborrowConstraint", 100.into()),
        ])
    };
    dict(vec![
        ("resources", vec![resource(1), resource(2)].into()),
        (
            "appStates",
            vec![
                dict(vec![("appStateID", 2.into()), ("state", false.into())]),
                dict(vec![("appStateID", 1.into()), ("speechMode", (-1).into())]),
                dict(vec![("appStateID", 3.into()), ("state", false.into())]),
            ]
            .into(),
        ),
    ])
}

fn audio_latencies() -> Value {
    let entry = |kind: i64, audio_type: Option<&str>| {
        let mut entry = Dict::new()
            .with("type", kind)
            .with("inputLatencyMicros", 0)
            .with("outputLatencyMicros", 0);
        if let Some(audio_type) = audio_type {
            entry.insert("audioType", audio_type);
        }
        Value::Dict(entry)
    };
    Value::Array(vec![
        entry(100, None),
        entry(100, Some("default")),
        entry(100, Some("media")),
        entry(100, Some("telephony")),
        entry(100, Some("speechRecognition")),
        entry(100, Some("alert")),
        entry(101, None),
        entry(101, Some("default")),
        entry(102, Some("default")),
    ])
}

/// Audio format bits.
pub mod format {
    /// PCM 8 to 32 kHz, mono and stereo.
    pub const PCM_VOICE: u64 = 0x3fc;
    pub const PCM_44K: u64 = 0xc00;
    pub const PCM_48K: u64 = 0xc000;
    /// PCM mono at 8, 16, 24 and 32 kHz.
    pub const PCM_VOICE_MONO: u64 = 0x154;
    pub const PCM_44K_MONO: u64 = 0x400;
    pub const PCM_48K_MONO: u64 = 0x4000;
    /// Opus at 16, 24 and 48 kHz.
    pub const OPUS: u64 = 0x7000_0000;
    pub const AAC_LC_44K: u64 = 0x40_0000;
    pub const AAC_LC_48K: u64 = 0x80_0000;
}

fn audio_formats(config: &Config) -> Value {
    use format::*;
    let is48 = config.media_sample_rate == 48_000;
    let pcm = PCM_VOICE | if is48 { PCM_48K } else { PCM_44K };
    let pcm_mono = PCM_VOICE_MONO | if is48 { PCM_48K_MONO } else { PCM_44K_MONO };
    let aac = if is48 { AAC_LC_48K } else { AAC_LC_44K };
    let pcm_input = config.microphone.then_some(pcm_mono);
    let wireless_input = config.microphone.then_some(if config.microphone_opus {
        pcm_mono | OPUS
    } else {
        pcm_mono
    });
    let entry = |kind: i64, audio_type: &str, output: u64, input: Option<u64>| {
        let mut entry = Dict::new()
            .with("type", kind)
            .with("audioType", audio_type)
            .with("audioOutputFormats", output);
        if let Some(input) = input {
            entry.insert("audioInputFormats", input);
        }
        Value::Dict(entry)
    };
    Value::Array(vec![
        entry(100, "compatibility", pcm, pcm_input),
        entry(101, "compatibility", pcm, None),
        entry(100, "default", pcm | OPUS, wireless_input),
        entry(100, "alert", pcm | OPUS, None),
        entry(100, "media", pcm, None),
        entry(100, "telephony", pcm_mono | OPUS, wireless_input),
        entry(100, "speechRecognition", pcm_mono | OPUS, wireless_input),
        entry(101, "default", pcm | OPUS, None),
        entry(102, "media", aac, None),
    ])
}

fn display_entry(display: &Display) -> Value {
    let reported = |mm: u32| mm.clamp(MIN_REPORTED_MM, MAX_REPORTED_MM);
    let width_mm = reported(display.width_mm.unwrap_or(DEFAULT_WIDTH_MM));
    let height_mm = reported(display.height_mm.unwrap_or_else(|| {
        (f64::from(width_mm) * f64::from(display.height) / f64::from(display.width.max(1))).round()
            as u32
    }));
    Value::Dict(
        Dict::new()
            .with("uuid", MAIN_DISPLAY)
            .with("type", MAIN_SCREEN)
            .with("maxFPS", sanitize_fps(display.fps))
            .with("widthPixels", display.width)
            .with("heightPixels", display.height)
            .with("widthPhysical", width_mm)
            .with("heightPhysical", height_mm)
            .with("features", DISPLAY_HIGH_FIDELITY_TOUCH | DISPLAY_KNOBS)
            .with("primaryInputDevice", 1)
            // Automatic appearance, until the receiver sets day or night.
            .with("uiAppearanceMode", 0)
            .with("uiAppearanceSetting", 0)
            .with("mapAppearanceMode", 0)
            .with("mapAppearanceSetting", 0)
            // SETUP enables view areas, so /info always describes one.
            .with("viewAreas", vec![view_area(display)])
            .with("initialViewArea", 0),
    )
}

fn view_area(display: &Display) -> Value {
    let (width, height) = (display.width, display.height);
    let area = |insets: Insets| {
        Dict::new()
            .with(
                "widthPixels",
                width.saturating_sub(insets.left + insets.right),
            )
            .with(
                "heightPixels",
                height.saturating_sub(insets.top + insets.bottom),
            )
            .with("originXPixels", insets.left)
            .with("originYPixels", insets.top)
    };
    let mut view = area(display.view_area.unwrap_or_default());
    let safe = area(display.safe_area.unwrap_or_default())
        .with("drawUIOutsideSafeArea", display.draw_outside_safe_area);
    view.insert("safeArea", safe);
    Value::Dict(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::new(
            "test",
            "02:00:00:00:00:02",
            "02:00:00:00:00:02",
            "366.0",
            Display::new(1280, 720),
        )
    }

    fn display(info: &Value) -> &Value {
        &info.get("displays").unwrap().as_array().unwrap()[0]
    }

    fn view(info: &Value) -> &Value {
        &display(info).get("viewAreas").unwrap().as_array().unwrap()[0]
    }

    fn int(value: &Value, key: &str) -> i64 {
        value.get(key).unwrap().as_i64().unwrap()
    }

    fn audio(info: &Value, kind: i64, audio_type: &str) -> Value {
        info.get("audioFormats")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|f| {
                int(f, "type") == kind && f.get("audioType").unwrap().as_str() == Some(audio_type)
            })
            .unwrap()
            .clone()
    }

    #[test]
    fn the_display_declares_automatic_appearance_and_full_areas() {
        let info = info(&config());
        let display = display(&info);
        for key in [
            "uiAppearanceMode",
            "uiAppearanceSetting",
            "mapAppearanceMode",
            "mapAppearanceSetting",
        ] {
            assert_eq!(int(display, key), 0, "{key}");
        }
        assert_eq!(int(display, "initialViewArea"), 0);
        assert_eq!(int(display, "primaryInputDevice"), 1);
        assert!(display.get("cornerMasks").is_none());
        let view = view(&info);
        assert_eq!(
            (int(view, "widthPixels"), int(view, "heightPixels")),
            (1280, 720)
        );
        assert_eq!(
            (int(view, "originXPixels"), int(view, "originYPixels")),
            (0, 0)
        );
        let safe = view.get("safeArea").unwrap();
        assert_eq!(
            (int(safe, "widthPixels"), int(safe, "heightPixels")),
            (1280, 720)
        );
        assert_eq!(
            safe.get("drawUIOutsideSafeArea").unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn hevc_is_offered_only_when_enabled() {
        assert!(info(&config()).get("hevcInfo").is_none());
        assert!(info(&Config {
            hevc: true,
            ..config()
        })
        .get("hevcInfo")
        .is_some());
    }

    #[test]
    fn driving_side_and_display_values() {
        let mut config = config();
        config.right_hand_drive = true;
        config.manufacturer = "Example".into();
        config.model = "HeadUnit".into();
        config.display.fps = 37;
        config.display.width_mm = Some(125);
        let info = info(&config);
        assert_eq!(info.get("rightHandDrive").unwrap().as_bool(), Some(true));
        assert_eq!(info.get("manufacturer").unwrap().as_str(), Some("Example"));
        assert_eq!(info.get("model").unwrap().as_str(), Some("HeadUnit"));
        let display = display(&info);
        assert_eq!(int(display, "maxFPS"), 35);
        assert_eq!(int(display, "widthPhysical"), 125);
        assert_eq!(int(display, "heightPhysical"), 70);
    }

    #[test]
    fn icons_keep_their_bytes() {
        let mut config = config();
        config.icons = vec![Icon {
            width: 1,
            height: 1,
            png: vec![1, 2, 3, 4],
        }];
        config.icon_label = "Car".into();
        let info = info(&config);
        assert_eq!(info.get("oemIconVisible").unwrap().as_bool(), Some(true));
        assert_eq!(info.get("oemIconLabel").unwrap().as_str(), Some("Car"));
        let icon = &info.get("oemIcons").unwrap().as_array().unwrap()[0];
        assert_eq!(
            icon.get("imageData").unwrap().as_data(),
            Some(&[1u8, 2, 3, 4][..])
        );
        assert_eq!(int(icon, "widthPixels"), 1);
    }

    #[test]
    fn safe_area_insets_reach_the_view_area() {
        let mut config = config();
        config.display = Display {
            safe_area: Some(Insets {
                top: 10,
                bottom: 15,
                left: 20,
                right: 25,
            }),
            draw_outside_safe_area: false,
            ..Display::new(960, 540)
        };
        let info = info(&config);
        let safe = view(&info).get("safeArea").unwrap().clone();
        assert_eq!(
            (int(&safe, "widthPixels"), int(&safe, "heightPixels")),
            (915, 515)
        );
        assert_eq!(
            (int(&safe, "originXPixels"), int(&safe, "originYPixels")),
            (20, 10)
        );
        assert_eq!(
            safe.get("drawUIOutsideSafeArea").unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn microphone_inputs_only_when_enabled() {
        let input = |info: &Value, kind: i64, audio_type: &str| {
            audio(info, kind, audio_type)
                .get("audioInputFormats")
                .and_then(Value::as_i64)
        };
        let without = info(&config());
        assert_eq!(input(&without, 100, "telephony"), None);
        let with = info(&Config {
            microphone: true,
            ..config()
        });
        assert_eq!(input(&with, 100, "default"), Some(0x7000_4154));
        assert_eq!(input(&with, 100, "telephony"), Some(0x7000_4154));
        assert_eq!(input(&with, 100, "compatibility"), Some(0x4154));
        let pcm_only = info(&Config {
            microphone: true,
            microphone_opus: false,
            ..config()
        });
        for audio_type in ["telephony", "default", "speechRecognition"] {
            assert_eq!(
                input(&pcm_only, 100, audio_type),
                Some(0x4154),
                "{audio_type}"
            );
        }
        assert_eq!(
            audio(&with, 100, "telephony").get("audioOutputFormats"),
            audio(&pcm_only, 100, "telephony").get("audioOutputFormats")
        );
    }

    #[test]
    fn audio_output_off_drops_formats_and_audio_features() {
        let silent = Config {
            audio_output: false,
            ..config()
        };
        let info = info(&silent);
        assert!(info.get("audioFormats").is_none());
        assert_eq!(silent.features(), FEATURES & !AUDIO_FEATURES);
        assert_eq!(config().features(), 0x61_5653_aee2);
    }

    #[test]
    fn fps_snaps_to_steps() {
        assert_eq!(sanitize_fps(10), 30);
        assert_eq!(sanitize_fps(37), 35);
        assert_eq!(sanitize_fps(38), 40);
        assert_eq!(sanitize_fps(90), 60);
    }

    #[test]
    fn the_plist_encodes_and_decodes() {
        let info = info(&Config {
            microphone: true,
            ..config()
        });
        let decoded = crate::bplist::decode(&crate::bplist::encode(&info)).unwrap();
        assert_eq!(decoded, info);
        assert_eq!(int(&decoded, "features"), 0x61_5653_aee2);
        assert_eq!(
            decoded.get("hidDevices").unwrap().as_array().unwrap().len(),
            4
        );
    }
}
