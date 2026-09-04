#!/usr/bin/env bash
# Package a cross-compiled auto-scanner-ocr binary into release artifacts:
# a tarball, a .deb and an .rpm, named auto-scanner-ocr-<version>[-<suffix>]-<triple>.
#
# Usage: package-dist.sh <target-dir> <triple> <version> [--deb-arch A] [--rpm-arch A] [--rpm-machine M] [--suffix S]
set -euo pipefail

TARGET_DIR=$1
TRIPLE=$2
VERSION=$3
shift 3

DEB_ARCH=any
RPM_ARCH=noarch
RPM_MACHINE=      # docker --platform for QEMU-emulated native rpm builds
SUFFIX=

while [ $# -gt 0 ]; do
    case $1 in
        --deb-arch)    DEB_ARCH=$2; shift 2 ;;
        --rpm-arch)    RPM_ARCH=$2; shift 2 ;;
        --rpm-machine) RPM_MACHINE=$2; shift 2 ;;
        --suffix)      SUFFIX=$2; shift 2 ;;
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
# rpmbuild's arch check ("No compatible architectures found for build")
# is keyed on the *host* machine in every rpm (Ubuntu and Fedora alike),
# so cross-arch rpmbuild is impossible by design. For foreign targets we
# run rpmbuild natively inside a QEMU-emulated Fedora container of that
# arch (docker + binfmt on the runner); x86_64 stays on the host.
BIN_ABS=$(readlink -f "$BIN")
SPEC=$STAGE.spec
cat > "$SPEC" <<EOF
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

RPMDIR=$PWD/rpmbuild
if [ "$RPM_ARCH" = "x86_64" ]; then
    rpmbuild -bb --define "_topdir $RPMDIR" \
        --define "debug_package %{nil}" \
        --define "__os_install_post %{nil}" \
        --define "source_date_epoch_from_changelog %{nil}" \
        "$SPEC"
else
    # The spec embeds an absolute $BIN_ABS; the container sees the whole repo.
    docker run --rm --platform "$RPM_MACHINE" \
        -v "$PWD":/io -w /io fedora:41 bash -lc '
        dnf install -y rpm-build >/dev/null &&
        rpmbuild -bb --define "_topdir /io/rpmbuild" \
            --define "debug_package %{nil}" \
            --define "__os_install_post %{nil}" \
            --define "source_date_epoch_from_changelog %{nil}" \
            /io/'"$SPEC"
fi
mv "rpmbuild/RPMS/$RPM_ARCH/$PKG-$VERSION-1.$RPM_ARCH.rpm" \
    "$DIST/$PKG-$VERSION${SUFFIX:+-$SUFFIX}-$TRIPLE.rpm"

# ----------------------------------------------------------------- report ---
ls -l "$DIST"