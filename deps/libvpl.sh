#!/usr/bin/env bash

# Intel's oneVPL dispatcher (libvpl), static, for the mingw cross build: FFmpeg's
# h264_qsv, scale_qsv and the QSV hwcontext need it. Modeled on
# BtbN/FFmpeg-Builds scripts.d/50-onevpl.sh.

set -ex

cd libvpl

# intel/libvpl#198 (the patch BtbN applies): the wcscpy_s/wcscat_s fallback defines are
# meant for old MSVC, but _MSC_VER is undefined on mingw, and mingw-w64 12+ headers break
# on them. Older mingw-w64 (Debian bullseye has 8) builds fine with them, and may not
# declare wcscpy_s at all, so the patch goes in only where it is needed.
MINGW_MAJOR=$(printf '#include <_mingw.h>\n__MINGW64_VERSION_MAJOR\n' | "${CROSS_COMPILE}gcc" -E -x c - | tail -n 1)
if [ "$MINGW_MAJOR" -ge 12 ] 2>/dev/null; then
    LIBVPL_PATCH=applied
    sed -i -e 's/^#if _MSC_VER < 1400$/#if defined(_MSC_VER) \&\& _MSC_VER < 1400/' \
        libvpl/src/windows/mfx_dispatcher_defs.h
else
    LIBVPL_PATCH=skipped
fi
echo "libvpl: mingw-w64 headers v$MINGW_MAJOR, wcscpy_s patch $LIBVPL_PATCH"

rm -rf build
mkdir build
cd build

cmake .. \
    -DCMAKE_SYSTEM_NAME=Windows \
    -DCMAKE_C_COMPILER="${CROSS_COMPILE}gcc" \
    -DCMAKE_CXX_COMPILER="${CROSS_COMPILE}g++" \
    -DCMAKE_RC_COMPILER="${CROSS_COMPILE}windres" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$DIST" \
    -DCMAKE_INSTALL_LIBDIR=lib \
    -DCMAKE_INSTALL_BINDIR=bin \
    -DCMAKE_INSTALL_INCLUDEDIR=include \
    -DBUILD_SHARED_LIBS=OFF \
    -DBUILD_TESTS=OFF \
    -DBUILD_EXAMPLES=OFF \
    -DINSTALL_EXAMPLES=OFF

make -j$NPROCS
make install

# libvpl is C++. FFmpeg's configure links its libvpl test without --static, so it never
# reads Libs.private (where BtbN puts -lstdc++): the C++ runtime has to be on Libs.
sed -i -e 's/^Libs: \(.*\)$/Libs: \1 -lstdc++/' "$DIST/lib/pkgconfig/vpl.pc"
cat "$DIST/lib/pkgconfig/vpl.pc"
