#!/usr/bin/env bash
set -euo pipefail

version="${WILDDATUM_VERSION:-${ECOSCOPE_VERSION:-v0.1.0-alpha.2}}"
install_root="${WILDDATUM_INSTALL_DIR:-${ECOSCOPE_INSTALL_DIR:-${HOME:?HOME is not set}/.local}}"
repo="https://github.com/krnzt/wilddatum"
os="$(uname -s)"
arch="$(uname -m)"

case "$os/$arch" in
  Darwin/arm64|Darwin/x86_64) target="macos-universal" ;;
  Linux/x86_64) target="linux-x86_64" ;;
  *)
    echo "WildDatum $version has no prebuilt package for $os/$arch." >&2
    echo "Supported: macOS arm64/x86_64 and Linux x86_64." >&2
    exit 1
    ;;
esac

base="wilddatum-${version}-${target}"
url="$repo/releases/download/$version/$base.tar.gz"
temporary="$(mktemp -d)"
trap 'rm -rf "$temporary"' EXIT

curl -fL --retry 3 -o "$temporary/$base.tar.gz" "$url"
curl -fL --retry 3 -o "$temporary/$base.tar.gz.sha256" "$url.sha256"
(cd "$temporary" && shasum -a 256 -c "$base.tar.gz.sha256")
tar -C "$temporary" -xzf "$temporary/$base.tar.gz"
mkdir -p "$install_root/bin" "$install_root/share/wilddatum"
cp "$temporary/$base/bin/wilddatum" "$install_root/bin/wilddatum"
if [[ ! -e "$install_root/bin/ecoscope" || -L "$install_root/bin/ecoscope" ]]; then
  ln -sfn wilddatum "$install_root/bin/ecoscope"
fi
rm -rf "$install_root/share/wilddatum/web"
cp -R "$temporary/$base/share/wilddatum/web" "$install_root/share/wilddatum/web"

echo "Installed WildDatum $version to $install_root/bin/wilddatum"
case ":${PATH:-}:" in
  *":$install_root/bin:"*) ;;
  *) echo "Add $install_root/bin to PATH before continuing." ;;
esac
"$install_root/bin/wilddatum" setup
