<h1 align="center">CarBridge</h1>

<p align="center"><b>English</b> · <a href="docs/README.zh-CN.md">简体中文</a></p>

<p align="center">
  Wireless CarPlay for cars whose head unit speaks Carlink.<br>
  An OpenWrt router in the car receives CarPlay from your iPhone and casts it to the head unit.
</p>

```text
iPhone ── wireless CarPlay (Wi-Fi + Bluetooth) ──▶ OpenWrt router ── Carlink (Bluetooth LE + Wi-Fi) ──▶ head unit
```

## Features

- **Wireless both ways**: the iPhone joins the router's Wi-Fi and Bluetooth as it would a CarPlay car; the router
  finds the head unit over Bluetooth LE and casts to it over a second Wi-Fi radio. No cable to the head unit.
- **Fits the screen**: CarPlay is drawn at the size of the area the head unit shows it in, pixel for pixel once the
  head unit's touches have measured it, with controls sized for the screen. Full screen and the head unit's
  landscape and portrait windows all work.
- **H.264 and H.265**, at the frame rate the head unit asks for, up to 60 frames a second.
- **Sound and microphone**: music, navigation, calls and Siri, each as its own kind of sound on the head unit; the
  car's microphone for calls and Siri. Navigation prompts can play as media, call or assistant sound instead.
- **Now playing**: title, artist and cover on the head unit, whose play, pause, next and previous buttons control
  the iPhone. The head unit's keys work as CarPlay keys.
- **Set up from a web page**: pair head units and iPhones, upload the MFi certificate, set each head unit's picture,
  codec and sound, read and download the log. In English and Chinese.
- **Connects by itself**: once paired, CarBridge connects whenever the car and the iPhone are there, and tells on the
  head unit's screen what it waits for.

CarBridge is developed and tested with Leapmotor head units, whose Carlink app is built on the UCar SDK (ICCOA
CarLink).

## What you need

- **A router running OpenWrt** (current release, with LuCI), with:
  - a Wi-Fi access point for the iPhone, 5 GHz recommended;
  - a **second Wi-Fi adapter** for the head unit, with its driver and firmware, for example a USB adapter with an
    RTL8811CU or RTL8821CU chip;
  - a Bluetooth adapter that BlueZ supports, with both Bluetooth LE (for the head unit) and Classic (for the
    iPhone).
- **An MFi identity of your own**: the private key `identity.pk8` and the certificate `certificate.p7b` that let an
  accessory speak CarPlay. CarBridge does not include one.
- **An iPhone** with wireless CarPlay, and **a head unit** with wireless Carlink.

## Install

