#!/bin/sh
# Draw the screens a design pass looks at, in a dark and a light theme, as pictures.
#
# The `screens` test draws each scene with the real view and writes its cells; this turns
# them into PNG files under target/screens.
set -e
cd "$(dirname "$0")/.."
out=target/screens
rm -rf "$out"
for theme in dark light; do
  LOKODE_SCREENS="$out/cells" LOKODE_SCREEN_THEME="$theme" \
    cargo test -q --lib ui::view::tests::screens -- --ignored --exact >/dev/null
done
python3 scripts/screens.py "$out/cells" "$out"
