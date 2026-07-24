pkgname=mu
pkgver=0.1.0
pkgrel=1
pkgdesc='Small composable agent runtime for the terminal'
arch=('x86_64')
url='https://github.com/ylxdzsw/mu'
license=('MIT')
depends=('bash' 'jq' 'sqlite')
makedepends=('cargo' 'git')
options=('!lto')
_source_url="${MU_SOURCE_URL:-$url.git}"
_source_branch="${MU_SOURCE_BRANCH:-master}"
source=("$pkgname::git+$_source_url#branch=$_source_branch")
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
  cargo test --manifest-path "$srcdir/$pkgname/Cargo.toml" -- --test-threads=1
}

package() {
  install -Dm755 "$srcdir/target/release/mu" "$pkgdir/usr/bin/mu"
  install -dm755 "$pkgdir/usr/libexec/mu"
  ln -s ../../bin/mu "$pkgdir/usr/libexec/mu/apply_patch"
  ln -s ../../bin/mu "$pkgdir/usr/libexec/mu/edit"
  ln -s ../../bin/mu "$pkgdir/usr/libexec/mu/view_image"
  install -Dm644 "$srcdir/$pkgname/mu.zsh" "$pkgdir/usr/share/zsh/plugins/mu/mu.zsh"
  install -dm755 "$pkgdir/usr/share/mu"
  cp -a "$srcdir/$pkgname/builtins/." "$pkgdir/usr/share/mu/"
  install -Dm644 "$srcdir/$pkgname/README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 "$srcdir/$pkgname/SPEC.md" "$pkgdir/usr/share/doc/$pkgname/SPEC.md"
  install -Dm644 "$srcdir/$pkgname/LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
