# Maintainer: cesasol <cesasol@gitlab.com>
pkgname=comfyui-downloader-git
pkgver=r1.820762d
pkgrel=1
pkgdesc="CivitAI model downloader daemon for ComfyUI"
arch=('x86_64')
url="https://gitlab.com/cesasol/comfyui-downloader"
license=('MIT')
depends=('gcc-libs' 'glibc' 'libnotify')
makedepends=('cargo' 'rust')
provides=('comfyui-downloader' 'comfyui-dl')
conflicts=('comfyui-downloader')
source=("$pkgname::git+https://gitlab.com/cesasol/comfyui-downloader.git")
sha256sums=('SKIP')

pkgver() {
  cd "$pkgname"
  printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
  cd "$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$pkgname"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --release --frozen
}

package() {
  cd "$pkgname"
  install -Dm755 target/release/comfyui-downloader "$pkgdir/usr/bin/comfyui-downloader"
  install -Dm755 target/release/comfyui-dl         "$pkgdir/usr/bin/comfyui-dl"
  install -Dm644 systemd/comfyui-downloader.service \
    "$pkgdir/usr/lib/systemd/user/comfyui-downloader.service"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
