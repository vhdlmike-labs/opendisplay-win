#!/usr/bin/env bash
# Build dist/OpenDisplay-Windows-<version>.zip: the release exe, the signed
# MTT Virtual Display Driver (downloaded, checksum-pinned), helpers, docs.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/opt/llvm-mingw-20261020-ucrt-ubuntu-22.04-x86_64/bin:$PATH"

VDD_URL=https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/25.7.23/VirtualDisplayDriver-x86.Driver.Only.zip
VDD_SHA256=e24210692b442b39af763536330ce78b423f19342b7a7792c26de3944e418b3a
VDD_LICENSE=https://raw.githubusercontent.com/VirtualDrivers/Virtual-Display-Driver/master/LICENSE

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="OpenDisplay-Windows-$version"
work=target/package
out="$work/$name"

cargo build --release
rm -rf "$work" && mkdir -p "$out/driver" "$work/dl" dist

curl -sSfL -o "$work/dl/vdd.zip" "$VDD_URL"
echo "$VDD_SHA256  $work/dl/vdd.zip" | sha256sum -c -
python3 -m zipfile -e "$work/dl/vdd.zip" "$work/dl/vdd"
for f in MttVDD.inf MttVDD.dll mttvdd.cat; do cp "$work/dl/vdd/VirtualDisplayDriver/$f" "$out/driver/"; done
curl -sSfL -o "$out/driver/LICENSE.txt" "$VDD_LICENSE"
python3 packaging/make_vdd_settings.py "$out/driver/vdd_settings.xml"

cp target/x86_64-pc-windows-gnullvm/release/OpenDisplay.exe target/x86_64-pc-windows-gnullvm/release/opendisplay-win.exe "$out/"
cp packaging/*.cmd "$out/"
sed 's/$/\r/' packaging/README.txt > "$out/README.txt"
sed 's/$/\r/' LICENSE > "$out/LICENSE.txt"

rm -f "dist/$name.zip"
(cd "$work" && python3 -m zipfile -c "../../dist/$name.zip" "$name")
sha256sum "dist/$name.zip"
python3 -m zipfile -l "dist/$name.zip"
