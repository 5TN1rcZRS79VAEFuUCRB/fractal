# Fractal+

An unofficial fork of [Fractal](https://gitlab.gnome.org/World/fractal), the GNOME Matrix app,
that adds voice calls and screen sharing. It installs next to Fractal without replacing it.

Calls work with Element Call, so you can call people who use Element.

## Features

- ✅ Encrypted group voice calls in any room
- 🧪 Screen sharing with sound from your apps (new, not well tested yet)
- 🚧 Camera video

## Install

Download the `.flatpak` file from the [latest release](../../releases) and open it, or run:

```sh
flatpak install fractalplus-*-x86_64.flatpak
```

You need Flatpak with the Flathub remote set up: <https://flathub.org/setup>

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
