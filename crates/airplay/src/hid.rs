// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/AirPlayHid.kt (DiPlay
// commit 615b32c).

//! The HID devices the receiver declares in /info, and their input reports: a two-finger touch screen, a knob
//! with select, home and back, media keys and telephony keys. Reports go to the iPhone as `hidSendReport`
//! commands on the event channel.

use crate::bplist::{Dict, Value};

/// The devices' identifiers, as their `uuid` in hex.
pub const TOUCH: u32 = 0x2a2a_2a2a;
pub const KNOB: u32 = 0x2a2a_2a2b;
pub const MEDIA: u32 = 0x2a2a_2a2c;
pub const TELEPHONY: u32 = 0x2a2a_2a2d;

/// The fingers the touch screen reports.
pub const TOUCH_CONTACTS: usize = 2;
const BYTES_PER_CONTACT: usize = 6;

/// One finger on the touch screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    /// The report slot, below [`TOUCH_CONTACTS`]; a finger keeps its slot from press to release.
    pub slot: u8,
    /// The position as fractions of the screen's width and height.
    pub x: f64,
    pub y: f64,
    /// Whether the finger touches; a release reports the position it left at.
    pub down: bool,
}

/// The knob's buttons and movement for one report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Knob {
    pub select: bool,
    pub home: bool,
    pub back: bool,
    pub x: i8,
    pub y: i8,
    /// Turns of the knob; negative is counter-clockwise.
    pub wheel: i8,
}

/// Media key usages, by their index in the media device's report.
pub mod media {
    pub const PLAY: u8 = 1;
    pub const PAUSE: u8 = 2;
    pub const PLAY_PAUSE: u8 = 3;
    pub const NEXT: u8 = 4;
    pub const PREVIOUS: u8 = 5;
}

/// Telephony key usages, by their index in the telephony device's report.
pub mod telephony {
    pub const HOOK_SWITCH: u8 = 1;
    pub const FLASH: u8 = 2;
    pub const DROP: u8 = 3;
}

/// The touch report for a screen of `width` × `height` pixels: each slot holds its contact, or is up.
pub fn touch_report(contacts: &[Contact], width: u32, height: u32) -> Vec<u8> {
    let mut report = vec![0; BYTES_PER_CONTACT * TOUCH_CONTACTS];
    for slot in 0..TOUCH_CONTACTS {
        let offset = slot * BYTES_PER_CONTACT;
        report[offset] = slot as u8;
        let Some(contact) = contacts.iter().find(|c| usize::from(c.slot) == slot) else {
            continue;
        };
        report[offset + 1] = u8::from(contact.down);
        let scale = |fraction: f64, pixels: u32| {
            (fraction * f64::from(pixels)).round().clamp(0.0, 65_535.0) as u16
        };
        report[offset + 2..offset + 4].copy_from_slice(&scale(contact.x, width).to_le_bytes());
        report[offset + 4..offset + 6].copy_from_slice(&scale(contact.y, height).to_le_bytes());
    }
    report
}

pub fn knob_report(knob: &Knob) -> Vec<u8> {
    let buttons = u8::from(knob.select) | u8::from(knob.home) << 1 | u8::from(knob.back) << 2;
    let axis = |value: i8| value.max(-127) as u8;
    vec![buttons, axis(knob.x), axis(knob.y), axis(knob.wheel)]
}

/// A one-byte report naming a key by its index; 0 releases it.
pub fn key_report(index: u8) -> Vec<u8> {
    vec![index]
}

fn device(uid: u32, name: &str, descriptor: Vec<u8>, display: &str) -> Value {
    Value::Dict(
        Dict::new()
            .with("hidProductID", 1)
            .with("hidVendorID", 2)
            .with("hidCountryCode", 0)
            .with("uuid", format!("{uid:x}"))
            .with("name", name)
            .with("displayUUID", display)
            .with("hidDescriptor", descriptor),
    )
}

/// The four devices, on the display `display`, for a touch screen of `width` × `height` pixels.
pub fn devices(width: u32, height: u32, display: &str) -> Vec<Value> {
    vec![
        device(
            TOUCH,
            "Touchscreen",
            touch_descriptor(width, height),
            display,
        ),
        device(KNOB, "Knob", KNOB_DESCRIPTOR.to_vec(), display),
        device(MEDIA, "Media", MEDIA_DESCRIPTOR.to_vec(), display),
        device(
            TELEPHONY,
            "Telephony",
            TELEPHONY_DESCRIPTOR.to_vec(),
            display,
        ),
    ]
}

