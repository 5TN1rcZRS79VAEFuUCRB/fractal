# Fractal+

An unofficial fork of [Fractal](https://gitlab.gnome.org/World/fractal), the GNOME Matrix app,
that adds voice and video calls and screen sharing. It installs next to Fractal without replacing it.

Calls work with Element Call, so you can call people who use Element.

## Features

- ✅ Encrypted group calls in any room
- 🧪 Camera video, and other people's cameras and shared screens (new)
- 🧪 Screen sharing with sound from your apps (new)
- Click a video to open it in its own window, which can go full screen (F11 or double-click)

## Install

On any Linux distribution with [Flatpak](https://flathub.org/setup):

```sh
flatpak install --from https://5tn1rczrs79vaefuucrb.github.io/fractalplus/fractalplus.flatpakref
```

Or download the `.flatpak` file from the [latest release](../../releases) and open it.
Either way, Fractal+ then updates with `flatpak update` or in your software app.

### Windows (test build)

Download `fractalplus-windows-x86_64-setup.exe` from the [latest release](../../releases) and run
it. It installs Fractal+ for your user only, with a Start menu entry, and doesn't need
administrator rights. To update, run the setup of a newer version: Fractal+ tells you when one is
out. The installer isn't signed, so Windows may show "Windows protected your PC": click
**More info**, then **Run anyway**.

If you'd rather not install anything, the `fractalplus-windows-x86_64.zip` has the same program:
unzip it anywhere and run `Fractal+\bin\fractalplus.exe`.

### macOS (test build, Apple silicon)

Download `fractalplus-macos-arm64.dmg` from the [latest release](../../releases), open it and
drag Fractal+ to Applications. The app isn't signed by Apple, so the first time, open it, click
**Done** on the warning, then go to **System Settings → Privacy & Security** and click
**Open Anyway** next to Fractal+.

On Windows and macOS, screen sharing shares your whole main screen, and sharing sound from apps
only works on Windows. Locations in messages show without a map on Windows.

## Requirements

Your homeserver needs a call server set up (LiveKit with lk-jwt-service, the same setup Element
Call uses).

## Build from source

```sh
meson setup _build
ninja -C _build
sudo ninja -C _build install
fractalplus
```

## Problems

Report problems with calls [here](../../issues), not to the Fractal project.

## License

GPL-3.0-or-later, like Fractal. Everything except calls is the work of the Fractal developers.
