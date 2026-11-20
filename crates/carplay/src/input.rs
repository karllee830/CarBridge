// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Touches and keys from the head unit, turned into CarPlay's HID reports.
//!
//! The head unit reports touches as Android motion events, in the pixels of the picture it shows, which is the
//! picture the iPhone renders. CarPlay takes two contacts in fixed slots, as fractions of the screen.

use carbridge_airplay::hid::Contact;

/// Contacts CarPlay's touch screen reports.
const SLOTS: usize = 2;

/// Android motion event actions; the pointer index of a further pointer goes in bits 8 and up.
mod action {
    pub const DOWN: i64 = 0;
    pub const UP: i64 = 1;
    pub const MOVE: i64 = 2;
    pub const CANCEL: i64 = 3;
    pub const POINTER_DOWN: i64 = 5;
    pub const POINTER_UP: i64 = 6;
}

/// Follows the fingers on the head unit's screen and gives CarPlay's contacts for each event.
pub struct TouchMapper {
    width: u32,
    height: u32,
    /// The pointer id in each slot, and its last position as fractions.
    slots: [Option<(i64, f64, f64)>; SLOTS],
}

impl TouchMapper {
    /// For a picture of `width` × `height` pixels.
    pub fn new(width: u32, height: u32) -> TouchMapper {
        TouchMapper {
            width: width.max(1),
            height: height.max(1),
            slots: [None; SLOTS],
        }
    }

    /// The picture's size, `width` × `height`.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Follows a picture of another size from the next event on; the fingers on the screen stay where they are.
    pub fn set_size(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    /// The contacts after one motion event: `action` as Android gives it, and every pointer on the screen as
    /// (id, x, y) in picture pixels, in pointer index order. Returns `None` for an action that changes nothing.
    pub fn event(&mut self, action: i64, pointers: &[(i64, i64, i64)]) -> Option<Vec<Contact>> {
        let (kind, index) = (action & 0xFF, usize::try_from(action >> 8).unwrap_or(0));
        if !matches!(
            kind,
            action::DOWN
                | action::UP
                | action::MOVE
                | action::CANCEL
                | action::POINTER_DOWN
                | action::POINTER_UP
        ) {
            return None;
        }
        if kind == action::DOWN || kind == action::CANCEL {
            self.slots = [None; SLOTS];
        }
        // Positions of the pointers still or newly on the screen.
        for &(id, x, y) in pointers {
            let position = (
                (x as f64 / f64::from(self.width)).clamp(0.0, 1.0),
                (y as f64 / f64::from(self.height)).clamp(0.0, 1.0),
            );
            let slot = match self
                .slots
                .iter()
                .position(|s| s.is_some_and(|(sid, _, _)| sid == id))
            {
                Some(slot) => Some(slot),
                None if kind != action::CANCEL => self.slots.iter().position(Option::is_none),
                None => None,
            };
            if let Some(slot) = slot {
                self.slots[slot] = Some((id, position.0, position.1));
            }
        }
        let released: Option<i64> = match kind {
            action::UP | action::CANCEL => None,
            action::POINTER_UP => pointers.get(index).map(|&(id, _, _)| id),
            _ => None,
        };
        let all_up = matches!(kind, action::UP | action::CANCEL);
        let contacts = (0..SLOTS)
            .map(|slot| match self.slots[slot] {
                Some((id, x, y)) => Contact {
                    slot: slot as u8,
                    x,
                    y,
                    down: !all_up && Some(id) != released,
                },
                None => Contact {
                    slot: slot as u8,
                    x: 0.0,
                    y: 0.0,
                    down: false,
                },
            })
            .collect();
        if all_up {
            self.slots = [None; SLOTS];
        } else if let Some(id) = released {
            for slot in &mut self.slots {
                if slot.is_some_and(|(sid, _, _)| sid == id) {
                    *slot = None;
                }
            }
        }
        Some(contacts)
    }
}

/// A key CarPlay takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Back,
    Home,
    Select,
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    /// Answer a call.
    Answer,
    /// End or reject a call.
    HangUp,
    /// Start the voice assistant.
    Siri,
}

