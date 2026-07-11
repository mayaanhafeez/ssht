# Maintainer: Ayaan Hafeez <m.ayaan.hafeez@gmail.com>
pkgname=ssht
pkgver=0.1.3
pkgrel=1
pkgdesc="Smart SSH session manager that auto-attaches to persistent tmux sessions"
arch=('x86_64' 'aarch64')
url="https://github.com/mayaanhafeez/ssht"
license=('MIT')
depends=('openssh' 'gcc-libs')
optdepends=('tmux: required on remote hosts you connect to')
makedepends=('cargo')

# rusqlite's "bundled" feature compiles sqlite3.c via the cc crate, which
# inherits makepkg's CFLAGS. Arch injects -flto=auto there by default, which
# produces thin LTO objects with no usable symbols for Rust's (non-LTO) final
# link -- every sqlite3_* symbol then comes back undefined. Disabling LTO for
# this package keeps the bundled C object a normal, linkable archive.
# !debug: don't split out a separate ssht-debug package for the release.
options=('!lto' '!debug')

# No source= array on purpose: this PKGBUILD lives at the repo root and builds
# directly from the checkout it's part of (CI checks out the tagged commit; a
# human can `git clone` and run `makepkg` the same way). makepkg runs the
# functions below in $srcdir (= $startdir/src) -- which here collides with the
# project's own Rust src/ dir -- so both functions cd to $startdir (the repo
# root, where Cargo.toml lives) to build and locate the binary reliably.

build() {
  cd "$startdir"
  cargo build --release --locked
}

package() {
  cd "$startdir"
  install -Dm755 "target/release/ssht" "$pkgdir/usr/bin/ssht"
  install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
}
