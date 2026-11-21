#!/bin/sh
# Build the data files of Fractal+ (resources, translations, settings schema,
# icon) without the program, plus a config.rs with paths relative to the
# program, for the Windows and macOS packages. Their builds compile the program
# with Cargo alone, because the tools to build the data don't run there.
#
# Usage: build-aux/build-data.sh OUTPUT_DIR
set -eu

out=$(realpath -m "$1")
build=_build-data

meson setup "$build" -Dprofile=default --prefix=/ --wipe 2>/dev/null \
  || meson setup "$build" -Dprofile=default --prefix=/
meson compile -C "$build" resources_gresource ui-resources_gresource fractalplus-gmo

rm -rf "$out"
mkdir -p "$out/share/fractalplus" "$out/share/glib-2.0/schemas" "$out/share/locale" \
  "$out/share/icons/hicolor/scalable/apps"
cp "$build/data/resources/resources.gresource" "$build/src/ui-resources.gresource" \
  "$out/share/fractalplus/"
cp "$build"/data/*.gschema.xml "$out/share/glib-2.0/schemas/"
for mo in "$build"/po/*/LC_MESSAGES/fractalplus.mo; do
  lang=$(basename "$(dirname "$(dirname "$mo")")")
  mkdir -p "$out/share/locale/$lang/LC_MESSAGES"
  cp "$mo" "$out/share/locale/$lang/LC_MESSAGES/"
done
app_id=$(sed -n "s/^application_id = '\(.*\)'/\1/p" meson.build)
cp data/icons/org.gnome.Fractal.svg "$out/share/icons/hicolor/scalable/apps/$app_id.svg"

# With the prefix /, the data paths are /share/…: make them relative.
sed 's#"/share/#"share/#g' src/config.rs > "$out/config.rs"
grep -q '"share/fractalplus"' "$out/config.rs"
echo "Data files in $out"
