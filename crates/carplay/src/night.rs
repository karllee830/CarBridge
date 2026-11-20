// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The head unit's day or night mode, kept for the iPhone's CarPlay sessions: each session is told the mode once it
//! can be, and again whenever the head unit changes it.

/// The head unit's mode and what the session of the moment has been told of it.
#[derive(Debug, Default)]
pub struct NightMode {
    /// The head unit's mode, night when true; `None` with no head unit connected or before it gave one.
    head_unit: Option<bool>,
    /// The session of the moment, by its number, and whether it has been told the head unit's mode of the moment.
    session: Option<(u64, bool)>,
}

impl NightMode {
    /// Takes the head unit's mode, or `None` once the head unit is gone. A mode is told to the session of the moment
    /// even when it has been told the same before.
    pub fn set_head_unit(&mut self, night: Option<bool>) {
        self.head_unit = night;
        if let Some((_, told)) = &mut self.session {
            *told = false;
        }
    }

    /// The head unit's mode, night when true.
    pub fn head_unit(&self) -> Option<bool> {
        self.head_unit
    }

    /// Session `id` is up and is to be told the head unit's mode.
    pub fn session_started(&mut self, id: u64) {
        self.session = Some((id, false));
    }

    /// Session `id` ended; one that started since stays.
    pub fn session_ended(&mut self, id: u64) {
        if self.session.is_some_and(|(current, _)| current == id) {
            self.session = None;
        }
    }

    /// The session to tell the head unit's mode and the mode, while there is one to tell.
    pub fn due(&self) -> Option<(u64, bool)> {
        match self.session {
            Some((id, false)) => self.head_unit.map(|night| (id, night)),
            _ => None,
        }
    }

    /// Session `id` took `night`. A mode that changed meanwhile is still due.
    pub fn told(&mut self, id: u64, night: bool) {
        if let Some((current, told)) = &mut self.session {
            if *current == id && self.head_unit == Some(night) {
                *told = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_from_before_the_iphone_is_told_once_its_session_is_up() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(true));
        assert_eq!(mode.due(), None);
        mode.session_started(1);
        assert_eq!(mode.due(), Some((1, true)));
        mode.told(1, true);
        assert_eq!(mode.due(), None);
    }

    #[test]
    fn untold_mode_stays_due_until_taken() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(false));
        mode.session_started(1);
        assert_eq!(mode.due(), Some((1, false)));
        assert_eq!(mode.due(), Some((1, false)));
        mode.told(1, false);
        assert_eq!(mode.due(), None);
    }

    #[test]
    fn change_while_connected_is_told_at_once() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(false));
        mode.session_started(1);
        mode.told(1, false);
        mode.set_head_unit(Some(true));
        assert_eq!(mode.due(), Some((1, true)));
        mode.told(1, true);
        mode.set_head_unit(Some(true));
        assert_eq!(mode.due(), Some((1, true)));
    }

    #[test]
    fn change_during_a_send_is_still_due() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(false));
        mode.session_started(1);
        mode.set_head_unit(Some(true));
        mode.told(1, false);
        assert_eq!(mode.due(), Some((1, true)));
    }

    #[test]
    fn new_session_is_told_the_kept_mode() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(true));
        mode.session_started(1);
        mode.told(1, true);
        mode.session_ended(1);
        assert_eq!(mode.due(), None);
        mode.session_started(2);
        assert_eq!(mode.due(), Some((2, true)));
    }

    #[test]
    fn end_of_an_older_session_leaves_the_new_one() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(true));
        mode.session_started(1);
        mode.session_started(2);
        mode.session_ended(1);
        assert_eq!(mode.due(), Some((2, true)));
        mode.told(1, true);
        assert_eq!(mode.due(), Some((2, true)));
    }

    #[test]
    fn end_of_the_head_unit_clears_the_mode() {
        let mut mode = NightMode::default();
        mode.set_head_unit(Some(true));
        mode.session_started(1);
        mode.set_head_unit(None);
        assert_eq!(mode.head_unit(), None);
        assert_eq!(mode.due(), None);
        mode.session_ended(1);
        mode.session_started(2);
        assert_eq!(mode.due(), None);
        // The next head unit's mode is told afresh.
        mode.set_head_unit(Some(false));
        assert_eq!(mode.due(), Some((2, false)));
    }

    #[test]
    fn nothing_is_told_without_a_head_unit() {
        let mut mode = NightMode::default();
        mode.session_started(1);
        assert_eq!(mode.due(), None);
        mode.session_ended(1);
        mode.set_head_unit(Some(true));
        mode.set_head_unit(None);
        mode.session_started(2);
        assert_eq!(mode.due(), None);
    }
}
