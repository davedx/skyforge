#!/bin/sh
set -eu

task_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$task_root"
if [ "$(uname -s)" != "Darwin" ]; then
    echo "This bundle script targets macOS. Use cargo run --release on other platforms." >&2
    exit 1
fi
task_cargo=${HARNESS_CARGO:-"$HOME/.cargo/bin/cargo"}
if [ ! -x "$task_cargo" ]; then task_cargo=cargo; fi
"$task_cargo" build --release --locked

task_bundle="$task_root/target/release/Skyforge.app"
mkdir -p "$task_bundle/Contents/MacOS" "$task_bundle/Contents/Resources"
cp target/release/skyforge "$task_bundle/Contents/MacOS/skyforge"

# Normalize the source onto a transparent square without stretching the artwork.
task_iconset="$task_root/target/release/Skyforge.iconset"
task_icon_source="$task_root/target/release/Skyforge-icon.png"
mkdir -p "$task_iconset"
task_icon_width=$(sips -g pixelWidth icon.png | awk '/pixelWidth:/ { print $2 }')
task_icon_height=$(sips -g pixelHeight icon.png | awk '/pixelHeight:/ { print $2 }')
task_icon_side=$task_icon_width
if [ "$task_icon_height" -gt "$task_icon_side" ]; then task_icon_side=$task_icon_height; fi
sips -p "$task_icon_side" "$task_icon_side" icon.png --out "$task_icon_source" >/dev/null
for task_icon_size in 16 32 128 256 512; do
    sips -z "$task_icon_size" "$task_icon_size" "$task_icon_source" \
        --out "$task_iconset/icon_${task_icon_size}x${task_icon_size}.png" >/dev/null
    task_icon_retina=$((task_icon_size * 2))
    sips -z "$task_icon_retina" "$task_icon_retina" "$task_icon_source" \
        --out "$task_iconset/icon_${task_icon_size}x${task_icon_size}@2x.png" >/dev/null
done
iconutil -c icns "$task_iconset" --output "$task_bundle/Contents/Resources/Skyforge.icns"

cat > "$task_bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Skyforge</string>
  <key>CFBundleDisplayName</key><string>Skyforge</string>
  <key>CFBundleExecutable</key><string>skyforge</string>
  <key>CFBundleIconFile</key><string>Skyforge.icns</string>
  <key>CFBundleIdentifier</key><string>dev.skyforge.prototype</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
codesign --force --sign - "$task_bundle"
echo "Built $task_bundle"

# A running pre-rename instance restarts via its original executable path.
# Keep that path as a launcher into the renamed bundle, so the deferred
# restart starts Skyforge with the correct macOS bundle identity.
task_legacy="$task_root/target/release/Harness.app/Contents/MacOS/harness"
if [ -f "$task_legacy" ]; then
    cat > "$task_legacy.next" <<'LAUNCHER'
#!/bin/sh
set -eu
task_legacy_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec "$task_legacy_dir/../../../Skyforge.app/Contents/MacOS/skyforge" "$@"
LAUNCHER
    chmod +x "$task_legacy.next"
    mv -f "$task_legacy.next" "$task_legacy"
fi
if [ -f "$task_root/target/release/harness" ]; then
    cp "$task_root/target/release/skyforge" "$task_root/target/release/harness.next"
    mv -f "$task_root/target/release/harness.next" "$task_root/target/release/harness"
fi
