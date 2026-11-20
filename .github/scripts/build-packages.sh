#!/bin/bash
# Builds the CarBridge packages with the OpenWrt SDK, in the SDK's directory (the working directory of its container).
#
#   /feed   this repository (FEED)
#   /cache  where the Rust toolchain is kept between runs (CACHE)
#   /out    receives bin/ with the packages, and logs/ (OUT)
#
# The SDK builds Rust from source, which takes hours, and installs it into its staging directory. This script keeps
# that installation, with the stamps that tell the build system it is done, in rust.tar in the cache directory, and
# puts it back on the next run. The SDK's release pins its feeds, so the Rust it builds stays the same for one SDK.
set -euo pipefail

feed="${FEED:-/feed}"
out="${OUT:-/out}"
cache="${CACHE:-/cache}/rust.tar"
packages=(carbridge luci-app-carbridge)

# Snapshot containers ship without the SDK and download it with setup.sh
[ ! -f setup.sh ] || bash setup.sh

sed -e 's,https://git.openwrt.org/feed/,https://github.com/openwrt/,' \
	-e 's,https://git.openwrt.org/openwrt/,https://github.com/openwrt/,' \
	-e 's,https://git.openwrt.org/project/,https://github.com/openwrt/,' \
	feeds.conf.default > feeds.conf
echo "src-link carbridge $feed" >> feeds.conf
cat feeds.conf

./scripts/feeds update -a
./scripts/feeds install "${packages[@]}"
make defconfig

if [ -f "$cache" ]; then
	echo "Restoring Rust from the cache"
	tar -xf "$cache"
	# The feeds were just checked out, so their files are newer than the cached stamps, which would have the build
	# system build Rust again. Make the stamps newer, in the order the build steps made them.
	find build_dir/target-*/host/rustc-*-src -maxdepth 1 -type f -name '.*' -exec touch {} +
	touch staging_dir/hostpkg/stamp/.rust_installed
fi

status=0
for pkg in "${packages[@]}"; do
	make BUILD_LOG=1 CONFIG_AUTOREMOVE=y V="${V:-s}" -j "$(nproc)" "package/$pkg/compile" || { status=$?; break; }
done

if [ "$status" -eq 0 ] && [ ! -f "$cache" ]; then
	echo "Saving Rust to the cache"
	tar -cf "$cache" staging_dir/target-*/host build_dir/target-*/host/rustc-*-src \
		staging_dir/hostpkg/stamp/.rust_installed
fi

[ ! -d bin ] || mv bin "$out/"
[ ! -d logs ] || mv logs "$out/"
exit "$status"
