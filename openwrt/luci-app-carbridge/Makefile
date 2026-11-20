# SPDX-FileCopyrightText: 2026 CarBridge contributors
# SPDX-License-Identifier: AGPL-3.0-only

include $(TOPDIR)/rules.mk

LUCI_TITLE:=Web interface for CarBridge
# The version is CarBridge's, from the workspace's Cargo.toml.
PKG_VERSION:=$(shell sed -n 's/^version = "\(.*\)"/\1/p' $(CURDIR)/../../Cargo.toml)
LUCI_DEPENDS:=+carbridge +luci-base
LUCI_PKGARCH:=all

PKG_LICENSE:=AGPL-3.0-only

include $(TOPDIR)/feeds/luci/luci.mk

# call BuildPackage - OpenWrt buildroot signature
