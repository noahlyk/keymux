# Maintainer: fibsussy <fibsussy@tuta.io>
pkgname=keymux
pkgver=1.3.2
pkgrel=1
pkgdesc="Keyboard middleware for gaming with low-level input interception"
arch=('x86_64' 'aarch64')
url="https://github.com/fibsussy/keymux"
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
sha256sums=()

build() {
    cd "$startdir"
    cargo build --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/keymux" "$pkgdir/usr/bin/keymux"

    # Install systemd services (if systemd is detected)
    if [ -d "/run/systemd/system" ]; then
        install -Dm644 "systemd/keymux.service" "$pkgdir/usr/lib/systemd/system/keymux.service"

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
