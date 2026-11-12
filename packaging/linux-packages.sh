#!/usr/bin/env bash
# Build magia_<version>_amd64.deb and magia-<version>-1.x86_64.rpm from release binaries.
# usage: packaging/linux-packages.sh <dir with magia and magia-gui> <output dir>
# Needs dpkg-deb (deb) and rpmbuild (rpm); a missing tool skips that package with a warning.
#
# Both install /usr/bin/magia (CLI), /usr/bin/magia-gui (desktop app), the menu entry, the icon
# and the docs. The binaries only link libc/libm/libgcc; OpenGL, Wayland and X11 are loaded at
# runtime, so they are listed as dependencies rather than found by the linker.
set -euo pipefail
bin="${1:?dir with magia and magia-gui}"
out="${2:?output dir}"
here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Files as they land on disk, shared by both formats.
stage="$work/stage"
install -Dm755 "$bin/magia" "$stage/usr/bin/magia"
install -Dm755 "$bin/magia-gui" "$stage/usr/bin/magia-gui"
install -Dm644 "$here/magia.desktop" "$stage/usr/share/applications/magia.desktop"
install -Dm644 "$here/magia.png" "$stage/usr/share/icons/hicolor/256x256/apps/magia.png"
install -Dm644 "$here/magia.svg" "$stage/usr/share/icons/hicolor/scalable/apps/magia.svg"
install -Dm644 "$root/README.md" "$stage/usr/share/doc/magia/README.md"
install -Dm644 "$root/docs/magia.example.jsonc" "$stage/usr/share/doc/magia/magia.example.jsonc"
install -Dm644 "$root/LICENSE" "$stage/usr/share/licenses/magia/LICENSE"

summary="Coding agent for the terminal and desktop (pure Rust, OpenGL GUI)"
describe="magia reads, edits and runs code in a project folder, talking to OpenCode Zen,
OpenCode Go, OpenRouter or any OpenAI/Anthropic-compatible endpoint. It ships a
native desktop app (magia-gui) and a headless CLI (magia), built for old x86-64
CPUs (SSE2) and low memory."

if command -v dpkg-deb >/dev/null; then
  deb="$work/deb"
  cp -a "$stage" "$deb"
  # Debian keeps the license under /usr/share/doc.
  mv "$deb/usr/share/licenses/magia/LICENSE" "$deb/usr/share/doc/magia/copyright"
  rm -r "$deb/usr/share/licenses"
  mkdir -p "$deb/DEBIAN"
  size="$(du -sk "$deb/usr" | cut -f1)"
  {
    echo "Package: magia"
    echo "Version: $version"
    echo "Architecture: amd64"
    echo "Maintainer: Alexandro Trevisan <https://github.com/alextrevisan/magia>"
    echo "Installed-Size: $size"
    echo "Depends: libc6 (>= 2.35), libgcc-s1, libgl1, libegl1, libxkbcommon0"
    echo "Recommends: libwayland-client0, libwayland-egl1, libx11-6, libxcursor1, libxi6, libxrandr2, git"
    echo "Suggests: wl-clipboard, bubblewrap"
    echo "Section: devel"
    echo "Priority: optional"
    echo "Homepage: https://github.com/alextrevisan/magia"
    echo "Description: $summary"
    echo "$describe" | sed 's/^/ /'
  } > "$deb/DEBIAN/control"
  dpkg-deb --root-owner-group -Zxz --build "$deb" "$out/magia_${version}_amd64.deb" >/dev/null
  echo "built $out/magia_${version}_amd64.deb"
else
  echo "warning: dpkg-deb not found, skipping .deb" >&2
fi

if command -v rpmbuild >/dev/null; then
  top="$work/rpm"
  mkdir -p "$top"/{BUILD,RPMS,SPECS,SOURCES,SRPMS}
  cat > "$top/SPECS/magia.spec" <<SPEC
Name:           magia
Version:        $version
Release:        1
Summary:        $summary
License:        MIT
URL:            https://github.com/alextrevisan/magia
ExclusiveArch:  x86_64
# Runtime libraries the GUI opens itself (not visible to the automatic dependency scan).
Requires:       libglvnd-egl libglvnd-glx libxkbcommon
Recommends:     libwayland-client libwayland-egl libX11 libXcursor libXi libXrandr git
Suggests:       wl-clipboard bubblewrap
# The binaries are already built and stripped.
%global debug_package %{nil}
%global __strip /bin/true

%description
$describe

%install
cp -a $stage/. %{buildroot}/

%files
/usr/bin/magia
/usr/bin/magia-gui
/usr/share/applications/magia.desktop
/usr/share/icons/hicolor/256x256/apps/magia.png
/usr/share/icons/hicolor/scalable/apps/magia.svg
%doc /usr/share/doc/magia/README.md
%doc /usr/share/doc/magia/magia.example.jsonc
%license /usr/share/licenses/magia/LICENSE
SPEC
  rpmbuild --quiet -bb --define "_topdir $top" "$top/SPECS/magia.spec"
  cp "$top"/RPMS/x86_64/*.rpm "$out/"
  echo "built $(ls "$out"/magia-*.rpm)"
else
  echo "warning: rpmbuild not found, skipping .rpm" >&2
fi
