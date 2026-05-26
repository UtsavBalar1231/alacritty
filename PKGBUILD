# Maintainer: Utsav Balar <utsav@local>

pkgname=alacritty-git
_pkgname=alacritty
_srcname=alacritty-src
pkgver=0.18.0.dev.r2501.g62d2aec
pkgrel=1
pkgdesc="A cross-platform, GPU-accelerated terminal emulator with local patches"
arch=('x86_64')
url="https://github.com/alacritty/alacritty"
license=('Apache-2.0' 'MIT')
depends=(
  'fontconfig'
  'freetype2'
  'libxcursor'
  'libxi'
  'libxkbcommon'
  'libxkbcommon-x11'
  'libxrandr'
)
makedepends=(
  'cargo'
  'cmake'
  'desktop-file-utils'
  'git'
  'libxcb'
  'ncurses'
  'rust'
  'scdoc'
)
checkdepends=('ttf-dejavu')
optdepends=('ncurses: for the alacritty terminfo database')
provides=('alacritty')
conflicts=('alacritty')
source=("${_srcname}::git+file://${ALACRITTY_SRC:-$PWD}")
sha256sums=('SKIP')

pkgver() {
  cd "${_srcname}"

  local version revision commit
  version="$(awk -F '"' '/^version = / { print $2; exit }' alacritty/Cargo.toml | tr '-' '.')"
  revision="$(git rev-list --count HEAD)"
  commit="$(git rev-parse --short=7 HEAD)"

  printf '%s.r%s.g%s' "${version}" "${revision}" "${commit}"
}

prepare() {
  cd "${_srcname}"

  local host
  host="$(rustc -vV | sed -n 's/^host: //p')"
  cargo fetch --locked --target "${host}"
}

build() {
  cd "${_srcname}"

  CARGO_INCREMENTAL=0 cargo build --release --locked --offline -p alacritty
}

check() {
  cd "${_srcname}"

  CARGO_INCREMENTAL=0 cargo test --release --locked --offline -p alacritty
}

package() {
  cd "${_srcname}"

  desktop-file-install -m 644 --dir "${pkgdir}/usr/share/applications" \
    extra/linux/Alacritty.desktop

  install -Dm755 target/release/alacritty "${pkgdir}/usr/bin/alacritty"

  scdoc < extra/man/alacritty.1.scd | install -Dm644 /dev/stdin \
    "${pkgdir}/usr/share/man/man1/alacritty.1"
  scdoc < extra/man/alacritty-msg.1.scd | install -Dm644 /dev/stdin \
    "${pkgdir}/usr/share/man/man1/alacritty-msg.1"
  scdoc < extra/man/alacritty.5.scd | install -Dm644 /dev/stdin \
    "${pkgdir}/usr/share/man/man5/alacritty.5"
  scdoc < extra/man/alacritty-bindings.5.scd | install -Dm644 /dev/stdin \
    "${pkgdir}/usr/share/man/man5/alacritty-bindings.5"
  scdoc < extra/man/alacritty-escapes.7.scd | install -Dm644 /dev/stdin \
    "${pkgdir}/usr/share/man/man7/alacritty-escapes.7"

  install -Dm644 extra/linux/org.alacritty.Alacritty.appdata.xml \
    "${pkgdir}/usr/share/metainfo/org.alacritty.Alacritty.appdata.xml"
  install -Dm644 extra/completions/alacritty.bash \
    "${pkgdir}/usr/share/bash-completion/completions/alacritty"
  install -Dm644 extra/completions/_alacritty \
    "${pkgdir}/usr/share/zsh/site-functions/_alacritty"
  install -Dm644 extra/completions/alacritty.fish \
    "${pkgdir}/usr/share/fish/vendor_completions.d/alacritty.fish"
  install -Dm644 extra/logo/compat/alacritty-term.svg \
    "${pkgdir}/usr/share/pixmaps/Alacritty.svg"

  install -Dm644 LICENSE-MIT "${pkgdir}/usr/share/licenses/${pkgname}/LICENSE-MIT"
  install -Dm644 LICENSE-APACHE "${pkgdir}/usr/share/licenses/${pkgname}/LICENSE-APACHE"
}
