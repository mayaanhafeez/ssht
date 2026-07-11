# Maintainer: Ayaan Hafeez <m.ayaan.hafeez@gmail.com>
pkgname=ssht
pkgver=0.1.1
pkgrel=1
pkgdesc="Smart SSH session manager that auto-attaches to persistent tmux sessions"
arch=('x86_64' 'aarch64')
url="https://github.com/mayaanhafeez/ssht"
license=('MIT')
depends=('openssh' 'gcc-libs')
optdepends=('tmux: required on remote hosts you connect to')
makedepends=('cargo')

# No source= array on purpose: this PKGBUILD lives at the repo root and
# builds directly from the checkout it's part of (CI checks out the tagged
# commit; a human can `git clone` and run `makepkg` the same way). With an
# empty source array, makepkg treats srcdir as startdir, i.e. this directory.

build() {
  cargo build --release --locked
}

package() {
  install -Dm755 "target/release/ssht" "$pkgdir/usr/bin/ssht"
  install -Dm644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
}
