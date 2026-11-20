// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Command numbers, per category. On the audio category the command is the sound's
//! [`AudioType`](crate::pb::AudioType).

/// Category 1, the control channel.
pub mod control {
    pub const HEARTBEAT: u16 = 1;
    pub const GET_PORT_REQUEST: u16 = 2;
    pub const GET_PORT_RESPONSE: u16 = 3;
    pub const NOTIFY_CAR_TO_FOREGROUND: u16 = 4;
    pub const NOTIFY_CAR_TO_BACKGROUND: u16 = 5;
    pub const NOTIFY_MICROPHONE_STATE: u16 = 6;
    pub const NOTIFY_MIRROR_STATE: u16 = 7;
    pub const NOTIFY_AUDIO_PLAYER_STATE: u16 = 8;
    pub const NOTIFY_PHONE_STATE: u16 = 9;
    pub const NOTIFY_MUSIC_INFO: u16 = 10;
    pub const NOTIFY_NAVIGATION_INFO: u16 = 11;
    pub const CUSTOM_KEY_EVENT: u16 = 12;
    pub const VR_CMD_TO_PHONE: u16 = 13;
    pub const NOTIFY_CALL_HUNG_UP: u16 = 14;
    pub const NOTIFY_SWITCH_DAY_OR_NIGHT: u16 = 15;
    pub const AWAKEN_VOICE_ASSISTANT: u16 = 16;
    pub const AUDIO_PLAYER_CONTROL: u16 = 17;
    pub const BLUETOOTH_MAC_INFO: u16 = 22;
    pub const DISCONNECT: u16 = 23;
    pub const GET_UCAR_CONFIG_REQUEST: u16 = 24;
    pub const GET_UCAR_CONFIG_RESPONSE: u16 = 25;
    pub const GET_APP_LIST_REQUEST: u16 = 26;
    pub const GET_APP_LIST_RESPONSE: u16 = 27;
    pub const GET_APP_DETAIL_INFO_REQUEST: u16 = 28;
    pub const GET_APP_DETAIL_INFO_RESPONSE: u16 = 29;
    pub const INVOKE_APP: u16 = 30;
    pub const APP_STATE_CHANGED: u16 = 31;
    pub const APP_LIST_CHANGED: u16 = 32;
    pub const POI_ADDRESS: u16 = 33;
    pub const CALL_INFO: u16 = 34;
    pub const PICK_UP_CALL: u16 = 35;
    pub const ADD_NOTIFICATION: u16 = 36;
    pub const CLICK_NOTIFICATION: u16 = 37;
    pub const DELETE_NOTIFICATION: u16 = 38;
    pub const TOUCH_PAD_VIBRATOR: u16 = 39;
    pub const NOTIFY_CAR_VR_STATE: u16 = 40;
    pub const PAUSE_CAST: u16 = 41;
}

/// Category 2, the sensor channel; all from the head unit.
pub mod sensor {
    pub const GPS: u16 = 1;
    pub const LIGHTS: u16 = 2;
    pub const GYROSCOPE: u16 = 3;
    pub const ACCELERATION: u16 = 4;
    pub const OIL: u16 = 5;
    pub const GEAR_INFO: u16 = 6;
    pub const LIGHT_SENSOR_INFO: u16 = 7;
    pub const BATTERY: u16 = 8;
}

/// Category 3, the auth channel.
pub mod auth {
    pub const AUTH_REQUEST: u16 = 1;
    pub const AUTH_RESPONSE: u16 = 2;
    pub const AUTH_CONFIRM: u16 = 3;
}

/// Category 6, the cert channel.
pub mod cert {
    pub const CAR_CERTIFICATE: u16 = 1;
}

/// Category 7: acks have command 0.
pub mod ack {
    pub const ACK: u16 = 0;
}
