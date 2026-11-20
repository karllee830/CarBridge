<h1 align="center">CarBridge</h1>

<p align="center"><a href="../README.md">English</a> · <b>简体中文</b></p>

<p align="center">
  让车机支持 Carlink 的汽车用上无线 CarPlay。<br>
  在车里放一台 OpenWrt 路由器，接收 iPhone 的 CarPlay，再投屏到车机上。
</p>

```text
iPhone ── 无线 CarPlay（Wi-Fi + 蓝牙）──▶ OpenWrt 路由器 ── Carlink（低功耗蓝牙 + Wi-Fi）──▶ 车机
```

## 功能

- **两头都是无线**：iPhone 像连 CarPlay 汽车一样连路由器的 Wi-Fi 和蓝牙；路由器通过低功耗蓝牙找到车机，再用第二块 Wi-Fi
  网卡投屏过去，不用给车机接线。
- **画面贴合屏幕**：CarPlay 按车机显示投屏的区域大小绘制，车机的触摸测出实际区域后做到像素一一对应，按钮大小也按屏幕调整。全屏、
  车机的横屏窗口和竖屏窗口都能用。
- **支持 H.264 和 H.265**，帧率跟随车机，最高 60 帧。
- **声音和麦克风**：音乐、导航、通话和 Siri 在车机上各按自己的声音类型播放；通话和 Siri 用车上的麦克风。导航播报也可以改成按
  媒体、通话或语音助手的声音播放。
- **正在播放**：车机上显示歌名、歌手和封面，车机的播放、暂停、上一首、下一首按钮能控制 iPhone。车机按键可以当 CarPlay 按键用。
- **网页里完成设置**：配对车机和 iPhone、上传 MFi 证书、给每台车机单独设置画面、编码和声音、查看和下载日志。界面有中英文。
- **自动连接**：配对一次以后，只要车机和 iPhone 都在，CarBridge 就会自己连上；等待期间在车机屏幕上提示当前在等什么。

CarBridge 在零跑的车机上开发和测试。零跑车机的 Carlink 应用基于 UCar SDK（ICCOA CarLink）。

## 需要准备

- **一台装了 OpenWrt 的路由器**（较新的版本，带 LuCI），并且有：
  - 一个给 iPhone 连的 Wi-Fi 热点，建议用 5 GHz；
  - **第二块 Wi-Fi 网卡**，专门用来连车机，并装好它的驱动和固件，比如一块 RTL8811CU 或 RTL8821CU 芯片的 USB 网卡；
  - 一个 BlueZ 支持的蓝牙适配器，同时支持低功耗蓝牙（连车机）和经典蓝牙（连 iPhone）。
- **你自己的 MFi 身份**：让配件能使用 CarPlay 的私钥 `identity.pk8` 和证书 `certificate.p7b`。CarBridge 不附带。
- **一部支持无线 CarPlay 的 iPhone**，和**一台支持无线 Carlink 的车机**。

## 安装

