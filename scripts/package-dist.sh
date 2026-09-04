#!/usr/bin/env bash
# Package a cross-compiled auto-scanner-ocr binary into release artifacts:
# a tarball, a .deb and an .rpm, named auto-scanner-ocr-<version>[-<suffix>]-<triple>.
#
# Usage: package-dist.sh <target-dir> <triple> <version> [--deb-arch A] [--rpm-arch A] [--suffix S]
set -euo pipefail

TARGET_DIR=$1
TRIPLE=$2
VERSION=$3
shift 3

DEB_ARCH=any
RPM_ARCH=noarch
SUFFIX=

while [ $# -gt 0 ]; do
    case $1 in
        --deb-arch) DEB_ARCH=$2; shift 2 ;;
        --rpm-arch) RPM_ARCH=$2; shift 2 ;;
        --suffix)   SUFFIX=$2; shift 2 ;;
        *) echo "unknown arg: $1" >&2; exit 1 ;;
    esac
done

PKG=auto-scanner-ocr
STAGE=$TRIPLE
DIST=dist
DESCRIPTION="Flatbed scan -> searchable OCR PDF, in a terminal UI"
MAINTAINER="Jonathan Stechmann <j-stechmann@users.noreply.github.com>"

BIN=$TARGET_DIR/$PKG
[ -x "$BIN" ] || { echo "binary not found: $BIN" >&2; exit 1; }

# The release profile strips already (strip = true); fall back to host strip
# for the rare foreign-arch case where the linker ignored it.
if [ -z "$(file -b "$BIN" | grep -i 'stripped')" ] 2>/dev/null; then
    strip "$BIN" 2>/dev/null || true
fi

rm -rf "$STAGE" "$STAGE.deb-root" "$DIST"
mkdir -p "$STAGE" "$DIST"

# ---------------------------------------------------------------- tarball ---
install -m 0755 "$BIN" "$STAGE/$PKG"
install -m 0644 README.md LICENSE config.toml "$STAGE/"
# config.toml is read from the current working directory (or
# ~/.config/auto-scanner-ocr/config.toml) — never next to the binary.
tar -czf "$DIST/$PKG-$VERSION${SUFFIX:+-$SUFFIX}-$TRIPLE.tar.gz" \
    --sort=name --owner=0 --group=0 --numeric-owner --mtime='1970-01-01' \
    -C "$STAGE" .
rm "$STAGE/$PKG" "$STAGE/README.md" "$STAGE/LICENSE" "$STAGE/config.toml"

# -------------------------------------------------------------------- deb ---
DEBROOT=$STAGE.deb-root
install -Dm 0755 "$BIN" "$DEBROOT/usr/bin/$PKG"
install -Dm 0644 config.toml "$DEBROOT/usr/share/doc/$PKG/examples/config.toml"
mkdir -p "$DEBROOT/DEBIAN"
cat > "$DEBROOT/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $DEB_ARCH
Maintainer: $MAINTAINER
Section: utils
Priority: optional
Depends: sane-utils, tesseract-ocr, ocrmypdf, img2pdf, poppler-utils
Recommends: hplip, unpaper, libnotify-bin, tesseract-ocr-deu, tesseract-ocr-script-latn
Description: $DESCRIPTION
 Flatbed scan -> searchable OCR PDF, via SANE + ocrmypdf. Linux-only TUI.
 .
 Config is read from ./config.toml (CWD) or
 ~/.config/auto-scanner-ocr/config.toml — see the examples copy.
EOF
dpkg-deb --build --root-owner-group "$DEBROOT" \
    "$DIST/$PKG-$VERSION${SUFFIX:+-$SUFFIX}-$TRIPLE.deb"

# -------------------------------------------------------------------- rpm ---
# Absolute binary path: %install runs with cwd=rpmbuild/BUILD, so relative
# paths from the repo root would not resolve.
BIN_ABS=$(readlink -f "$BIN")
# The package arch comes from BuildArch in the spec; no --target is passed.
# ("No compatible architectures found for build" is what Debian's rpm says
# for cross targets it has no platform config for — BuildArch avoids that
# machinery entirely; %install is arch-agnostic, it just copies a binary.)
cat > "$STAGE.spec" <<EOF
Name:           $PKG
Version:        $VERSION
Release:        1%{?dist}
Summary:        $DESCRIPTION
License:        GPL-2.0-or-later
URL:            https://github.com/j-stechmann/auto-scanner-ocr
BuildArch:      $RPM_ARCH
Requires:       sane-backends, tesseract, ocrmypdf, img2pdf, poppler-utils

%description
Flatbed scan -> searchable OCR PDF, via SANE + ocrmypdf. Linux-only TUI.
Config is read from ./config.toml (CWD) or ~/.config/auto-scanner-ocr/config.toml.

%install
install -Dm 0755 $BIN_ABS %{buildroot}/usr/bin/$PKG

%files
/usr/bin/$PKG
EOF
rpmbuild -bb --define "_topdir $PWD/rpmbuild" \
    --define "debug_package %{nil}" \
    --define "__os_install_post %{nil}" \
    --define "_target_cpu $RPM_ARCH" \
    --define "_target_platform ${RPM_ARCH}-linux-gnu" \
    --target "$RPM_ARCH" "$STAGE.spec" 2>/dev/null || \
rpmbuild -bb --define "_topdir $PWD/rpmbuild" \
    --define "debug_package %{nil}" \
    --define "__os_install_post %{nil}" \
    --define "_target_cpu $RPM_ARCH" \
    --define "_target_platform ${RPM_ARCH}-linux-gnu" \
    "$STAGE.spec"
mv "rpmbuild/RPMS/$RPM_ARCH/$PKG-$VERSION-1.$RPM_ARCH.rpm" \
    "$DIST/$PKG-$VERSION${SUFFIX:+-$SUFFIX}-$TRIPLE.rpm"

# ----------------------------------------------------------------- report ---
ls -l "$DIST"