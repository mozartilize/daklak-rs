daklak_xkb_blob_for_hooks = custom_target(
  'daklak-xkb-blob-for-hooks',
  output: 'daklak.xkb',
  command: [
    xkb_artifact_generator,
    daklak_build_dir,
    '@OUTPUT@',
    'keymap',
  ],
  depends: [daklak_bin],
  build_by_default: true,
  build_always_stale: true,
  console: true,
  install: true,
  install_dir: get_option('datadir') / 'daklak' / 'xkb',
)

daklak_xkb_symbols_for_hooks = custom_target(
  'daklak-xkb-symbols-for-hooks',
  output: 'daklak_vn',
  command: [
    xkb_artifact_generator,
    daklak_build_dir,
    '@OUTPUT@',
    'symbols',
  ],
  depends: [daklak_bin],
  build_by_default: true,
  build_always_stale: true,
  console: true,
  install: true,
  install_dir: get_option('datadir') / 'daklak' / 'xkb',
)

daklak_xkb_rules_for_hooks = custom_target(
  'daklak-xkb-rules-for-hooks',
  output: 'evdev',
  command: [
    xkb_artifact_generator,
    daklak_build_dir,
    '@OUTPUT@',
    'rules',
  ],
  depends: [daklak_bin],
  build_by_default: true,
  build_always_stale: true,
  console: true,
  install: true,
  install_dir: get_option('datadir') / 'daklak' / 'xkb',
)
