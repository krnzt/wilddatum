#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary_path="${1:?binary path is required}"
output_dir="${2:?output directory is required}"
target_slug="${3:?target slug is required}"
platform="${4:?MCPB platform is required}"
stage_dir="$(mktemp -d)"
trap 'rm -rf "$stage_dir"' EXIT

test -x "$binary_path"
test -f "$repo_dir/viewer/web-bootstrap/dist/index.html"
version="$($binary_path --version | awk '{print $2}')"
base="ecoscope-v${version}-${target_slug}"
mkdir -p "$stage_dir/$base/bin" "$stage_dir/$base/share/ecoscope" "$output_dir"
cp "$binary_path" "$stage_dir/$base/bin/ecoscope"
cp "$repo_dir/LICENSE" "$stage_dir/$base/LICENSE"
cp "$repo_dir/THIRD_PARTY_NOTICES.md" "$stage_dir/$base/THIRD_PARTY_NOTICES.md"
cp -R "$repo_dir/viewer/web-bootstrap/dist" "$stage_dir/$base/share/ecoscope/web"

archive="$output_dir/$base.tar.gz"
tar -C "$stage_dir" -czf "$archive" "$base"
"$repo_dir/scripts/build-mcpb.sh" \
  "$binary_path" "$output_dir/$base.mcpb" "$platform"

for artifact in "$archive" "$output_dir/$base.mcpb"; do
  (cd "$output_dir" && shasum -a 256 "$(basename "$artifact")" > "$(basename "$artifact").sha256")
done