1. Download the packages from the [Releases](../../releases) page:
   - `carbridge-<version>-r1-<architecture>-<release>.apk`, for your router's package architecture and OpenWrt
     release (`OPENWRT_ARCH` and `VERSION_ID` in the router's `/etc/os-release`);
   - `luci-app-carbridge-<version>-r1.apk`, the web page, the same for every router.
   - for the web page in Chinese, also `luci-i18n-carbridge-zh-cn-<version>.apk`.

   Releases have packages for OpenWrt 25.12 on 64-bit ARM (including Raspberry Pi 3, 4 and 5) and x86-64; the release
   notes list them. For another architecture or release, build the packages yourself, as in
   [Building the packages](#building-the-packages).

2. Copy them to the router and install them, which also installs BlueZ, D-Bus, `iw` and wpa_supplicant:

   ```bash
   apk add --allow-untrusted ./carbridge-*.apk ./luci-app-carbridge-*.apk
   ```

## Getting started

Open the router's web page and go to **Services → CarBridge**.

1. **Head unit Wi-Fi adapter**: under **Settings**, choose the second Wi-Fi adapter and save. CarBridge leaves it
   out of the router's own Wi-Fi and names its interface `carlink0`, also when it is plugged in again later.
2. **MFi certificate**: choose the button on the CarPlay card and upload `identity.pk8` and `certificate.p7b`.
   CarBridge checks that they belong together and installs them readable by root only; CarPlay starts right away.
3. **Pair the head unit**: on the head unit, open Carlink; it shows a six-digit PIN. Choose **Pair a head unit**,
   enter the PIN and choose Connect. Keep the head unit on its PIN screen until it is paired.
4. **Pair the iPhone**: choose **Pair an iPhone**. On the iPhone, open **Settings → Bluetooth**, tap **CarBridge**
   under Other Devices and pair, then allow CarPlay.

From then on CarBridge connects to the head unit and to the iPhone by itself whenever both are there. The settings
under the head unit's **Advanced** button and under **Advanced settings** have an ⓘ that explains each one.

## Troubleshooting

- **The CarPlay card says CarPlay cannot run**: it names the reason: no usable MFi identity, Bluetooth not ready,
  or no IPv4 address on the iPhone's network (`lan_iface`). CarBridge tries again every five seconds.
- **The head unit does not connect again**: it looks for CarBridge only while its Carlink app waits for a phone.
  Open Carlink on the head unit, or choose its reconnect button.
- **Something else**: the log is at the bottom of the web page, and `logread -e carbridge` shows it on the router.
  Turn on **Detailed log** for diagnostics of the sound, the microphone and the link to the head unit, kept in
  `/opt/carbridge/log`, and attach it to an issue.

How CarBridge sizes the picture, connects and logs is described in detail in the [reference](docs/reference.md).

## Building from source

### Building the packages

Build them with the OpenWrt SDK of your router's release and target, with this repository as a feed:

```bash
echo "src-link carbridge $PWD/openwrt" >> /path/to/sdk/feeds.conf
cd /path/to/sdk
./scripts/feeds update -a && ./scripts/feeds install carbridge luci-app-carbridge
make package/carbridge/compile package/luci-app-carbridge/compile
```

Rust comes from the SDK's packages feed; the packages land in `bin/packages/<architecture>/carbridge/`. Pushing a
tag `vX.Y.Z` that matches the version in `Cargo.toml` has GitHub Actions build them and publish a release.

### Building and testing the program


```bash
cargo build --release
cargo test --workspace
```

| Path | Contents |
| --- | --- |
| `crates/carbridge` | the `carbridge` program |
| `crates/carlink-proto` | Carlink wire format: messages, encrypted records, authentication |
| `crates/carlink-ble` | finding the head unit and the Bluetooth LE handoff |
| `crates/carlink-wifi` | joining the network the head unit opens |
| `crates/carlink-session` | the control, sensor, certificate and media channels |
| `crates/carlink-wfd` | Wi-Fi Display source: RTSP, MPEG-TS over RTP, input back channel |
| `crates/carplay` | the CarPlay receiver, on top of `crates/airplay` and `crates/iap2` |
| `crates/airplay` | AirPlay for CarPlay: pairing, the control and event channels, screen, audio, microphone; GPL-3.0-only |
| `crates/iap2` | iAP2 for wireless CarPlay: link layer, identification, MFi authentication, Wi-Fi setup; GPL-3.0-only |
| `crates/platform-openwrt` | BlueZ, wpa_supplicant, uci and ubus |
| `openwrt` | the OpenWrt packages `carbridge` and `luci-app-carbridge` |
| `tools/screens` | the screens shown on the head unit, and the script that renders them |

## Contributing

Issues and pull requests are welcome. Your first pull request requires agreeing to the
[Contributor Agreement](https://gist.github.com/karllee830/d0cc16330da0b0ec9d9751cb898f6192): tick the checkbox in the
pull request description; you only need to agree once.

## License

Copyright (C) 2026 Karl Lee ([karllee830](https://github.com/karllee830)) and CarBridge contributors

CarBridge is free software licensed under the [GNU AGPL-3.0](LICENSE) (AGPL-3.0-only). The crates `crates/airplay`
and `crates/iap2` are a Rust rewrite, by CarBridge contributors, of the CarPlay code of
[DiPlay](https://github.com/shihabal3amri/DiPlay), which builds on [xcertplay](https://github.com/shilapi/xcertplay).
Like those, they are licensed under the [GNU GPL-3.0](crates/airplay/LICENSE) (GPL-3.0-only), and each of their files
names DiPlay's, xcertplay's and CarBridge's contributors as copyright holders and says which upstream files it was
rewritten from. Section 13 of each license allows the two to be combined into one program. If you change CarBridge
and let others use it over a network, for example through its web page, offer them the source code of your version.

CarPlay and iPhone are trademarks of Apple Inc. This project is not affiliated with or endorsed by Apple, ICCOA or
any car maker.
