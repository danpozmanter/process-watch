#!/usr/bin/env bash
# Build pw and package it for the host platform into OUT (default dist):
#   Linux    process-watch-VERSION-linux-ARCH.tar.gz, a static (musl) pw
#   FreeBSD  process-watch-VERSION-freebsd-ARCH.tar.gz
#   macOS    process-watch-VERSION-macos-ARCH.tar.gz
#   Windows  process-watch-VERSION-windows-ARCH.zip, and a -portable.zip whose
#            data folder keeps the recordings beside pw.exe
# A tarball holds pw, install.sh, the service files and the docs.
# With --installers it also builds the release's installers:
#   Linux    process-watch_VERSION-1_ARCH.deb and process-watch-VERSION-1.ARCH.rpm
#            (dpkg-deb, rpmbuild)
#   Windows  process-watch-VERSION-ARCH-setup.exe (Inno Setup 6)
# Each gets a .sha256 beside it, and its path is printed.
#
# Usage: scripts/package.sh [--installers] [OUT]
#
# MACOSX_DEPLOYMENT_TARGET names the oldest macOS the binary runs on.
set -euo pipefail

installers=0
if [ "${1:-}" = --installers ]; then
    installers=1
    shift
fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$(mkdir -p "${1:-dist}" && cd "${1:-dist}" && pwd)"

die() { echo "error: $*" >&2; exit 1; }
need() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die "packaging needs $tool on the PATH"
    done
}

name=process-watch
summary="Top programs by RAM or CPU over the past hours"
description="pw records every process's CPU and memory and reports the programs that used the most, with subprocesses rolled into the program that ran them."
maintainer="process-watch contributors"
homepage="https://github.com/danpozmanter/process-watch"
version="$(awk -F'"' '/^version = / { print $2; exit }' "$root/Cargo.toml")"
[ -n "$version" ] || die "no version in Cargo.toml"

case "$(uname -s)" in
    Linux) platform=linux ;;
    FreeBSD) platform=freebsd ;;
    Darwin) platform=macos ;;
    MINGW* | MSYS* | CYGWIN*) platform=windows ;;
    *) die "unsupported OS: $(uname -s)" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) die "unsupported architecture: $(uname -m)" ;;
esac
base="$name-$version-$platform-$arch"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

# Linux builds against musl, statically, so one binary runs on every
# distribution whatever its C library.
build_args=(--release --locked --manifest-path "$root/Cargo.toml")
binary="$root/target/release/pw"
if [ "$platform" = linux ]; then
    target="$arch-unknown-linux-musl"
    build_args+=(--target "$target")
    binary="$root/target/$target/release/pw"
fi
[ "$platform" = windows ] && binary="$binary.exe"
need cargo
cargo build "${build_args[@]}" >&2
[ -f "$binary" ] || die "the build did not produce $binary"
# A dynamically linked binary names its loader in a PT_INTERP header.
if [ "$platform" = linux ]; then
    need readelf
    if readelf -l "$binary" | grep -q INTERP; then
        die "$binary needs a dynamic loader; it should be static"
    fi
fi

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1"
    else
        sha256 -r "$1"
    fi
}

# Writes FILE's checksum beside it and names it on stdout.
publish() {
    (cd "$out" && sha256 "$(basename "$1")" > "$(basename "$1").sha256")
    echo "$1"
}

# Zips FOLDER as one top-level folder; ZipFile keeps empty folders, which
# Compress-Archive drops. PowerShell 7's .NET names entries with the forward
# slashes zip readers expect, where Windows PowerShell's writes backslashes.
zip_folder() {
    need pwsh cygpath
    rm -f "$2"
    pwsh -NoProfile -Command "Add-Type -AssemblyName System.IO.Compression.FileSystem; [System.IO.Compression.ZipFile]::CreateFromDirectory('$(cygpath -w "$1")', '$(cygpath -w "$2")', [System.IO.Compression.CompressionLevel]::Optimal, \$true)"
}

docs() {
    cp "$root/README.md" "$root/LICENSE" "$root/CHANGELOG.md" "$1/"
}

# The systemd user unit for a pw installed as /usr/bin/pw.
packaged_unit() {
    sed 's|^ExecStart=.*|ExecStart=/usr/bin/pw record|' "$root/contrib/systemd/process-watch.service"
}

