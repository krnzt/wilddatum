#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary_path="${1:-$repo_dir/target/release/ecoscope}"
output_path="${2:-$repo_dir/dist/ecoscope.mcpb}"
platform="${3:-}"
stage_dir="$(mktemp -d)"
trap 'rm -rf "$stage_dir"' EXIT

case "$platform" in
  darwin|linux) ;;
  *) echo "usage: $0 BINARY OUTPUT {darwin|linux}" >&2; exit 2 ;;
esac

test -x "$binary_path"
test -f "$repo_dir/viewer/web-bootstrap/dist/index.html"
command -v jq >/dev/null
version="$($binary_path --version | awk '{print $2}')"
mkdir -p "$stage_dir/server" "$stage_dir/share/ecoscope"
jq --arg version "$version" --arg platform "$platform" \
  '.version = $version | .compatibility.platforms = [$platform]' \
  "$repo_dir/packages/mcpb/manifest.json" > "$stage_dir/manifest.json"
cp "$repo_dir/LICENSE" "$stage_dir/LICENSE"
cp "$repo_dir/THIRD_PARTY_NOTICES.md" "$stage_dir/THIRD_PARTY_NOTICES.md"
cp "$binary_path" "$stage_dir/server/ecoscope"
cp -R "$repo_dir/viewer/web-bootstrap/dist" "$stage_dir/share/ecoscope/web"
mkdir -p "$(dirname "$output_path")"
output_dir="$(cd "$(dirname "$output_path")" && pwd)"
output_path="$output_dir/$(basename "$output_path")"
(cd "$stage_dir" && zip -q -r "$output_path" .)
openssl dgst -sha256 "$output_path"
