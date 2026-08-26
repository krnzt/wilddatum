#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${1:?version is required}"
tag="${2:?tag is required}"
macos_mcpb="${3:?macOS MCPB is required}"
linux_mcpb="${4:?Linux MCPB is required}"
output="${5:-$repo_dir/server.json}"
release_base="https://github.com/krnzt/ecoscope/releases/download/$tag"

sha() {
  shasum -a 256 "$1" | awk '{print $1}'
}

jq \
  --arg version "$version" \
  --arg macos_url "$release_base/$(basename "$macos_mcpb")" \
  --arg macos_sha "$(sha "$macos_mcpb")" \
  --arg linux_url "$release_base/$(basename "$linux_mcpb")" \
  --arg linux_sha "$(sha "$linux_mcpb")" \
  '.version = $version
   | .packages[0].identifier = $macos_url
   | .packages[0].fileSha256 = $macos_sha
   | .packages[1].identifier = $linux_url
   | .packages[1].fileSha256 = $linux_sha' \
  "$repo_dir/packages/mcpb/server.template.json" > "$output"
