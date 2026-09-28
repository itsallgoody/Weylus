#!/usr/bin/env sh

set -ex

# cross compile windows version
cargo build --target x86_64-pc-windows-gnu --release

# weylus.exe ships alone in the zip: it must not need a MinGW runtime DLL (libvpl brings in C++)
WIN_EXE=target/x86_64-pc-windows-gnu/release/weylus.exe
x86_64-w64-mingw32-objdump -p "$WIN_EXE" | grep 'DLL Name' | sort -u
if x86_64-w64-mingw32-objdump -p "$WIN_EXE" | grep -iE 'DLL Name: (libstdc|libgcc|libwinpthread)'; then
    echo "weylus.exe imports a MinGW runtime DLL; link it statically"
    exit 1
fi

# cleanup cross compiled windows artifacts
(cd deps && ./clean.sh)

# build linux versions
cargo deb --  --features=va-static

# check if installing works
dpkg -i target/debian/Weylus*.deb
cp target/release/weylus target/release/weylus_va_static

# build version with dynamic libva
cargo build --release

mkdir packages

PKGDIR="$PWD/packages"

# package windows
(
  cd target/x86_64-pc-windows-gnu/release/
  zip weylus-windows.zip weylus.exe
  mv weylus-windows.zip "$PKGDIR/"
)

# package linux
(
  cp target/debian/Weylus*.deb "$PKGDIR/"
  cp weylus.desktop target/release/
  cd target/release/
  zip weylus-linux.zip weylus weylus_va_static weylus.desktop
  mv weylus-linux.zip "$PKGDIR/"
)
