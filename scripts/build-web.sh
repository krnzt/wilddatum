#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
npm --prefix "$repo_dir/viewer/web-bootstrap" ci
npm --prefix "$repo_dir/viewer/web-bootstrap" run build

