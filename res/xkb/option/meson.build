# The `daklak:vn` xkb option for KWin sessions, installed into libxkbcommon's
# XKB extensions directory or ~/.config/xkb (see `xkb_option_dir` in the
# top-level meson.build).
# The rules file is `evdev.post`, which libxkbcommon appends to the stock evdev
# ruleset, so it composes with other packages. The registry fragment exposes
# the standalone US + Daklak layout in keyboard settings as an alternative
# to enabling the daklak:vn option on an existing layout.
install_data('evdev.xml', install_dir: xkb_option_dir / 'rules')
foreach artifact : [['daklak_vn', 'symbols', 'symbols'], ['evdev.post', 'rules', 'rules-post']]
  custom_target(
    'daklak-xkb-option-' + artifact[2],
    output: artifact[0],
    command: [
      xkb_artifact_generator,
      daklak_build_dir,
      '@OUTPUT@',
      artifact[2],
    ],
    depends: [daklak_bin],
    build_by_default: true,
    build_always_stale: true,
    console: true,
    install: true,
    install_dir: xkb_option_dir / artifact[1],
  )
endforeach
