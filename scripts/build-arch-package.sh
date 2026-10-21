#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
build_dir="$repo_dir/target/arch-package"
mkdir -p "$build_dir"
git -C "$repo_dir" archive --format=tar.gz --prefix=alacritty/ HEAD > "$build_dir/alacritty-source.tar.gz"
cp "$repo_dir/packaging/arch/PKGBUILD" "$build_dir/PKGBUILD"
cd "$build_dir"
makepkg --force --noconfirm