fn touch_descriptor(width: u32, height: u32) -> Vec<u8> {
    let [x0, x1] = (width.min(0xffff) as u16).to_le_bytes();
    let [y0, y1] = (height.min(0xffff) as u16).to_le_bytes();
    let mut bytes = vec![0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01];
    for _ in 0..TOUCH_CONTACTS {
        bytes.extend_from_slice(&[
            0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, // a finger
            0x09, 0x38, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, // its index
            0x15, 0x00, 0x25, 0x01, 0x09, 0x33, 0x75, 0x01, 0x95, 0x01, 0x81,
            0x02, // touching
            0x95, 0x07, 0x81, 0x03, // padding
            0x05, 0x01, 0x26, x0, x1, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, // x
            0x26, y0, y1, 0x09, 0x31, 0x81, 0x02, 0xc0, // y
        ]);
    }
    bytes.push(0xc0);
    bytes
}

const KNOB_DESCRIPTOR: &[u8] = &[
    0x05, 0x01, 0x09, 0x08, 0xa1, 0x01, //
    0x05, 0x09, 0x09, 0x01, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81,
    0x02, // select
    0x05, 0x0c, 0x0a, 0x23, 0x02, 0x0a, 0x24, 0x02, 0x95, 0x02, 0x81, 0x02, // home, back
    0x95, 0x05, 0x81, 0x01, // padding
    0x05, 0x01, 0x09, 0x01, 0xa1, 0x00, //
    0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
    0xc0, // x, y
    0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0, // wheel
];

const MEDIA_DESCRIPTOR: &[u8] = &[
    0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, //
    0x15, 0x00, 0x25, 0x06, 0x05, 0x0c, //
    0x0a, 0x00, 0x00, 0x0a, 0xb0, 0x00, 0x0a, 0xb1, 0x00, 0x0a, 0xcd, 0x00, //
    0x0a, 0xb5, 0x00, 0x0a, 0xb6, 0x00, 0x0a, 0x9e, 0x02, //
    0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
];

const TELEPHONY_DESCRIPTOR: &[u8] = &[
    0x05, 0x0b, 0x09, 0x07, 0xa1, 0x01, //
    0x15, 0x00, 0x25, 0x11, 0x05, 0x0b, //
    0x09, 0x00, 0x09, 0x20, 0x09, 0x21, 0x09, 0x26, 0x09, 0x2f, //
    0x09, 0xb0, 0x09, 0xb1, 0x09, 0xb2, 0x09, 0xb3, 0x09, 0xb4, 0x09, 0xb5, //
    0x09, 0xb6, 0x09, 0xb7, 0x09, 0xb8, 0x09, 0xb9, 0x09, 0xba, 0x09, 0xbb, //
    0x05, 0x07, 0x09, 0x2a, //
    0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_reports_put_each_contact_in_its_slot() {
        let report = touch_report(
            &[Contact {
                slot: 1,
                x: 0.5,
                y: 0.25,
                down: true,
            }],
            1280,
            720,
        );
        assert_eq!(report, [0, 0, 0, 0, 0, 0, 1, 1, 0x80, 0x02, 0xb4, 0x00]);
        let released = touch_report(
            &[Contact {
                slot: 0,
                x: -0.1,
                y: 1.0,
                down: false,
            }],
            1280,
            720,
        );
        assert_eq!(&released[..6], [0, 0, 0, 0, 0xd0, 0x02]);
    }

    #[test]
    fn knob_reports() {
        assert_eq!(knob_report(&Knob::default()), [0, 0, 0, 0]);
        let knob = Knob {
            select: true,
            back: true,
            wheel: -128,
            x: 3,
            ..Knob::default()
        };
        assert_eq!(knob_report(&knob), [0x05, 3, 0, 0x81]);
        assert_eq!(
            knob_report(&Knob {
                home: true,
                ..Knob::default()
            })[0],
            0x02
        );
    }

    #[test]
    fn devices_name_the_display_and_carry_the_screen_size() {
        let devices = devices(1280, 720, "main");
        assert_eq!(devices.len(), 4);
        assert_eq!(devices[0].get("uuid").unwrap().as_str(), Some("2a2a2a2a"));
        assert_eq!(devices[3].get("uuid").unwrap().as_str(), Some("2a2a2a2d"));
        assert_eq!(
            devices[1].get("displayUUID").unwrap().as_str(),
            Some("main")
        );
        let touch = devices[0].get("hidDescriptor").unwrap().as_data().unwrap();
        // Logical maximum of x, then of y, in each finger's collection.
        let finger = &touch[6..6 + 51];
        assert_eq!(&finger[32..35], [0x26, 0x00, 0x05]);
        assert_eq!(&finger[43..46], [0x26, 0xd0, 0x02]);
        assert_eq!(touch.len(), 6 + 2 * 51 + 1);
    }
}
