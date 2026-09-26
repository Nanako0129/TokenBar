#!/bin/sh
# Re-render the OG embed card from og.html → public/og-card-v2.png (1200×630).
# Bump OUT to a fresh filename on every redesign — social platforms cache
# scrapes per image URL, and a new name is the only reliable cache bust.
# LANG_QS=zh-tw|zh-cn renders the Chinese card (see og.html).
# Needs Chrome; fonts load from Google Fonts, so run online.
set -e
cd "$(dirname "$0")"
OUT="${OUT:-../public/og-card-v5.png}"
# A localized render must never overwrite the default English card.
if [ -n "$LANG_QS" ] && [ "$OUT" = "../public/og-card-v5.png" ]; then
  OUT="../public/og-card-v5-$LANG_QS.png"
fi
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
"$CHROME" --headless=new --screenshot="$OUT" \
  --window-size=1200,630 --hide-scrollbars --virtual-time-budget=8000 \
  "file://$PWD/og.html${LANG_QS:+?lang=$LANG_QS}"
sips -g pixelWidth -g pixelHeight "$OUT"
