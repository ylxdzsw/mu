# Packages the current checkout. This is convenient for local `makepkg` use from
# the project root; publishing to the AUR would still need a declared license,
# remote sources, and likely a distinct package name.

pkgname=mu
pkgver=0.1.0
pkgrel=1
pkgdesc='Small composable agent runtime for the terminal'
arch=('x86_64')
url='https://github.com/ylxdzsw/mu'
license=('custom')
depends=('bash')
makedepends=('cargo')
source=()
sha256sums=()

build() {
  export CARGO_HOME="$srcdir/cargo-home"
  export CARGO_TARGET_DIR="$srcdir/target"
  cargo build --manifest-path "$startdir/Cargo.toml" --release
}

check() {
  export CARGO_HOME="$srcdir/cargo-home"
  export CARGO_TARGET_DIR="$srcdir/target"
  cargo test --manifest-path "$startdir/Cargo.toml"
}

package() {
  install -Dm755 "$srcdir/target/release/mu" "$pkgdir/usr/bin/mu"
  install -Dm644 "$startdir/mu.zsh" "$pkgdir/usr/share/mu/mu.zsh"
  install -Dm644 "$startdir/README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 "$startdir/SPEC.md" "$pkgdir/usr/share/doc/$pkgname/SPEC.md"
}
