#!/usr/bin/env bash
#
# Create a stable self-signed code-signing identity for momentum in the login
# keychain. Signing with a *stable* identity (instead of ad-hoc) is what lets
# macOS persist the Bluetooth (TCC) grant across rebuilds: TCC keys the grant on
# the signature's designated requirement, which stays constant as long as the
# same certificate and identifier are used.
#
# Idempotent: re-running is a no-op once the identity exists.

set -euo pipefail

IDENTITY="${MOMENTUM_SIGN_IDENTITY:-momentum-dev}"
KEYCHAIN="${MOMENTUM_KEYCHAIN:-$HOME/Library/Keychains/login.keychain-db}"
P12_PASS="momentum"

if security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
  echo "Code-signing identity '$IDENTITY' already present — nothing to do."
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Generating self-signed code-signing certificate '$IDENTITY'…"
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
  -subj "/CN=$IDENTITY" \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" >/dev/null 2>&1

openssl pkcs12 -export \
  -out "$TMP/identity.p12" \
  -inkey "$TMP/key.pem" -in "$TMP/cert.pem" \
  -name "$IDENTITY" -passout "pass:$P12_PASS" >/dev/null 2>&1

# Import the key+cert and allow /usr/bin/codesign to use the private key.
security import "$TMP/identity.p12" -k "$KEYCHAIN" -P "$P12_PASS" -T /usr/bin/codesign

echo
echo "Created code-signing identity '$IDENTITY' in:"
echo "  $KEYCHAIN"
echo
echo "On the first 'make build', macOS may ask to allow codesign to use the key."
echo "Choose 'Always Allow' so subsequent builds are non-interactive."
