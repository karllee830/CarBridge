# CarBridge reference

How CarBridge behaves in detail: what the web interface shows, how the picture is sized, how it connects and
what it logs. For installing and first steps, see the [README](../README.md).

## Contents

- [Web interface](#web-interface)
- [MFi identity](#mfi-identity)
- [Settings of each head unit](#settings-of-each-head-unit)
- [Picture size, frame rate and density](#picture-size-frame-rate-and-density)
- [H.265](#h265)
- [Window modes](#window-modes)
- [Calibration](#calibration)
- [Status screens](#status-screens)
- [Navigation prompts](#navigation-prompts)
- [The car tile](#the-car-tile)
- [Music info](#music-info)
- [Connecting to the head unit](#connecting-to-the-head-unit)
- [Start-up and recovery](#start-up-and-recovery)
- [Detailed log](#detailed-log)

## Web interface

The web interface is under Services → CarBridge, laid out for phones as well as larger screens. At the top it shows
whether CarBridge is connected to the head unit, and to which, and whether CarPlay runs and an iPhone is connected,
with CarBridge's version next to the title. While CarPlay cannot run, its card says why: no usable MFi identity,
Bluetooth not ready, no IPv4 address on the iPhone's network interface (`lan_iface`), or a receiver that did not
start; its MFi certificate button, marked while the identity is missing or unusable, opens the dialog in [MFi
identity](#mfi-identity). Below are two buttons, one to pair a head unit and one to pair an iPhone, and the head units
CarBridge paired with, each by the name it advertises or its certificate gives, else its ID, with when it was paired,
whether it is the one connected now or last, and the settings it has of its own. Pairing a head unit asks for the PIN
its Carlink screen shows and connects with it, telling how that goes: paired, the PIN refused (the head unit shows a
new one from time to time), or a failed connection, which is tried again; leaving before it is paired withdraws the
PIN. A PIN given pairs the next head unit without a key of its own, rather than the key paired with last being tried
on it. A head unit known by no key of its own is tried with the key paired with last; once it takes that key it goes
by it from then on (`head-units/<id>.alias`), whatever head unit pairs next, and one that refuses it leaves every key
as it is and pairs with its PIN. The first button is marked while a head unit waits for its PIN. Pairing an iPhone
says how to pair it from its Bluetooth settings, lets iPhones pair and counts down the time left, and names the
iPhones that pair meanwhile; pairing that cannot be opened is tried once more after a second, and then the dialog says
why, as it says when CarPlay cannot run. Then come the iPhones paired over Bluetooth, each of which can be unpaired,
and the settings an owner sets once: whether CarBridge runs, the name the iPhone and the head unit show, and the
Wi-Fi adapter that joins the head unit's network. The other settings, and the log, which can also be downloaded, stay
hidden until asked for: whether CarPlay runs at all, how long iPhones can pair after a start, the interfaces and the
Bluetooth adapter, and the defaults for head units. All of them are in `/etc/config/carbridge`. Each setting is
explained by the ⓘ after it, shown under the pointer or with a tap. In every choice the option that is the default is
marked [Default] (【默认】).

The Wi-Fi adapter for the head unit is chosen from the router's adapters, other than the iPhone's access point, and
kept by its MAC address (`carlink_mac`). When it is there, before CarBridge starts and whenever it is plugged in,
`/usr/libexec/carbridge-carlink` takes it out of OpenWrt's Wi-Fi and names its interface `carlink0`, which the
firewall rule in `/usr/share/carbridge/carlink-input.nft` lets the head unit's connections in on. netifd sets up every
radio of the type `mac80211`, a disabled one too, and deletes the interfaces of one it sets up without any; so the
adapter's sections in `/etc/config/wireless` get the type `carbridge`, which netifd leaves alone, with the type they
had kept in `carbridge_type`, and an adapter without a section gets one, `carbridge_carlink`, which also keeps
OpenWrt's detection from adding a radio for it. Without a chosen adapter, `carlink_iface` names the interface, as it
is.

## MFi identity

The packages need BlueZ, D-Bus and wpa_supplicant from the official feeds. The MFi identity for CarPlay is the
owner's: `identity.pk8` (a P-256 private key in PKCS #8) and `certificate.p7b` (PKCS #7 holding its one certificate)
in `/etc/carbridge/mfi` (`--mfi-dir`). The web interface's MFi certificate dialog shows whether both are there and
usable, and what the certificate names (subject, issuer, serial number, validity); the private key is never shown.
When the identity is missing or cannot be used, the dialog uploads the two files, in DER, PEM or base64, next to the
request file `/tmp/run/carbridge/mfi-install` (`--mfi-request-file`; `mfi-identity.pk8` and `mfi-certificate.p7b`).
CarBridge checks that both can be read and that the key belongs to the certificate, installs them readable by root
only (the directory `0700`, the files `0600`), removes what was uploaded, and starts CarPlay without a restart.
Replacing a usable identity needs confirming in the dialog and applies once CarBridge starts again. On the command
line, `carbridge --check-mfi` tells the same of the identity in `--mfi-dir`, and `carbridge --install-mfi KEY
CERTIFICATE` checks and installs a pair, `--replace` to replace a usable one.

## Settings of each head unit

Each head unit can have settings of its own, under Advanced in the list of head units: its full screen resolution
(automatic, the area the head unit shows the cast in, or a width × height of its own: even, from 320×240 to
3840×2160), its frame rate (automatic, 30 or 60), H.265 (`off`, `auto` or `force`, as `hevc` below), how CarPlay
starts on it (full screen or in the window), the kind of sound the iPhone's navigation prompts play as (see below),
and the density of its screen (DPI) that CarPlay's density in each mode is worked out from, as below: RealDPI, the
real density the head unit reports for its screen, by default; DPI, the one it gives phones to lay out with; or one
from 72 to 640 dots per inch. Each choice names what it stands for, a density with the screen's diagonal it gives,
from the screen size the head unit reports. The option marked [Default] takes the option of `/etc/config/carbridge`
(`carplay_picture` and `carplay_size`, `carplay_fps`, `hevc`, `carplay_start`, `nav_audio`), which are thus the
defaults for head units without settings of their own, and keeps following it when it changes; any other option is
kept as the head unit's own. The same dialog lists the views measured of the head unit, as below, with the density
each gives, and its Recalibrate button clears them, for the window modes to be calibrated again the next time CarPlay
shows in them. CarBridge keeps a head unit's settings with its key, in `head-units/<id>.settings`, and forgets them
with it. The web interface asks for a change by writing a JSON line to `/tmp/run/carbridge/settings`
(`--settings-file`): the head unit's `id`, as the list names it, and the settings that change, `full_screen` (`auto`
or `WxH`), `fps` (`auto`, `30` to `60`), `hevc`, `start` (`fullscreen` or `window`), `dpi` (`configured` or `72` to
`640`), `nav_audio` (`tts`, `media`, `call` or `assistant`), each also `default`, and `views` with `default` to clear
the views; for example `{"id":"0123456789ab","full_screen":"1920x1080","dpi":"220"}`. A change to the head unit
connected now applies at once: a new picture or frame rate as below, the iPhone connecting to CarPlay again for it; an
H.265 setting that casts in the other codec ends the connection to the head unit, which connects again and is cast to
in it, and the iPhone connects again when it was offered other codecs. How CarPlay starts applies from the head unit's
next connection. While no head unit is connected, a change to the head unit connected last applies to the iPhones that
connect from then on, as CarPlay follows that head unit until another connects; other head units take their settings
when they connect.

## Picture size, frame rate and density

CarPlay's picture follows the head unit (`carplay_picture` and `carplay_fps` set to `auto`, the defaults). Its size is
that of the area the head unit shows the cast in, which its configuration names, rather than its whole screen, unless
the head unit has a full screen resolution of its own, which comes first. The head unit shows a full screen cast on a
view a little smaller than that area, fitted by the area's shape whatever the picture, and its touches measure that
view (see [Calibration](#calibration)); once they have, the picture is the view's size from the next connection on,
pixel for pixel on the screen, the iPhone connected meanwhile keeping its picture. Until then the picture is the area,
shown on the view in the same shape, a little scaled down; the picture's density gives its physical size, which
CarPlay sizes its controls by. It starts from the density of the head unit's screen: the real density its
configuration reports (`realDpi`), when that is from 72 to 640, else the density it gives the phone to lay out with
(`dpi`), unless the head unit's DPI setting chooses the latter first or a number of its own. In each mode the head
unit shows the picture on a view of its own size, full screen as in its windows, and names that size, in its screen's
pixels, in every touch; CarBridge keeps it as the view of the mode the touch came in, for the picture shown there,
with the head unit's key in `head-units/<id>.views`. The picture's density in a mode is then the screen's density
scaled by the picture's size over the view's, `density × √(picture width × height ÷ (view width × height))`, the
picture's width over the view's and its height over the view's taken together, so that CarPlay's controls are as large
on the view as on a screen of that density. The view is kept for pictures of the same shape, which the head unit fits
into the same area; for a picture of another shape it is measured again. Until a mode's view is measured, the screen's
density applies as it is. A higher density gives larger controls. The log names where the density of each picture
comes from. Its frame rate is the one the head unit asks for, from 30 to 60, and 30 when the head unit asks for 30 or
names none. Sizes are kept within 320×240 and 3840×2160 and made even. Set to fixed, CarPlay is drawn at
`carplay_size` (1280×720 by default), which is also the size until a head unit is known, and runs at 30 or 60 frames a
second. Such head units name a small H.264 level in their video formats but decode a larger picture, so the cast
claims the 1280×720 mode at 30 frames a second for a picture of up to 30 and at 60 otherwise, with an H.264 level that
holds the picture; the waiting screen stays at 30. CarBridge keeps the head unit's picture with its key and at startup
uses that of the head unit connected last, until that head unit is forgotten. Each iPhone renders the picture of the
moment when it connects to CarPlay; when a head unit asks for another picture than the connected iPhone renders,
CarBridge ends the iPhone's session and the iPhone connects again with it. Touches follow the picture the iPhone
renders.

## H.265

H.265 (`hevc`) is cast to head units that offer it by default (`auto`); others are cast H.264. `off` casts H.264 to
every head unit, and `force` has the iPhone send H.265 and the head unit decode it, even when the head unit does not
offer it, for head units whose decoder takes H.265 without saying so. An iPhone keeps the codecs it was offered until
it connects to CarPlay again.

## Window modes

Head units with window modes show the cast full screen, as their screen is, or in a window; the head unit's window
button switches between the two, and the mode menu of its window between its mirror mode, a landscape window, and its
simple mode, a portrait one. CarBridge offers both window modes, takes the one the head unit picks when it connects,
and follows the one whose app the menu starts. It reports the display mode with each app state, the head unit free to
switch. `carplay_start` sets whether CarPlay starts full screen (`fullscreen`, the default) or in the window
(`window`). Full screen and the landscape window show the same picture, so switching between them leaves CarPlay as
it is, unless their densities differ: the density is fixed for a CarPlay session, so a switch to a mode with another
density has the iPhone connect to CarPlay again with it. The landscape window is the mirror mode's, the portrait
window the simple mode's.

## Calibration

The full screen view is learned from ordinary touches, which go on to the iPhone; a new one applies when the iPhone
next connects. A window whose view is not known yet is calibrated once, the first time the head unit shows CarPlay
in it: CarPlay is not shown there, a screen asks for one touch anywhere, and that touch, which does not go to the
iPhone, gives the view. A screen then says the calibration is done while the iPhone connects to CarPlay again with
the window's shape and density, at most once, and CarPlay shows in the window from its first picture. The
calibration screen stays until the touch comes; when the head unit switches to another mode before, CarBridge
follows it, calibrating that one if it needs it. The calibration screens are clips like the waiting screen, in
landscape and portrait (`--calibration-dir`, `/usr/share/carbridge` by default); the waiting screen stands in for a
missing one. For the portrait window CarPlay is drawn for the head unit's screen turned upright: the iPhone connects to
CarPlay again to draw it, which takes a few seconds, and meanwhile the head unit is sent nothing, neither the old
picture, which it would show squeezed, nor the waiting screen, for at most ten seconds; touches wait as well. A full
screen turned from the screen's shape is not offered, as head units stretch it over the whole screen. The picture
in the window derives from the full screen one, the head unit's own resolution included. The waiting
screen has a portrait layout for the portrait window. CarBridge keeps the window mode with the head unit's key, so
that a CarPlay started in the window has the right shape before the head unit connects.

## Status screens

While CarPlay shows nothing, the head unit is shown a screen of what CarPlay is doing: how to pair an iPhone while
none is paired (open the CarBridge page, Pair an iPhone, then the iPhone's Bluetooth settings), how to pair one while
pairing is open, the waiting screen while a paired iPhone is awaited, a connecting screen while an iPhone is
connected and CarPlay starts, and why CarPlay cannot run. Each is a clip in landscape and portrait, H.264 and H.265,
in `--screen-dir` (`/usr/share/carbridge`); the waiting screen stands in for a missing one. Their source is
`tools/screens/screens.html`, which `tools/screens/render.sh` renders into `openwrt/carbridge/files` with headless
Chrome and ffmpeg.

## Navigation prompts

The iPhone's navigation prompts play on the head unit as navigation speech by default (`nav_audio` `tts`,
`--nav-audio`); a head unit can play them as media, as a phone call or as the AI assistant instead, and then treats
them as that kind of sound: their volume, how other sound is lowered meanwhile, and audio focus. Media is the head
unit's media type for sound of a player of its own, next to the cast's music, which a second stream in the music's
player would garble. A change applies from the next prompt, without the iPhone connecting again. When navigation
plays in the player of a call or of Siri that plays at the same time, it is converted to that player's format and
mixed into its sound, and goes on in that format once the other ends; the head unit is told of a call or Siri by
their own sound only, not by navigation played as one.

## The car tile

The CarPlay home screen's car tile is named after the head unit, from its Carlink certificate: its short name, else
its product name, else "车辆" (also before any head unit has connected). Its icon is CarBridge's car icon, except for
Leapmotor head units (vendor ID `0028`), which get `/etc/carbridge/oem-icons/0028.png` when the owner puts that file
in place; CarBridge does not ship it. The file must be a square, opaque PNG (no alpha channel or transparent colours)
of 64 to 512 pixels, 192 recommended, and at most 256 KiB, without rounded corners: the iPhone rounds them. Otherwise
CarBridge's icon is used and the log says why. An iPhone gets the tile when it connects. CarBridge keeps what the
certificate says with the head unit's key, and at startup names the tile after the head unit connected last, until
that head unit is forgotten. When a head unit names a tile that looks different from the one the connected iPhone
shows, CarBridge ends the iPhone's CarPlay session, and the iPhone connects again with the new tile. A certificate
that cannot be read leaves the tile as it is.

## Music info

What plays on the iPhone goes to the head unit as music info (`NotifyMusicInfo`): title, artist, album, duration,
position, whether it plays, and the song's cover, which the iPhone sends over iAP2. Head units with the UCar SDK show
it in the capsule their cast window shrinks to, whose play, pause, next and previous buttons reach the iPhone as
media keys; they show music there only with a cover, so a song without artwork, or with artwork over 1 MiB, gets a
grey music note. The cover goes once per song and when it changes. A head unit that connects while music plays is
told at once, and told that nothing plays once the iPhone's CarPlay session ends.

## Connecting to the head unit

CarBridge asks a head unit for a Wi-Fi Direct group first, and for its access point when it refuses that, does not
answer for 15 seconds, or opens a network that cannot be joined; the next connection asks for the other kind. The
kind that worked is kept with the head unit's key (`head-units/<id>.network`) and asked for first next time. It also
asks for a 5 GHz channel in the other half of the band from the router's access point (`ap_iface`), outside the DFS
channels: 36 for an access point at 100 and above, 149 for one at 36 to 64, none when the access point's channel is
not known; the head unit may choose another, and the log tells what was asked and what it chose. Before connecting
again, CarBridge waits up to five seconds for the kernel to drop the last LE link to the head unit; the detailed log
notes the ATT exchange by opcode, handle and length.

## Start-up and recovery

CarPlay starts as soon as it can: without an IPv4 address on `lan_iface`, a usable MFi identity or Bluetooth,
CarBridge tries again every five seconds until it runs, logging the first failure and the recovery. Keys from the
head unit are logged with the CarPlay key each became, in the detailed log while it is on, else at most once every
ten seconds. A watchdog thread ends CarBridge when its main loop has not run for a minute, and procd starts it again;
the service runs it at a higher priority (`nice -10`).

## Detailed log

The detailed log (`detail_log`, off by default) also writes the log to persistent storage, in `detail_log_dir`
(`/opt/carbridge/log`, `--detail-log-dir`), with diagnostics the other logs leave out: once a second, the car's
microphone while it records (what the head unit sends, the longest gap between its messages, its level and how long
it was silent, what went on to the iPhone, and the formats the iPhone and the head unit were asked for) and the
sound dropped on its way to the head unit, by audio type; every five seconds, the cast's writes that waited for room,
the TCP state of the cast and of the media channel, and the Wi-Fi link to the head unit. It is kept in ten files of
1 MiB, `carbridge.log` the newest and `carbridge.log.1` to `.9` the older ones, the oldest removed as a new one
starts, and written in chunks every few seconds. Turning it on or off applies without a restart: the service creates
or removes `/tmp/run/carbridge/detail-log` (`--detail-switch-file`), which CarBridge looks for every two seconds. The
log section's second button downloads the detailed log's files joined, the oldest first. Next to the log's button,
Restart CarPlay restarts the CarBridge service after asking: CarPlay and the picture on the head unit drop for a few
seconds.