impl Key {
    /// The key for an Android or Carlink key code, if CarPlay has one.
    pub fn from_keycode(keycode: i64) -> Option<Key> {
        Some(match keycode {
            4 => Key::Back,
            3 | 8103 => Key::Home,
            23 | 66 => Key::Select,
            126 => Key::Play,
            127 => Key::Pause,
            85 => Key::PlayPause,
            87 => Key::Next,
            88 => Key::Previous,
            5 => Key::Answer,
            6 => Key::HangUp,
            8107 => Key::Siri,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(contacts: &[Contact]) -> Vec<bool> {
        contacts.iter().map(|c| c.down).collect()
    }

    #[test]
    fn one_finger_tap() {
        let mut touch = TouchMapper::new(1280, 720);
        let pressed = touch.event(0, &[(7, 640, 360)]).unwrap();
        assert_eq!(down(&pressed), [true, false]);
        assert_eq!((pressed[0].x, pressed[0].y), (0.5, 0.5));
        let moved = touch.event(2, &[(7, 1280, 0)]).unwrap();
        assert_eq!((moved[0].x, moved[0].y), (1.0, 0.0));
        let released = touch.event(1, &[(7, 1280, 0)]).unwrap();
        assert_eq!(down(&released), [false, false]);
        assert_eq!(released[0].x, 1.0);
    }

    #[test]
    fn two_fingers_keep_their_slots() {
        let mut touch = TouchMapper::new(100, 100);
        touch.event(0, &[(3, 10, 10)]).unwrap();
        // Second pointer down at index 1.
        let both = touch
            .event(5 | 1 << 8, &[(3, 10, 10), (9, 90, 90)])
            .unwrap();
        assert_eq!(down(&both), [true, true]);
        assert_eq!(both[1].x, 0.9);
        // First pointer up at index 0: slot 0 goes up, slot 1 stays.
        let first_up = touch.event(6, &[(3, 10, 10), (9, 80, 80)]).unwrap();
        assert_eq!(down(&first_up), [false, true]);
        let moved = touch.event(2, &[(9, 70, 70)]).unwrap();
        assert_eq!(down(&moved), [false, true]);
        assert_eq!(moved[1].x, 0.7);
        let up = touch.event(1, &[(9, 70, 70)]).unwrap();
        assert_eq!(down(&up), [false, false]);
    }

    #[test]
    fn follows_the_picture_size() {
        let mut touch = TouchMapper::new(1280, 720);
        touch.set_size(2458, 1484);
        assert_eq!(touch.size(), (2458, 1484));
        let pressed = touch.event(0, &[(1, 1229, 742)]).unwrap();
        assert_eq!((pressed[0].x, pressed[0].y), (0.5, 0.5));
        touch.set_size(0, 0);
        assert_eq!(touch.size(), (1, 1));
    }

    #[test]
    fn cancel_and_unknown_actions() {
        let mut touch = TouchMapper::new(100, 100);
        touch.event(0, &[(1, 50, 50)]).unwrap();
        assert_eq!(
            down(&touch.event(3, &[(1, 50, 50)]).unwrap()),
            [false, false]
        );
        assert!(touch.event(8, &[]).is_none());
        let again = touch.event(0, &[(2, 0, 0)]).unwrap();
        assert_eq!(down(&again), [true, false]);
    }

    #[test]
    fn keys() {
        assert_eq!(Key::from_keycode(4), Some(Key::Back));
        assert_eq!(Key::from_keycode(8103), Some(Key::Home));
        assert_eq!(Key::from_keycode(87), Some(Key::Next));
        assert_eq!(Key::from_keycode(8107), Some(Key::Siri));
        assert_eq!(Key::from_keycode(164), None);
    }
}
