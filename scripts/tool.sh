#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.."

DAKLAK_OUT="${DAKLAK_OUT:-$PWD/build/out}"
