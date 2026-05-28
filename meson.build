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

buildtype = get_option('buildtype')
cargo_profile = buildtype == 'debug' ? 'debug' : 'release'
cargo_target_dir = meson.current_build_dir() / 'cargo-target'

features = []
if get_option('kde')
  features += 'kde'
endif

summary({
  'prefix': get_option('prefix'),
  'bin dir': get_option('bindir'),
  'build type': buildtype,
  'kde support': get_option('kde'),
  'evdev grab': get_option('evdev_grab'),
  'xkbcommon': xkbcommon_dep.version(),
}, bool_yn: true, section: 'Configuration')

if get_option('evdev_grab')
  install_data('res/99-daklak.rules',
    install_dir: '/etc/udev/rules.d',
    install_mode: 'rw-r--r--',
  )
  meson.add_install_script('scripts/meson-postinst.sh')
endif

install_data('res/config.toml.example',
  install_dir: get_option('datadir') / 'daklak',
)

if get_option('kde')
  desktop_conf = configuration_data()
  desktop_conf.set('BINDIR', get_option('prefix') / get_option('bindir'))
  desktop = configure_file(
    input: 'res/daklak.desktop.in',
    output: 'daklak.desktop',
    configuration: desktop_conf,
  )
  install_data(desktop,
    install_dir: get_option('datadir') / 'applications',
  )
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
  ],
  build_by_default: true,
  build_always_stale: true,
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  install_mode: 'rwxr-xr-x',
)
