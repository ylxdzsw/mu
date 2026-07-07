pkgname=mu
pkgver=0.1.0
pkgrel=1
pkgdesc='Small composable agent runtime for the terminal'
arch=('x86_64')
url='https://github.com/ylxdzsw/mu'
license=('custom')
depends=('bash' 'jq' 'sqlite')
makedepends=('cargo' 'git')
source=("$pkgname::git+$url.git")
sha256sums=('SKIP')

pkgver() {
  cd "$pkgname"
  local _ver="$(grep -Po '^version\s*=\s*"\K[^"]*' Cargo.toml)"
  printf '%s.r%s.g%s' "$_ver" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

build() {
  export CARGO_HOME="$srcdir/cargo-home"
  export CARGO_TARGET_DIR="$srcdir/target"
  export CARGO_INCREMENTAL=0
  cargo build --manifest-path "$srcdir/$pkgname/Cargo.toml" --release
}

check() {
  export CARGO_HOME="$srcdir/cargo-home"
  export CARGO_TARGET_DIR="$srcdir/target"
  cargo test --manifest-path "$srcdir/$pkgname/Cargo.toml"
}

package() {
  install -Dm755 "$srcdir/target/release/mu" "$pkgdir/usr/bin/mu"
  install -Dm644 "$srcdir/$pkgname/mu.zsh" "$pkgdir/usr/share/mu/mu.zsh"
  install -Dm644 "$srcdir/$pkgname/README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 "$srcdir/$pkgname/SPEC.md" "$pkgdir/usr/share/doc/$pkgname/SPEC.md"
}