# The tree a distribution package installs under /usr.
usr_tree() {
    local usr="$1"
    install -d "$usr/bin" "$usr/lib/systemd/user" "$usr/share/doc/$name"
    install -m755 "$binary" "$usr/bin/pw"
    packaged_unit > "$usr/lib/systemd/user/process-watch.service"
    chmod 644 "$usr/lib/systemd/user/process-watch.service"
    install -m644 "$root/README.md" "$root/CHANGELOG.md" "$usr/share/doc/$name/"
    install -m644 "$root/LICENSE" "$usr/share/doc/$name/copyright"
}

deb_package() {
    local pkg="$stage/deb" deb_arch file
    need dpkg-deb
    case "$arch" in
        x86_64) deb_arch=amd64 ;;
        aarch64) deb_arch=arm64 ;;
    esac
    usr_tree "$pkg/usr"
    mkdir -p "$pkg/DEBIAN"
    # pw is static, so the package depends on nothing.
    cat > "$pkg/DEBIAN/control" <<CONTROL
Package: $name
Version: $version-1
Architecture: $deb_arch
Maintainer: $maintainer
Installed-Size: $(du -sk "$pkg/usr" | cut -f1)
Section: utils
Priority: optional
Homepage: $homepage
Description: $summary
 $description
CONTROL
    file="$out/${name}_$version-1_$deb_arch.deb"
    # xz, unlike the newer default zstd, is read by every dpkg the static
    # binary runs under.
    dpkg-deb -Zxz --build --root-owner-group "$pkg" "$file" >/dev/null
    publish "$file"
}

rpm_package() {
    local top="$stage/rpm" tree="$stage/rpm-tree" file="$out/$name-$version-1.$arch.rpm"
    need rpmbuild
    usr_tree "$tree/usr"
    mkdir -p "$top/SPECS"
    # The binary ships as built and already stripped; rpmbuild's own
    # stripping and debug packages have nothing to add.
    cat > "$top/SPECS/$name.spec" <<SPEC
%global debug_package %{nil}
%global __os_install_post %{nil}
%global _build_id_links none

Name: $name
Version: $version
Release: 1
Summary: $summary
License: Apache-2.0
URL: $homepage

%description
$description

%install
mkdir -p %{buildroot}
cp -r $tree/usr %{buildroot}/

%files
/usr/bin/pw
/usr/lib/systemd/user/process-watch.service
/usr/share/doc/$name
SPEC
    rpmbuild -bb --quiet --define "_topdir $top" --target "$arch" "$top/SPECS/$name.spec" >&2
    mv "$top/RPMS/$arch/$(basename "$file")" "$file"
    publish "$file"
}

windows_installer() {
    local bundle="$1" iscc setup="$name-$version-$arch-setup"
    need cygpath
    iscc="$(command -v iscc || command -v ISCC || true)"
    for candidate in "/c/Program Files (x86)/Inno Setup 6/ISCC.exe" "/c/Program Files/Inno Setup 6/ISCC.exe"; do
        [ -n "$iscc" ] || { [ -f "$candidate" ] && iscc="$candidate"; }
    done
    [ -n "$iscc" ] || die "--installers needs Inno Setup 6 (ISCC.exe)"
    "$iscc" -Q \
        "-DAppVersion=$version" \
        "-DPayload=$(cygpath -w "$bundle")" \
        "-DLicenseFile=$(cygpath -w "$root/LICENSE")" \
        "-O$(cygpath -w "$out")" \
        "-F$setup" \
        "$(cygpath -w "$root/scripts/process-watch.iss")" >&2
    publish "$out/$setup.exe"
}

case "$platform" in
    linux | freebsd | macos)
        bundle="$stage/$base"
        mkdir -p "$bundle"
        install -m755 "$binary" "$bundle/pw"
        install -m755 "$root/install.sh" "$bundle/install.sh"
        cp -R "$root/contrib" "$bundle/"
        docs "$bundle"
        tar -czf "$out/$base.tar.gz" -C "$stage" "$base"
        publish "$out/$base.tar.gz"
        if [ "$platform" = linux ] && [ "$installers" -eq 1 ]; then
            deb_package
            rpm_package
        fi
        ;;
    windows)
        bundle="$stage/$base"
        mkdir -p "$bundle"
        cp "$binary" "$bundle/pw.exe"
        docs "$bundle"
        zip_folder "$bundle" "$out/$base.zip"
        publish "$out/$base.zip"
        # A data folder beside pw.exe keeps every recording inside the
        # unpacked folder, so it runs from anywhere and leaves no trace.
        portable="$stage/$base-portable"
        cp -r "$bundle" "$portable"
        mkdir -p "$portable/data"
        zip_folder "$portable" "$out/$base-portable.zip"
        publish "$out/$base-portable.zip"
        [ "$installers" -eq 0 ] || windows_installer "$bundle"
        ;;
esac
