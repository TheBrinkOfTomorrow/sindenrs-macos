#!/bin/sh
# Create the self-signed code-signing certificate that bundle.sh signs the app with, so macOS
# keeps the Camera and Accessibility grants across rebuilds. An ad-hoc signature changes with
# every build, so macOS re-asks (and silently drops the Accessibility grant) each time; a fixed
# identity gives the app a stable designated requirement, and the grants stick.
#
# One-off and safe to re-run: it no-ops if the identity already exists. The GUI equivalent is
# Keychain Access > Certificate Assistant > Create a Certificate (Self-Signed Root, Code
# Signing). Remove it with:
#   security delete-identity -c "sindenrs_macos_dev" ~/Library/Keychains/login.keychain-db
set -eu
identity="${SINDENRS_SIGN_IDENTITY:-sindenrs_macos_dev}"
kc="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -p codesigning | grep -q "\"$identity\""; then
    echo "code-signing identity \"$identity\" already present"
    exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# A self-signed leaf with the code-signing extended key usage — what codesign needs.
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
    -subj "/CN=$identity" \
    -addext "basicConstraints=critical,CA:FALSE" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning"
# -legacy writes the bundle with the SHA1 MAC / 3DES that Apple's `security` can read; OpenSSL
# 3's modern default MAC fails to import ("MAC verification failed"). A throwaway password keeps
# the MAC check happy.
openssl pkcs12 -export -legacy -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
    -name "$identity" -out "$tmp/id.p12" -passout pass:sindenrs

# -A lets codesign use the private key without a keychain prompt on each build.
security import "$tmp/id.p12" -k "$kc" -P "sindenrs" -A

echo "created code-signing identity \"$identity\" in the login keychain"
security find-identity -p codesigning | grep "$identity"
