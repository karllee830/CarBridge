#!/bin/sh
# SPDX-FileCopyrightText: 2026 CarBridge contributors
# SPDX-License-Identifier: AGPL-3.0-only
#
# The Wi-Fi adapter that joins the head unit's network, chosen by its MAC address (carlink_mac in
# /etc/config/carbridge).
#
#   carbridge-carlink list    the router's Wi-Fi adapters, as JSON, for the web interface
#   carbridge-carlink setup   when the chosen adapter is there: takes it out of OpenWrt's Wi-Fi and names its
#                             interface carlink0
#
# setup runs when an adapter or a network interface appears (hotplug) and before CarBridge starts.

IFACE=carlink0
LOCK=/var/lock/carbridge-carlink.lock

log() { logger -t carbridge "carlink adapter: $*"; }

lower() { echo "$1" | tr 'A-F' 'a-f'; }

# phy_iface PHY: the network interface of a phy, the first one by name
phy_iface() {
	local n
	for n in /sys/class/net/*; do
		[ -e "$n/phy80211" ] || continue
		[ "$(readlink -f "$n/phy80211")" = "$(readlink -f "/sys/class/ieee80211/$1")" ] && { echo "${n##*/}"; return 0; }
	done
	return 1
}

# phy_by_mac MAC: the phy whose permanent address is MAC
phy_by_mac() {
	local p
	for p in /sys/class/ieee80211/*; do
		[ -e "$p/macaddress" ] || continue
		[ "$(lower "$(cat "$p/macaddress")")" = "$1" ] && { echo "${p##*/}"; return 0; }
	done
	return 1
}

# radios PHY MAC: the sections of /etc/config/wireless that stand for the phy, by their macaddr or by a path its
# device's ends with, as OpenWrt matches them
radios() {
	local dev s path mac
	dev=$(readlink -f "/sys/class/ieee80211/$1/device")
	for s in $(uci -q show wireless | sed -n 's/^wireless\.\([^.=]*\)=wifi-device$/\1/p'); do
		mac=$(lower "$(uci -q get "wireless.$s.macaddr")")
		path=$(uci -q get "wireless.$s.path")
		if [ -n "$mac" ] && [ "$mac" = "$2" ]; then
			echo "$s"
		elif [ -n "$path" ] && [ "${dev%"$path"}" != "$dev" ]; then
			echo "$s"
		fi
	done
}

list() {
	local p phy mac drv dev bus iface ap used s
	ap=$(uci -q get carbridge.main.ap_iface || echo wlan0)
	. /usr/share/libubox/jshn.sh
	json_init
	json_add_array adapters
	for p in /sys/class/ieee80211/*; do
		[ -e "$p/macaddress" ] || continue
		phy=${p##*/}
		mac=$(lower "$(cat "$p/macaddress")")
		drv=$(basename "$(readlink "$p/device/driver")" 2>/dev/null)
		dev=$(readlink -f "$p/device")
		case $dev in */usb*) bus=usb ;; *) bus=builtin ;; esac
		iface=$(phy_iface "$phy")
		# whether OpenWrt's own Wi-Fi has the adapter: a section for it that netifd sets up
		used=0
		for s in $(radios "$phy" "$mac"); do
			[ "$(uci -q get "wireless.$s.type")" = carbridge ] || used=1
		done
		json_add_object
		json_add_string mac "$mac"
		json_add_string phy "$phy"
		json_add_string driver "$drv"
		json_add_string bus "$bus"
		json_add_string iface "$iface"
		json_add_boolean ap "$([ -n "$iface" ] && [ "$iface" = "$ap" ] && echo 1 || echo 0)"
		json_add_boolean openwrt "$used"
		json_close_object
	done
	json_close_array
	json_dump
}

setup() {
	local mac phy found s iface
	mac=$(lower "$(uci -q get carbridge.main.carlink_mac)")
	[ -n "$mac" ] || return 0
	phy=$(phy_by_mac "$mac") || return 0

	# netifd sets up every radio of a type it has a handler for, a disabled one too: with no interfaces, and so it
	# deletes the adapter's interface. It leaves a radio of another type alone, while OpenWrt's detection adds no
	# radio for an adapter that has a section of any type. So the adapter's sections get the type carbridge, the one
	# they had kept in carbridge_type; a section made here names the adapter by its MAC address, as the setting does.
	found=
	for s in $(radios "$phy" "$mac"); do
		found=1
		type=$(uci -q get "wireless.$s.type")
		[ "$type" = carbridge ] && continue
		uci -q set "wireless.$s.carbridge_type=$type"
		uci -q set "wireless.$s.type=carbridge"
		uci -q set "wireless.$s.disabled=1"
		log "$mac taken out of OpenWrt's Wi-Fi ($s)"
	done
	if [ -z "$found" ]; then
		# the section is CarBridge's own: one left from another adapter is replaced
		uci -q delete wireless.carbridge_carlink
		uci -q batch <<-EOF
			set wireless.carbridge_carlink=wifi-device
			set wireless.carbridge_carlink.type='carbridge'
			set wireless.carbridge_carlink.carbridge_type='mac80211'
			set wireless.carbridge_carlink.macaddr='$mac'
			set wireless.carbridge_carlink.disabled='1'
		EOF
		log "$mac taken out of OpenWrt's Wi-Fi (carbridge_carlink)"
	fi
	[ -n "$(uci -q changes wireless)" ] && uci -q commit wireless

	# an adapter without an interface, as netifd leaves one it has set up, gets one
	if ! iface=$(phy_iface "$phy"); then
		if [ -e "/sys/class/net/$IFACE" ]; then
			log "cannot add $IFACE to $mac: another interface has that name"
			return 1
		fi
		iw phy "$phy" interface add "$IFACE" type managed || { log "cannot add $IFACE to $mac"; return 1; }
		log "$mac is $IFACE (added)"
		return 0
	fi
	[ "$iface" = "$IFACE" ] && return 0
	if [ -e "/sys/class/net/$IFACE" ]; then
		log "cannot name $iface $IFACE: another interface has that name"
		return 1
	fi
	ip link set dev "$iface" down && ip link set dev "$iface" name "$IFACE" || {
		log "cannot name $iface $IFACE"
		return 1
	}
	log "$mac is $IFACE (was $iface)"
}

case $1 in
	list) list ;;
	setup)
		# hotplug runs this for the adapter and for its interface, at once
		lock "$LOCK"
		setup; rc=$?
		lock -u "$LOCK"
		exit $rc ;;
	*) echo "usage: carbridge-carlink list|setup" >&2; exit 2 ;;
esac
