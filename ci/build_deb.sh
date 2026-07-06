#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.."

ci/build_zst.sh
scripts/release-deb.sh /opt/daklak-out
