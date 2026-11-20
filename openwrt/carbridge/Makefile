# SPDX-FileCopyrightText: 2026 CarBridge contributors
# SPDX-License-Identifier: AGPL-3.0-only
#
# OpenWrt package of CarBridge, built from this checkout: add the openwrt directory as a feed of the SDK or the build
# tree (src-link carbridge /path/to/CarBridge/openwrt in feeds.conf), then
#   ./scripts/feeds update carbridge && ./scripts/feeds install carbridge luci-app-carbridge
#   make package/carbridge/compile
# Rust comes from the packages feed (lang/rust).

include $(TOPDIR)/rules.mk

PKG_NAME:=carbridge
# The version is CarBridge's, from the workspace's Cargo.toml.
PKG_VERSION:=$(shell sed -n 's/^version = "\(.*\)"/\1/p' $(CURDIR)/../../Cargo.toml)
PKG_RELEASE:=1

PKG_LICENSE:=AGPL-3.0-only GPL-3.0-only
PKG_LICENSE_FILES:=LICENSE crates/airplay/LICENSE crates/iap2/LICENSE

PKG_BUILD_DEPENDS:=rust/host
PKG_BUILD_PARALLEL:=1

include $(INCLUDE_DIR)/package.mk
include $(TOPDIR)/feeds/packages/lang/rust/rust-package.mk

SOURCE_TREE:=$(CURDIR)/../..

define Package/carbridge
  SECTION:=net
  CATEGORY:=Network
  TITLE:=iPhone CarPlay to Carlink head units
  DEPENDS:=$(RUST_ARCH_DEPENDS) +bluez-daemon +dbus +iw +wpad-basic-mbedtls
endef

define Package/carbridge/description
  Receives wireless CarPlay from an iPhone on the router's access point and casts it to a head unit that speaks
  Carlink (ICCOA CarLink), found over Bluetooth LE and joined over Wi-Fi with a second radio.
endef

define Package/carbridge/conffiles
/etc/config/carbridge
/etc/carbridge/
endef

define Build/Prepare
	$(INSTALL_DIR) $(PKG_BUILD_DIR)
	$(CP) $(SOURCE_TREE)/Cargo.toml $(SOURCE_TREE)/Cargo.lock $(SOURCE_TREE)/LICENSE $(PKG_BUILD_DIR)/
	$(CP) $(SOURCE_TREE)/crates $(PKG_BUILD_DIR)/
endef

define Build/Compile
	$(call Build/Compile/Cargo,crates/carbridge)
endef

define Package/carbridge/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) $(PKG_INSTALL_DIR)/bin/carbridge $(1)/usr/bin/
	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./files/carbridge.init $(1)/etc/init.d/carbridge
	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./files/carbridge.config $(1)/etc/config/carbridge
	$(INSTALL_DIR) $(1)/etc/uci-defaults
	$(INSTALL_DATA) ./files/carbridge.defaults $(1)/etc/uci-defaults/90-carbridge
	$(INSTALL_DIR) $(1)/usr/libexec
	$(INSTALL_BIN) ./files/carlink-adapter.sh $(1)/usr/libexec/carbridge-carlink
	$(INSTALL_DIR) $(1)/etc/hotplug.d/ieee80211 $(1)/etc/hotplug.d/net
	$(INSTALL_DATA) ./files/carlink.hotplug-ieee80211 $(1)/etc/hotplug.d/ieee80211/05-carbridge-carlink
	$(INSTALL_DATA) ./files/carlink.hotplug-net $(1)/etc/hotplug.d/net/05-carbridge-carlink
	$(INSTALL_DIR) $(1)/usr/share/carbridge
	$(INSTALL_DATA) ./files/carlink-input.nft $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/waiting.h264 ./files/waiting.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/waiting-portrait.h264 ./files/waiting-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/calibrate.h264 ./files/calibrate.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/calibrate-portrait.h264 ./files/calibrate-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/calibrated.h264 ./files/calibrated.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/calibrated-portrait.h264 ./files/calibrated-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unpaired.h264 ./files/unpaired.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unpaired-portrait.h264 ./files/unpaired-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/pairing.h264 ./files/pairing.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/pairing-portrait.h264 ./files/pairing-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/connecting.h264 ./files/connecting.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/connecting-portrait.h264 ./files/connecting-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable.h264 ./files/unavailable.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-portrait.h264 ./files/unavailable-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-mfi.h264 ./files/unavailable-mfi.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-mfi-portrait.h264 ./files/unavailable-mfi-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-bluetooth.h264 ./files/unavailable-bluetooth.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-bluetooth-portrait.h264 ./files/unavailable-bluetooth-portrait.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-network.h264 ./files/unavailable-network.h265 $(1)/usr/share/carbridge/
	$(INSTALL_DATA) ./files/unavailable-network-portrait.h264 ./files/unavailable-network-portrait.h265 $(1)/usr/share/carbridge/
endef

$(eval $(call RustBinPackage,carbridge))
$(eval $(call BuildPackage,carbridge))
