# Maintainer: noahlyk <noahlykins@gmail.com>
pkgname=keymux
pkgver=1.7.0
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
sha256sums=()

build() {
    cd "$startdir"
    cargo build --release --locked
}

package() {
    cd "$startdir"
    install -Dm755 "target/release/keymux" "$pkgdir/usr/bin/keymux"

    # Service/init files are shipped unconditionally for every supported init
    # system and WM, since package() runs in the build environment (which may
    # be a clean chroot) rather than the end user's machine - detecting the
    # target init system/WM has to happen at install time in keymux.install,
    # not here.
    install -Dm644 "systemd/keymux.service" "$pkgdir/usr/lib/systemd/system/keymux.service"
    install -Dm644 "systemd/keymux-niri.service" "$pkgdir/usr/lib/systemd/user/keymux-niri.service"
    install -Dm644 "systemd/keymux-hyprland.service" "$pkgdir/usr/lib/systemd/user/keymux-hyprland.service"
    install -Dm644 "systemd/keymux-sway.service" "$pkgdir/usr/lib/systemd/user/keymux-sway.service"
    install -Dm644 "systemd/keymux-i3.service" "$pkgdir/usr/lib/systemd/user/keymux-i3.service"
    install -Dm644 "systemd/keymux-bspwm.service" "$pkgdir/usr/lib/systemd/user/keymux-bspwm.service"

    install -Dm755 "openrc/keymux" "$pkgdir/etc/init.d/keymux"
    install -Dm755 "openrc/keymux-niri" "$pkgdir/etc/init.d/keymux-niri"
    install -Dm755 "openrc/keymux-hyprland" "$pkgdir/etc/init.d/keymux-hyprland"
    install -Dm755 "openrc/keymux-sway" "$pkgdir/etc/init.d/keymux-sway"
    install -Dm755 "openrc/keymux-i3" "$pkgdir/etc/init.d/keymux-i3"
    install -Dm755 "openrc/keymux-bspwm" "$pkgdir/etc/init.d/keymux-bspwm"

    for sv in keymux keymux-niri keymux-hyprland keymux-sway keymux-i3 keymux-bspwm; do
        cp -r "runit/$sv" "$pkgdir/etc/sv/$sv"
        chmod 755 "$pkgdir/etc/sv/$sv/run" "$pkgdir/etc/sv/$sv/log/run"
    done

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
