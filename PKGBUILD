# Maintainer: noahlyk <noahlykins@gmail.com>
pkgname=keymux
pkgver=1.14.1
pkgrel=1
pkgdesc="Keyboard middleware for gaming with low-level input interception"
arch=('x86_64' 'aarch64')
url="https://github.com/noahlyk/keymux"
license=('MIT')
depends=('udev' 'libevdev')
makedepends=('rust' 'cargo')
optdepends=('systemd: for systemd service files (or use OpenRC/runit scripts)'
            'openrc: for OpenRC init scripts'
            'runit: for runit service directories'
            'niri: automatic game mode detection in Niri compositor'
            'hyprland: automatic game mode detection in Hyprland compositor'
            'sway: automatic game mode detection in Sway compositor'
            'i3-wm: automatic game mode detection in i3 window manager'
            'bspwm: automatic game mode detection in bspwm window manager')
options=('!debug')
install=keymux.install

source=()

# Local builds from a git checkout report the tag, commit count and hash, the same as `make`:
#   1.10.0+2.gbbd181e, or 1.10.0+2.gbbd181e.dirty with uncommitted changes.
# Anywhere without git (or on the release tag) the version above is used.
pkgver() {
    cd "$startdir"
    git rev-parse --git-dir >/dev/null 2>&1 || { echo "$pkgver"; return; }

    local version meta=() described dirty
    if described=$(git describe --tags --long --match 'v[0-9]*' 2>/dev/null); then
        local rest="${described%-g*}"
        version="${rest%-*}"
        version="${version#v}"
        local count="${rest##*-}"
        (( count > 0 )) && meta+=("$count.g${described##*-g}")
    else
        version="0.0.0"
        meta+=("g$(git rev-parse --short HEAD)")
    fi
    [ -n "$(git status --porcelain --untracked-files=no)" ] && meta+=(dirty)

    if (( ${#meta[@]} )); then
        local IFS=.
        version+="+${meta[*]}"
    fi
    echo "$version"
}
sha256sums=()

build() {
    cd "$startdir"
    KEYMUX_VERSION="$pkgver" cargo build --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/keymux" "$pkgdir/usr/bin/keymux"

    # Install systemd services (if systemd is detected)
    if [ -d "/run/systemd/system" ]; then
        install -Dm644 "systemd/keymux.service" "$pkgdir/usr/lib/systemd/system/keymux.service"
        # Steno overlay and text box reader: any Wayland compositor with layer-shell or the
        # input method protocol, so not tied to one
        install -Dm644 "systemd/keymux-steno.service" "$pkgdir/usr/lib/systemd/user/keymux-steno.service"

        if pacman -Qq niri &>/dev/null; then
            install -Dm644 "systemd/keymux-niri.service" "$pkgdir/usr/lib/systemd/user/keymux-niri.service"
        fi
        if pacman -Qq hyprland &>/dev/null; then
            install -Dm644 "systemd/keymux-hyprland.service" "$pkgdir/usr/lib/systemd/user/keymux-hyprland.service"
        fi
        if pacman -Qq sway &>/dev/null; then
            install -Dm644 "systemd/keymux-sway.service" "$pkgdir/usr/lib/systemd/user/keymux-sway.service"
        fi
        if pacman -Qq i3-wm &>/dev/null; then
            install -Dm644 "systemd/keymux-i3.service" "$pkgdir/usr/lib/systemd/user/keymux-i3.service"
        fi
        if pacman -Qq bspwm &>/dev/null; then
            install -Dm644 "systemd/keymux-bspwm.service" "$pkgdir/usr/lib/systemd/user/keymux-bspwm.service"
        fi
    fi

    # Install OpenRC scripts (if OpenRC is detected)
    if [ -d "/etc/openrc" ] || [ -d "/etc/init.d" ]; then
        install -Dm755 "openrc/keymux" "$pkgdir/etc/init.d/keymux"
        
        if pacman -Qq niri &>/dev/null; then
            install -Dm755 "openrc/keymux-niri" "$pkgdir/etc/init.d/keymux-niri"
        fi
        if pacman -Qq hyprland &>/dev/null; then
            install -Dm755 "openrc/keymux-hyprland" "$pkgdir/etc/init.d/keymux-hyprland"
        fi
        if pacman -Qq sway &>/dev/null; then
            install -Dm755 "openrc/keymux-sway" "$pkgdir/etc/init.d/keymux-sway"
        fi
        if pacman -Qq i3-wm &>/dev/null; then
            install -Dm755 "openrc/keymux-i3" "$pkgdir/etc/init.d/keymux-i3"
        fi
        if pacman -Qq bspwm &>/dev/null; then
            install -Dm755 "openrc/keymux-bspwm" "$pkgdir/etc/init.d/keymux-bspwm"
        fi
    fi

    # Install runit service directories (if runit is detected)
    if [ -d "/etc/runit" ] || [ -d "/service" ]; then
        cp -r "runit/keymux" "$pkgdir/etc/sv/keymux"
        chmod 755 "$pkgdir/etc/sv/keymux/run" "$pkgdir/etc/sv/keymux/log/run"
        
        if pacman -Qq niri &>/dev/null; then
            cp -r "runit/keymux-niri" "$pkgdir/etc/sv/keymux-niri"
            chmod 755 "$pkgdir/etc/sv/keymux-niri/run" "$pkgdir/etc/sv/keymux-niri/log/run"
        fi
        if pacman -Qq hyprland &>/dev/null; then
            cp -r "runit/keymux-hyprland" "$pkgdir/etc/sv/keymux-hyprland"
            chmod 755 "$pkgdir/etc/sv/keymux-hyprland/run" "$pkgdir/etc/sv/keymux-hyprland/log/run"
        fi
        if pacman -Qq sway &>/dev/null; then
            cp -r "runit/keymux-sway" "$pkgdir/etc/sv/keymux-sway"
            chmod 755 "$pkgdir/etc/sv/keymux-sway/run" "$pkgdir/etc/sv/keymux-sway/log/run"
        fi
        if pacman -Qq i3-wm &>/dev/null; then
            cp -r "runit/keymux-i3" "$pkgdir/etc/sv/keymux-i3"
            chmod 755 "$pkgdir/etc/sv/keymux-i3/run" "$pkgdir/etc/sv/keymux-i3/log/run"
        fi
        if pacman -Qq bspwm &>/dev/null; then
            cp -r "runit/keymux-bspwm" "$pkgdir/etc/sv/keymux-bspwm"
            chmod 755 "$pkgdir/etc/sv/keymux-bspwm/run" "$pkgdir/etc/sv/keymux-bspwm/log/run"
        fi
    fi

    install -Dm644 "config.example.ron" "$pkgdir/usr/share/doc/keymux/config.example.ron"
    install -Dm644 "README.md" "$pkgdir/usr/share/doc/keymux/README.md"
    install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/keymux/LICENSE"
    
    # Static shell completions generated at build time
    local _keymux="$startdir/target/release/keymux"
    
    # Fish
    install -dm755 "$pkgdir/usr/share/fish/vendor_completions.d"
    "$_keymux" completion fish > "$pkgdir/usr/share/fish/vendor_completions.d/keymux.fish"
    
    # Bash
    install -dm755 "$pkgdir/usr/share/bash-completion/completions"
    "$_keymux" completion bash > "$pkgdir/usr/share/bash-completion/completions/keymux"
    
    # Zsh
    install -dm755 "$pkgdir/usr/share/zsh/site-functions"
    "$_keymux" completion zsh > "$pkgdir/usr/share/zsh/site-functions/_keymux"
    
    install -dm755 "$pkgdir/etc/skel/.config/keymux"
}
