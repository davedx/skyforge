#!/bin/sh
set -eu

task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$task_root"
sh scripts/bundle-macos.sh

task_arch=$(uname -m)
case "$task_arch" in
    arm64|x86_64) ;;
    *) echo "Unsupported macOS architecture: $task_arch" >&2; exit 1 ;;
esac
task_dist="$task_root/target/dist"
task_name="Skyforge-macos-$task_arch.zip"
task_binary="skyforge-macos-$task_arch"
mkdir -p "$task_dist"
codesign --verify --deep --strict target/release/Skyforge.app
lipo target/release/skyforge -verify_arch "$task_arch"
ditto -c -k --sequesterRsrc --keepParent target/release/Skyforge.app "$task_dist/$task_name"
cp target/release/skyforge "$task_dist/$task_binary"
cd "$task_dist"
shasum -a 256 "$task_name" "$task_binary" > SHA256SUMS
echo "Release files: $task_dist"
