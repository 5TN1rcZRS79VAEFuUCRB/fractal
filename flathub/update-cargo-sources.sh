#!/usr/bin/env bash
#
# Regenerate fractalplus-cargo-sources.json from Cargo.lock, for the Flathub
# manifest. Run it from the repository root after changing dependencies.
#
# Dependencies: git, python3 with the aiohttp and tomlkit modules.
set -euo pipefail

TOOLS_DIR="$(mktemp -d)"
trap 'rm -rf "$TOOLS_DIR"' EXIT
git clone --depth 1 https://github.com/flatpak/flatpak-builder-tools.git "$TOOLS_DIR"
python3 "$TOOLS_DIR/cargo/flatpak-cargo-generator.py" Cargo.lock \
    -o flathub/fractalplus-cargo-sources.json
