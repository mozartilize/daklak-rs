project(
  'daklak',
  version: '0.1.0',
  meson_version: '>=0.60.0',
)

run_command('git', 'submodule', 'update', '--init', check: false)

cargo = find_program('cargo', required: true)

# Build-time system deps
#
# The Wayland protocol implementation is pure Rust (wayland-backend rs module),
# so libwayland-client/server/cursor/egl are NOT needed.
# The only C library linked at build time is xkbcommon.
xkbcommon_dep = dependency('xkbcommon', required: true)

# App indicator uses ksni (StatusNotifierItem over pure-Rust zbus) — no C library linked.
# dbus-1 is informational only: checked so the build summary reflects runtime requirements.
dbus_dep = dependency('dbus-1', required: false)

buildtype = get_option('buildtype')
is_dev_install = buildtype == 'debug'
cargo_profile = buildtype == 'debug' ? 'debug' : 'release'
cargo_target_dir = meson.current_build_dir() / 'cargo-target'
daklak_build_dir = meson.current_build_dir()

features = []
if get_option('kde')
  features += 'kde'
endif
if get_option('ibus')
  features += 'ibus'
endif
if get_option('evdev_grab')
  features += 'evdev_grab'
endif

summary({
  'prefix': get_option('prefix'),
  'bin dir': get_option('bindir'),
  'build type': buildtype,
  'kde support': get_option('kde'),
  'ibus/GNOME support': get_option('ibus'),
  'evdev grab': get_option('evdev_grab'),
  'xkbcommon': xkbcommon_dep.version(),
  'app indicator': 'ksni (StatusNotifierItem, pure-Rust zbus)',
  'dbus-1 (runtime)': dbus_dep.found() ? dbus_dep.version() : 'not found (informational)',
}, bool_yn: true, section: 'Configuration')

if get_option('evdev_grab')
  daklak_builtin_hook_dir = get_option('prefix') / get_option('libexecdir') / 'daklak' / 'hooks'
  daklak_builtin_xkb_dir = get_option('prefix') / get_option('datadir') / 'daklak' / 'xkb'
  install_data('res/99-daklak.rules',
    install_dir: '/etc/udev/rules.d',
    install_mode: 'rw-r--r--',
  )
  install_data(
    'res/evdev-hooks/sway-set',
    'res/evdev-hooks/sway-unset',
    'res/evdev-hooks/kde-set',
    'res/evdev-hooks/kde-unset',
    'res/evdev-hooks/gnome-set',
    'res/evdev-hooks/gnome-unset',
    'res/evdev-hooks/x11-set',
    'res/evdev-hooks/x11-unset',
    install_dir: get_option('libexecdir') / 'daklak' / 'hooks',
    install_mode: 'rwxr-xr-x',
  )
  meson.add_install_script('scripts/meson-postinst.sh')
else
  daklak_builtin_hook_dir = ''
  daklak_builtin_xkb_dir = ''
endif

install_data('res/config.toml.example',
  install_dir: get_option('datadir') / 'daklak',
)

if get_option('kde')
  desktop_conf = configuration_data()
  desktop_conf.set(
    'DAKLAK_EXEC',
    is_dev_install
      ? meson.project_source_root() / 'daklak-wrap.sh'
      : get_option('prefix') / get_option('bindir') / 'daklak',
  )
  desktop = configure_file(
    input: 'res/daklak.desktop.in',
    output: 'daklak.desktop',
    configuration: desktop_conf,
  )
  install_data(desktop,
    install_dir: get_option('datadir') / 'applications',
  )
endif

if get_option('ibus')
  ibus_comp_conf = configuration_data()
  ibus_comp_conf.set(
    'daklak_exec',
    is_dev_install
      ? meson.project_source_root() / 'daklak-ibus-wrap.sh'
      : get_option('prefix') / get_option('bindir') / 'daklak --ibus',
  )
  ibus_comp_conf.set('version', meson.project_version())
  ibus_comp = configure_file(
    input: 'res/ibus/daklak.xml',
    output: 'daklak.xml',
    configuration: ibus_comp_conf,
  )
  ibus_comp_dir = get_option('datadir') / 'ibus' / 'component'
  install_data(ibus_comp,
    install_dir: ibus_comp_dir,
  )
  meson.add_install_script('scripts/meson-ibus-postinst.sh')
endif

daklak_bin = custom_target(
  'daklak-bin',
  output: 'daklak',
  command: [
    find_program('scripts/cargo-build.sh'),
    cargo,
    cargo_target_dir,
    cargo_profile,
    '@OUTPUT@',
    meson.project_source_root(),
    ','.join(features),
    daklak_builtin_hook_dir,
    daklak_builtin_xkb_dir,
  ],
  build_by_default: true,
  build_always_stale: true,
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  install_mode: 'rwxr-xr-x',
)

if get_option('evdev_grab')
  xkb_artifact_generator = find_program('scripts/generate-xkb-symbols.sh')
  daklak_xkb_symbols = custom_target(
    'daklak-xkb-symbols',
    output: 'daklak_vn',
    command: [
      xkb_artifact_generator,
      meson.current_build_dir(),
      '@OUTPUT@',
      'symbols',
    ],
    depends: [daklak_bin],
    build_by_default: true,
    build_always_stale: true,
    console: true,
    install: true,
    install_dir: get_option('datadir') / 'X11' / 'xkb' / 'symbols',
  )
  subdir('res/xkb')
endif
