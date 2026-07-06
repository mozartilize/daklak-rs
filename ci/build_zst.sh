#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.."

scripts/build.sh -r
scripts/release-zst.sh /opt/daklak-out
