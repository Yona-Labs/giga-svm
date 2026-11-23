#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || $1 != /* ]]; then
  echo "Usage: $0 <absolute installation path>" >&2
  exit 1
fi

install_dir="$1"
if [[ -e "$install_dir" || -L "$install_dir" ]]; then
  echo "OpenSSL installation path already exists: $install_dir" >&2
  exit 1
fi

version=3.5.9
checksum=603f5602e2eef00d77fbd429d34dcd5822bb301757a1bc9cdb24c670f1eb859a
archive="openssl-$version.tar.gz"
work_dir="$(mktemp -d "$(dirname "$install_dir")/.openssl.XXXXXXXX")"
trap 'rm -rf -- "$work_dir"' EXIT

cd "$work_dir"
curl --fail --silent --show-error --location --retry 3 \
  "https://github.com/openssl/openssl/releases/download/openssl-$version/$archive" \
  --output "$archive"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check
tar -xzf "$archive"

cd "openssl-$version"
./Configure no-shared no-tests --prefix="$install_dir" --openssldir=/etc/ssl --libdir=lib
make -j "$(getconf _NPROCESSORS_ONLN)" build_sw
make DESTDIR="$work_dir/staging" install_sw
mv -T -- "$work_dir/staging$install_dir" "$install_dir"
"$install_dir/bin/openssl" version
