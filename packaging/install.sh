#!/bin/sh
# openorion packaging installer. Nothing runs on source; everything is in main.
# Usage: sh packaging/install.sh
#
# Binaries go to ~/.local/bin. openoriond and openorion are static (musl): they need
# no system libraries. openorion-ui, openorion-jack and openorion-remote load the
# GL/Wayland/X11, JACK and ALSA libraries at run time, which a static musl binary
# cannot, so they are built for the host (glibc).
#
# Run from a checkout, it builds first. On a machine without the source, set
# OPENORION_BIN to a directory holding prebuilt openoriond, openorion, openorion-ui,
# openorion-jack and openorion-remote; the build is then skipped.
set -eu

main() {
    here=$(cd "$(dirname "$0")" && pwd)
    cfg=${XDG_CONFIG_HOME:-$HOME/.config}
    data=${XDG_DATA_HOME:-$HOME/.local/share}
    bin=$HOME/.local/bin

    if [ -n "${OPENORION_BIN:-}" ]; then
        echo "==> 1/7 prebuilt binaries from $OPENORION_BIN"
        static=$OPENORION_BIN
        host=$OPENORION_BIN
    else
        echo "==> 1/7 build (static openoriond + openorion; host openorion-ui, -jack, -remote)"
        root=$(cd "$here/.." && pwd)
        cargo build --manifest-path "$root/Cargo.toml" --release \
            --target x86_64-unknown-linux-musl -p openoriond -p openorion-cli
        cargo build --manifest-path "$root/Cargo.toml" --release -p openorion-ui
        cargo build --manifest-path "$root/Cargo.toml" --release -p openorion-jack --features jack
        cargo build --manifest-path "$root/Cargo.toml" --release -p openorion-remote
        static=$root/target/x86_64-unknown-linux-musl/release
        host=$root/target/release
    fi

    echo "==> 2/7 install binaries to $bin"
    mkdir -p "$bin"
    install -m 0755 "$static/openoriond" "$static/openorion" "$bin/"
    install -m 0755 "$host/openorion-ui" "$host/openorion-jack" "$host/openorion-remote" "$bin/"

    echo "==> 3/7 install udev rule (sudo)"
    sudo install -m 0644 "$here/60-openorion.rules" /etc/udev/rules.d/60-openorion.rules

    echo "==> 4/7 reload udev rules (sudo)"
    sudo udevadm control --reload

    echo "==> 5/7 re-apply rules to the Orion only (sudo)"
    # attr-match checks the device's own attributes, so it fits the USB node
    # but not hidraw (the vendor id lives on its parent). hidraw nodes are
    # few, so that subsystem is triggered as a whole in a second call.
    sudo udevadm trigger --action=add --subsystem-match=usb \
        --attr-match=idVendor=23e5 --attr-match=idProduct=a210
    sudo udevadm trigger --action=add --subsystem-match=hidraw

    echo "==> 6/7 install systemd user units"
    mkdir -p "$cfg/systemd/user"
    install -m 0644 "$here/systemd/openoriond.service" "$cfg/systemd/user/openoriond.service"
    install -m 0644 "$here/systemd/openorion-remote.service" \
        "$cfg/systemd/user/openorion-remote.service"
    echo "    enable with: systemctl --user daemon-reload && systemctl --user enable --now openoriond"
    echo "    MIDI/OSC control (optional): systemctl --user enable --now openorion-remote"

    echo "==> 7/7 install desktop file"
    mkdir -p "$data/applications"
    install -m 0644 "$here/openorion.desktop" "$data/applications/openorion.desktop"

    case ":$PATH:" in
        *":$bin:"*) ;;
        *) echo "note: $bin is not in PATH" ;;
    esac
    echo "done. Without a local seat session, join the audio group instead:"
    echo "    sudo usermod -aG audio \$USER   (then log in again)"
}

main "$@"
