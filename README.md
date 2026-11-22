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
