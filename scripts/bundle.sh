#!/bin/bash
# Assemble TokenBar.app from the SwiftPM release build.
#
#   scripts/bundle.sh [marketing-version] [build-number]
#
# Run from the repo root. Since v1.0.0 there is a single app identity
# (TokenBar.app / com.nyanako.tokenbar) — prereleases ship through the same
# bundle on the Sparkle "beta" channel instead of a side-by-side app. The
# retired beta identity (com.nyanako.tokenbar.beta / "TokenBar Beta.app")
# is migrated from on first launch.
set -euo pipefail

VERSION="${1:-1.0.0}"
BUILD_NUMBER="${2:-100}"
BUNDLE_ID="${BUNDLE_ID:-com.nyanako.tokenbar}"
APP_NAME="${APP_DISPLAY:-TokenBar}"
# Overridable so a build can sit somewhere other than beside the release
# artifact without changing its identity. `make selftest-bundled` uses it to
# assemble a real `TokenBar.app` — same identifier, same CFBundleName — in a
# subdirectory, rather than renaming the app to avoid the collision.
OUT_DIR="${OUT_DIR:-dist}"
APP="$OUT_DIR/$APP_NAME.app"

echo "==> building release binaries"
cargo build --release
swift build -c release

echo "==> assembling $APP ($VERSION, build $BUILD_NUMBER, $BUNDLE_ID)"
rm -rf "$APP"
mkdir -p "$OUT_DIR"
# Keep Spotlight from indexing local builds — otherwise every dist/ app
# shows up beside the installed one in search results.
touch "$OUT_DIR/.metadata_never_index"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"

cp .build/release/TokenBar "$APP/Contents/MacOS/TokenBar"
# SwiftPM resource bundle (animation frames, agent icons).
cp -R .build/release/TokenBar_TokenBar.bundle "$APP/Contents/Resources/"
# Localizations land in the *main* bundle for packaged runs. Bare `swift run`
# stages the same .lproj directories from the SwiftPM resource bundle before
# SwiftUI creates any views.
cp -R Sources/TokenBar/Resources/Localizations/*.lproj "$APP/Contents/Resources/"
# Brand icon, shared with the Tauri app.
if [ -f assets/icon.icns ]; then
  cp assets/icon.icns "$APP/Contents/Resources/icon.icns"
fi
# Sparkle framework (SPM binary artifact).
SPARKLE_FRAMEWORK=".build/artifacts/sparkle/Sparkle/Sparkle.xcframework/macos-arm64_x86_64/Sparkle.framework"
if [ -d "$SPARKLE_FRAMEWORK" ]; then
  cp -R "$SPARKLE_FRAMEWORK" "$APP/Contents/Frameworks/"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>TokenBar</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$BUILD_NUMBER</string>
    <key>CFBundleIconFile</key>
    <string>icon</string>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleLocalizations</key>
    <array>
        <string>en</string>
        <string>zh-Hant</string>
    </array>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT License</string>
    <key>SUEnableInstallerLauncherService</key>
    <false/>
    <key>SUPublicEDKey</key>
    <string>EzyeEi0NEwYK/pYigOPVClXmbmnHXXBEHM7r2uy8GYs=</string>
    <!-- raw main-branch URL: GitHub's releases/latest/download excludes
         prerelease-flagged releases, which would break the beta channel.
         The old TokenBar-Native path keeps redirecting here for existing
         beta installs (the name is never reclaimed). -->
    <key>SUFeedURL</key>
    <string>https://raw.githubusercontent.com/Nanako0129/TokenBar/main/appcast.xml</string>
</dict>
</plist>
PLIST

echo "==> ad-hoc codesign"
codesign --force --deep --sign - "$APP"

echo "==> done: $APP"
