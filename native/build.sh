#!/bin/sh
set -eu
cd "$(dirname "$0")"
if [ "$(uname -s)" != Darwin ]; then
  echo 'GoFin Touch ID requires macOS.' >&2
  exit 1
fi
mkdir -p .build
cp GoFinTouchID.swift .build/main.swift
/usr/bin/plutil -lint Info.plist >/dev/null
/usr/bin/xcrun swiftc -O -framework AppKit -framework LocalAuthentication -framework CryptoKit \
  -target "$(/usr/bin/uname -m)-apple-macosx13.0" \
  -Xlinker -sectcreate -Xlinker __TEXT -Xlinker __info_plist -Xlinker Info.plist \
  .build/main.swift ApprovalPresentation.swift -o .build/gofin-touch-id
# Ad-hoc hardened signing supports a local beta; developer release signing is separate.
# No debugging, DYLD injection, or library-validation exceptions are granted.
/usr/bin/codesign --force --sign - --options runtime .build/gofin-touch-id
/usr/bin/codesign --verify --strict .build/gofin-touch-id
# Verify the same basename used at the protected installed location. The source
# bundle keeps its existing artifact name and checksum contract.
mkdir -p .build/authorization
cp .build/gofin-touch-id .build/authorization/GoFin
/usr/bin/codesign --verify --strict .build/authorization/GoFin
echo 'Built native/.build/gofin-touch-id (hardened runtime).'
