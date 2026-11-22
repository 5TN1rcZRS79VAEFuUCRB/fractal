#!/bin/bash
# Package Fractal+ for macOS as an app in a disk image, with the Homebrew
# libraries it uses.
#
# Usage: build-aux/macos/package.sh PROGRAM DATA_DIR
# where DATA_DIR comes from build-aux/build-data.sh.
set -euo pipefail

program=$1
data=$2
here=$(cd "$(dirname "$0")" && pwd)
brew=$(brew --prefix)
gst=$(brew --prefix gstreamer)
app="Fractal+.app"
contents="$app/Contents"

# The GStreamer plugins the app uses: calls, camera and screen capture, media
# messages (decoded by the system with applemedia) and the audio level of
# voice messages.
plugins=(coreelements app playback typefindfunctions autodetect videoconvertscale
  videorate videotestsrc audioconvert audioresample audiotestsrc volume level
  osxaudio applemedia jpeg isomp4 matroska vpx opus ogg audioparsers
  videoparsersbad gtk4)

rm -rf "$app" fractalplus-macos-arm64.dmg
mkdir -p "$contents/MacOS" "$contents/Resources" "$contents/lib/gstreamer-1.0" \
  "$contents/libs"
cp "$program" "$contents/MacOS/fractalplus"
cp -R "$data/share" "$contents/"

# GTK's own settings schemas, and the icon themes.
cp "$brew"/share/glib-2.0/schemas/org.gtk.gtk4.*.gschema.xml \
  "$contents/share/glib-2.0/schemas/"
glib-compile-schemas "$contents/share/glib-2.0/schemas"
mkdir -p "$contents/share/icons"
for theme in hicolor Adwaita; do
  if [ -d "$brew/share/icons/$theme" ]; then
    cp -R "$brew/share/icons/$theme" "$contents/share/icons/"
  fi
done

bundled=(-x "$contents/MacOS/fractalplus")
for plugin in "${plugins[@]}"; do
  file="$gst/lib/gstreamer-1.0/libgst$plugin.dylib"
  if [ -f "$file" ]; then
    cp "$file" "$contents/lib/gstreamer-1.0/"
    chmod u+w "$contents/lib/gstreamer-1.0/libgst$plugin.dylib"
    bundled+=(-x "$contents/lib/gstreamer-1.0/libgst$plugin.dylib")
  else
    echo "No GStreamer plugin $plugin"
  fi
done

# Copy the libraries and point the program and plugins to the copies.
# Libraries found through @rpath are looked up in the search folders. Without
# input, dylibbundler would ask forever for one it can't find.
dylibbundler -of -b "${bundled[@]}" -d "$contents/libs" -p @executable_path/../libs/ \
  -s "$gst/lib" -s "$brew/lib" < /dev/null

# dylibbundler can add its search path to a file twice, and macOS refuses to
# load a library with a duplicate one, so keep only one of each.
for file in "$contents/MacOS/fractalplus" "$contents"/lib/gstreamer-1.0/*.dylib \
  "$contents"/libs/*.dylib; do
  otool -l "$file" | awk '/cmd LC_RPATH/ {getline; getline; print $2}' | sort | uniq -d |
    while read -r rpath; do
      count=$(otool -l "$file" | awk '/cmd LC_RPATH/ {getline; getline; print $2}' |
        grep -cxF "$rpath")
      for _ in $(seq 2 "$count"); do
        install_name_tool -delete_rpath "$rpath" "$file"
      done
      echo "Removed duplicate rpath $rpath from $file"
    done
done

sed "s/@VERSION@/$(sed -n 's/^ *version: .\([0-9.a-z-]*\).,$/\1/p' meson.build | head -1)/" \
  "$here/Info.plist" > "$contents/Info.plist"
if command -v rsvg-convert >/dev/null && command -v iconutil >/dev/null; then
  iconset=$(mktemp -d)/fractalplus.iconset
  mkdir -p "$iconset"
  for size in 16 32 128 256 512; do
    rsvg-convert -w $size -h $size data/icons/org.gnome.Fractal.svg -o "$iconset/icon_${size}x${size}.png"
    rsvg-convert -w $((size * 2)) -h $((size * 2)) data/icons/org.gnome.Fractal.svg \
      -o "$iconset/icon_${size}x${size}@2x.png"
  done
  iconutil -c icns "$iconset" -o "$contents/Resources/fractalplus.icns"
fi

# Without a developer ID, sign it ad hoc: Apple silicon refuses to run
# unsigned code at all.
codesign --force --deep --sign - "$app"

# hdiutil often fails with "Resource busy" on CI machines, so try again a
# few times.
for try in 1 2 3 4 5; do
  if hdiutil create -volname "Fractal+" -srcfolder "$app" -ov -format UDZO \
    fractalplus-macos-arm64.dmg; then
    break
  fi
  if [ "$try" = 5 ]; then
    exit 1
  fi
  echo "hdiutil failed, trying again"
  sleep $((try * 5))
done
echo "Packaged fractalplus-macos-arm64.dmg"
