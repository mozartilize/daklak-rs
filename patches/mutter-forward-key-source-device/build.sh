#!/bin/sh
set -eu

src_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
src="$src_dir/mutter-forward-key-source-device.c"
out_dir="${HOME}/.local/lib/daklak"
out="${out_dir}/libmutter-forward-key-source-device.so"

mkdir -p "$out_dir"

# No link against clutter/glib: the detour resolves every Clutter symbol at
# runtime via dlopen(RTLD_NOLOAD) inside the host process. Only -ldl is needed.
# -fcf-protection=full: the host enables CET/IBT, and we reach our handler via
# an indirect jmp, so the handler needs its own endbr64 landing pad.
cc -shared -fPIC -Wall -Wextra -O2 -fcf-protection=full \
  "$src" \
  -o "$out" \
  -ldl

echo "built $out"

# ---------------------------------------------------------------------------
# Build-time verification.
#
# The detour is pinned to a specific mutter ABI. A soname bump or a changed
# clutter_event_key_new signature would silently stop the patch from installing
# (or crash gnome-shell at runtime). Verify NOW, against the real library on
# this machine, instead of discovering it at next login.
#
# Strategy: build a tiny probe that load-time links the target library (so it is
# resident before our constructor runs), preload our .so into it, then read the
# first bytes of clutter_input_method_forward_key and assert the detour shape:
#
#     f3 0f 1e fa   endbr64        (CET landing pad preserved)
#     48 b8 <imm64> movabs rax     (our handler address)
#     ff e0         jmp rax
#
# Skip with DAKLAK_SKIP_VERIFY=1 (e.g. building on a box with no GNOME).
# ---------------------------------------------------------------------------

if [ "${DAKLAK_SKIP_VERIFY:-0}" = "1" ]; then
  echo "verify: skipped (DAKLAK_SKIP_VERIFY=1)"
  echo "enable with: export LD_PRELOAD=\"$out\${LD_PRELOAD:+:\$LD_PRELOAD}\""
  exit 0
fi

# Single source of truth: the soname the detour actually dlopen()s.
soname=$(grep -oE 'libmutter-clutter-[0-9]+\.so\.[0-9]+' "$src" | head -n1)
if [ -z "$soname" ]; then
  echo "verify: FAIL — could not parse the target soname from $src" >&2
  exit 1
fi
echo "verify: target soname = $soname"

# Locate the library on this machine.
libpath=$(ldconfig -p 2>/dev/null | grep -F "$soname" | awk 'NR==1{print $NF}')
if [ -z "$libpath" ]; then
  libpath=$(find /usr/lib /usr/lib64 /usr/lib/* -maxdepth 2 -name "$soname" 2>/dev/null | head -n1 || true)
fi
if [ -z "$libpath" ] || [ ! -e "$libpath" ]; then
  echo "verify: SKIP — '$soname' not found on this machine." >&2
  echo "        The detour will only install where that soname exists (this GNOME's mutter)." >&2
  echo "        If this is the target box, mutter's Clutter ABI differs — update the soname in the .c." >&2
  exit 1
fi
echo "verify: found $libpath"

probe_dir=$(mktemp -d)
trap 'rm -rf "$probe_dir"' EXIT
probe_src="$probe_dir/probe.c"
probe_bin="$probe_dir/probe"

# Probe uses only dlopen/dlsym (no clutter headers). It load-time links the real
# library by full path so the lib is mapped before any constructor runs, which
# is what makes our RTLD_NOLOAD dlopen succeed and patch it.
cat > "$probe_src" <<'PROBE'
#include <dlfcn.h>
#include <stdio.h>
int main(void) {
  void *h = dlopen("DAKLAK_SONAME", RTLD_NOW | RTLD_NOLOAD);
  if (!h) { fprintf(stderr, "lib not resident\n"); return 2; }
  unsigned char *b = (unsigned char *) dlsym(h, "clutter_input_method_forward_key");
  if (!b) { fprintf(stderr, "symbol clutter_input_method_forward_key missing\n"); return 3; }
  int endbr = (b[0]==0xf3 && b[1]==0x0f && b[2]==0x1e && b[3]==0xfa);
  int det   = (b[4]==0x48 && b[5]==0xb8 && b[14]==0xff && b[15]==0xe0);
  if (endbr && det) { printf("OK\n"); return 0; }
  fprintf(stderr, "prologue not patched (endbr=%d detour=%d): "
          "%02x %02x %02x %02x %02x %02x ... %02x %02x\n",
          endbr, det, b[0],b[1],b[2],b[3],b[4],b[5],b[14],b[15]);
  return 1;
}
PROBE
# Substitute the parsed soname into the probe.
sed -i "s/DAKLAK_SONAME/$soname/" "$probe_src"

libdir=$(dirname "$libpath")
if ! cc "$probe_src" -o "$probe_bin" "$libpath" -ldl -Wl,-rpath,"$libdir" 2>"$probe_dir/cc.err"; then
  echo "verify: SKIP — could not link probe against $soname:" >&2
  sed 's/^/        /' "$probe_dir/cc.err" >&2
  exit 1
fi

logfile=/tmp/daklak-mutter-forward-key.log
rm -f "$logfile"
if LD_LIBRARY_PATH="$libdir" LD_PRELOAD="$out" "$probe_bin"; then
  echo "verify: PASS — detour installs on $soname (endbr64 preserved, jmp to handler)"
  [ -f "$logfile" ] && sed 's/^/        /' "$logfile" || true
else
  echo "verify: FAIL — detour did NOT install correctly on $soname." >&2
  echo "        Likely an ABI change (soname or clutter_event_key_new signature)." >&2
  [ -f "$logfile" ] && { echo "        constructor log:" >&2; sed 's/^/        /' "$logfile" >&2; }
  exit 1
fi

echo "enable with: export LD_PRELOAD=\"$out\${LD_PRELOAD:+:\$LD_PRELOAD}\""
