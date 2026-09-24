#!/bin/bash
# Build Sparkle.framework with installed-application-name normalization enabled.
#
#   scripts/build-sparkle.sh        # builds if needed, prints the framework path
#
# Why this exists: SPARKLE_NORMALIZE_INSTALLED_APPLICATION_NAME is a
# compile-time macro (Sparkle's Configurations/ConfigCommon.xcconfig sets it to
# 0), not an Info.plist key, so the official prebuilt xcframework can never
# perform the v2.0 TokenBar.app -> Syrtis.app rename no matter what the app
# declares. Enabling it requires compiling Sparkle ourselves. OpenAI did the
# same for the Codex -> ChatGPT rename.
#
# The macro has three consumers, all inside the framework, so one rebuild
# covers them: SUInstaller.m (chooses the install path), SUPlainInstaller.m
# (switches away from the atomic swap when the path changes) and
# InstallerProgressAppController.m (relaunches the app at the new path).
#
# Source comes from SwiftPM's own checkout, which Package.resolved already pins.
# Cloning Sparkle separately would create a second revision to keep in sync with
# the one the app links against.
#
# The macro is not exposed in any of the framework's 23 public headers, so the
# app still compiles and links against the stock SPM artifact; only the binary
# copied into Contents/Frameworks changes. Package.swift is untouched.
set -euo pipefail
cd "$(dirname "$0")/.."

SRC=".build/checkouts/Sparkle"
OUT=".build/sparkle-normalized"
FRAMEWORK="$OUT/Sparkle.framework"
STAMP="$OUT/.built-from"

[ -d "$SRC" ] || { echo "build-sparkle: $SRC missing — run 'swift build' first" >&2; exit 1; }
REV="$(git -C "$SRC" rev-parse HEAD)"
# Sparkle's project targets macOS 10.13, which Xcode 27 refuses outright
# ("supported deployment target versions is 12.0 to 27.0.x"), failing every
# target. Build for the app's own floor instead (Package.swift: .macOS(.v14)):
# the framework ships inside the app and never runs on anything older.
DEPLOYMENT_TARGET="14.0"
# Universal, matching the official xcframework slice the app used to ship. The
# deployment target is part of the stamp so a framework cached under the old
# setting is rebuilt rather than reused.
WANT="$REV arm64 x86_64 normalize=1 macos=$DEPLOYMENT_TARGET"

if [ -d "$FRAMEWORK" ] && [ "$(cat "$STAMP" 2>/dev/null)" = "$WANT" ]; then
  echo "$FRAMEWORK"
  exit 0
fi

command -v xcodebuild >/dev/null || {
  echo "build-sparkle: xcodebuild not found. Full Xcode is required to bundle a" >&2
  echo "  release; the Command Line Tools alone cannot build Sparkle." >&2
  exit 1
}

echo "==> building normalization-enabled Sparkle (${REV:0:12}, ~2-3 min)" >&2
rm -rf "$OUT"; mkdir -p "$OUT"
xcodebuild -project "$SRC/Sparkle.xcodeproj" -scheme Sparkle -configuration Release \
  -derivedDataPath "$OUT/dd" \
  -clonedSourcePackagesDirPath "$OUT/spm" \
  SPARKLE_NORMALIZE_INSTALLED_APPLICATION_NAME=1 \
  ONLY_ACTIVE_ARCH=NO ARCHS="arm64 x86_64" \
  MACOSX_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET" \
  CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM="" \
  build >"$OUT/build.log" 2>&1 || { tail -40 "$OUT/build.log" >&2; exit 1; }

cp -R "$OUT/dd/Build/Products/Release/Sparkle.framework" "$FRAMEWORK"
echo "$WANT" > "$STAMP"
echo "$FRAMEWORK"