1. 从 [Releases](../../../releases) 页面下载安装包：
   - `carbridge-<版本>-r1-<架构>-<OpenWrt 版本>.apk`，要和路由器的软件包架构、OpenWrt 版本对应（见路由器 `/etc/os-release` 里的
     `OPENWRT_ARCH` 和 `VERSION_ID`）；
   - `luci-app-carbridge-<版本>-r1.apk`，网页管理界面，所有路由器通用。
   - `luci-i18n-carbridge-zh-cn-<版本>.apk`，网页的中文翻译，所有路由器通用。

   Release 提供 OpenWrt 25.12 下 64 位 ARM（包括树莓派 3、4、5）和 x86-64 架构的安装包，具体列在发布说明里。
   其他架构或 OpenWrt 版本，请按[编译安装包](#编译安装包)自己编译。

2. 把两个包拷到路由器上安装，BlueZ、D-Bus、`iw` 和 wpa_supplicant 会一起装上：

   ```bash
   apk add --allow-untrusted ./carbridge-*.apk ./luci-app-carbridge-*.apk ./luci-i18n-carbridge-zh-cn-*.apk
   ```

## 开始使用

打开路由器的网页管理界面，进入 **服务 → CarBridge**。

1. **连接车机的 Wi-Fi 网卡**：在 **设置** 里选择第二块 Wi-Fi 网卡并保存。CarBridge 会让路由器自己的 Wi-Fi 不再使用它，并把它的
   接口命名为 `carlink0`，以后重新插上也一样。
2. **MFi 证书**：点 CarPlay 卡片上的 **MFi 证书** 按钮，上传 `identity.pk8` 和 `certificate.p7b`。CarBridge 会检查两者是否配套，
   安装成只有 root 能读，CarPlay 随即启动。
3. **配对车机**：在车机上打开 Carlink，屏幕上会显示 6 位验证码。点 **配对车机**，输入验证码，点“开始连接”。配对完成前，请让车机
   停留在显示验证码的界面。
4. **配对 iPhone**：点 **配对 iPhone**。在 iPhone 上打开 **设置 → 蓝牙**，在“其他设备”中轻点 **CarBridge** 并配对，然后允许使用
   CarPlay。

之后只要车机和 iPhone 都在，CarBridge 就会自己连上。车机的 **高级设置** 按钮和页面下方 **高级设置** 里的每一项，后面的 ⓘ
都有说明。

## 常见问题

- **CarPlay 卡片显示 CarPlay 无法运行**：卡片上会写原因：没有可用的 MFi 身份、蓝牙尚未就绪，或 iPhone 所在网络（`lan_iface`）
  没有 IPv4 地址。CarBridge 每 5 秒会自动重试。
- **车机断开后没有再连上**：车机只有在 Carlink 应用等待手机时才会找 CarBridge。在车机上打开 Carlink，或点它的重新连接按钮。
- **其他问题**：网页最下方有日志，在路由器上用 `logread -e carbridge` 也能看。打开 **详细日志** 可以记录声音、麦克风和车机连接的
  诊断信息，保存在 `/opt/carbridge/log`，提 issue 时请附上。

CarBridge 如何确定画面大小、如何连接车机、记录哪些日志，详见[参考文档](reference.md)（英文）。

## 从源码编译

### 编译安装包

用对应路由器版本和平台的 OpenWrt SDK 编译，把这个仓库加为软件源：

```bash
echo "src-link carbridge $PWD/openwrt" >> /path/to/sdk/feeds.conf
cd /path/to/sdk
./scripts/feeds update -a && ./scripts/feeds install carbridge luci-app-carbridge
make package/carbridge/compile package/luci-app-carbridge/compile
```

Rust 工具链来自 SDK 的 packages 软件源，编出的包在 `bin/packages/<架构>/carbridge/`。推送和 `Cargo.toml` 版本号一致的
`vX.Y.Z` 标签后，GitHub Actions 会自动编译并发布 Release。

### 编译和测试程序


```bash
cargo build --release
cargo test --workspace
```

| 路径 | 内容 |
| --- | --- |
| `crates/carbridge` | `carbridge` 主程序 |
| `crates/carlink-proto` | Carlink 协议格式：消息、加密记录、认证 |
| `crates/carlink-ble` | 发现车机，以及低功耗蓝牙握手 |
| `crates/carlink-wifi` | 加入车机开出的网络 |
| `crates/carlink-session` | 控制、传感器、证书和媒体通道 |
| `crates/carlink-wfd` | Wi-Fi Display 发送端：RTSP、基于 RTP 的 MPEG-TS、输入回传通道 |
| `crates/carplay` | CarPlay 接收端，基于 `crates/airplay` 和 `crates/iap2` |
| `crates/airplay` | CarPlay 用的 AirPlay：配对、控制和事件通道、画面、音频、麦克风；GPL-3.0-only |
| `crates/iap2` | 无线 CarPlay 用的 iAP2：链路层、识别、MFi 认证、Wi-Fi 设置；GPL-3.0-only |
| `crates/platform-openwrt` | BlueZ、wpa_supplicant、uci 和 ubus |
| `openwrt` | OpenWrt 软件包 `carbridge` 和 `luci-app-carbridge` |
| `tools/screens` | 车机上显示的状态画面，以及渲染它们的脚本 |

## 参与贡献

欢迎提交 issue 和拉取请求。第一次提交拉取请求前，需要同意[贡献者协议](https://gist.github.com/karllee830/d0cc16330da0b0ec9d9751cb898f6192)：
在拉取请求的描述里勾选即可，只需要同意一次。

## 许可证

Copyright (C) 2026 Karl Lee（[karllee830](https://github.com/karllee830)）及 CarBridge 贡献者

CarBridge 是自由软件，采用 [GNU AGPL-3.0](../LICENSE)（AGPL-3.0-only）授权。`crates/airplay` 和 `crates/iap2` 是
CarBridge 贡献者用 Rust 对 [DiPlay](https://github.com/shihabal3amri/DiPlay) 中 CarPlay 代码的重写，DiPlay 又基于
[xcertplay](https://github.com/shilapi/xcertplay)。和它们一样，这两个库采用 [GNU GPL-3.0](../crates/airplay/LICENSE)
（GPL-3.0-only）授权，每个文件都注明 DiPlay、xcertplay 和 CarBridge 的贡献者为版权人，并写明改写自上游的哪些文件。
两个许可证的第 13 条都允许把它们组合成一个程序。如果你修改了 CarBridge，并让别人通过网络使用它（比如通过它的网页），
需要向他们提供你修改后的源代码。

CarPlay 和 iPhone 是 Apple Inc. 的商标。本项目与 Apple、ICCOA 及任何汽车厂商均无关联，也未获得其认可。
