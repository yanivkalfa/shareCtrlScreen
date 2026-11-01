#!/usr/bin/env bash
# Create the self-signed code-signing certificate that signs every macOS build of
# ShareCtrlScreen, and print the two GitHub secret values.
#
# Run this ONCE, on any machine with openssl (macOS, Linux, Git Bash, WSL).
# Re-running creates a DIFFERENT certificate: every user would then have to grant
# Screen Recording and Accessibility again, because macOS ties those grants to the
# signing identity (plans/01-MACOS-FINDINGS.md section 8). Keep the .p12 safe.
#
# Usage: macos-make-signing-cert.sh [output-dir]
#   output-dir  where ShareCtrlScreen-signing.p12 is written (default: current dir)
#
# Output (stdout): the two secret assignments, nothing else. Guidance goes to stderr.
#   MAC_SIGN_P12_B64   base64 of the .p12
#   MAC_SIGN_P12_PASS  the generated .p12 password
# Add them at: GitHub repo > Settings > Secrets and variables > Actions >
# New repository secret. Then delete the terminal scrollback and store the .p12
# offline (password manager or an encrypted drive) as the only other copy.
#
# The private key never touches disk outside a mode-700 temp directory that is
# removed on exit; the password is generated, never accepted from the command line.
#
# References
#   - Apple, "Creating Certificates for Signing Code" (Certificate Assistant
#     equivalent: codeSigning extended key usage, digitalSignature key usage):
#     https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/Procedures/Procedures.html
#   - Tauri v2, "macOS Code Signing" (APPLE_CERTIFICATE / .p12 handling):
#     https://v2.tauri.app/distribute/sign/macos/
#   - OpenSSL 3 writes .p12 files with AES and a SHA-256 MAC by default, which
#     macOS `security import` rejects ("MAC verification failed"); the explicit
#     SHA1/3DES flags below keep the file importable. openssl-pkcs12(1):
#     https://docs.openssl.org/3.0/man1/openssl-pkcs12/

set -euo pipefail

readonly CERT_CN="ShareCtrlScreen Self-Signed"
readonly VALID_DAYS=3650
readonly P12_NAME="ShareCtrlScreen-signing.p12"

out_dir="${1:-.}"
if [[ ! -d "$out_dir" ]]; then
  echo "error: output directory '$out_dir' does not exist" >&2
  exit 1
fi
out_p12="$out_dir/$P12_NAME"
if [[ -e "$out_p12" ]]; then
  echo "error: $out_p12 already exists; refusing to overwrite a signing identity" >&2
  exit 1
fi

if ! command -v openssl >/dev/null 2>&1; then
  echo "error: openssl not found" >&2
  exit 1
fi

work="$(mktemp -d)"
chmod 700 "$work"
trap 'rm -rf "$work"' EXIT

cat >"$work/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions    = ext
prompt             = no

[dn]
CN = ${CERT_CN}

[ext]
basicConstraints = critical, CA:false
keyUsage         = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
subjectKeyIdentifier = hash
EOF

openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days "$VALID_DAYS" \
  -config "$work/openssl.cnf" \
  -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null

password="$(openssl rand -hex 24)"

# The password goes through the environment, not argv, so it never shows in `ps`.
P12_PASS="$password" openssl pkcs12 -export \
  -inkey "$work/key.pem" -in "$work/cert.pem" \
  -name "$CERT_CN" \
  -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1 \
  -passout env:P12_PASS -out "$out_p12"
chmod 600 "$out_p12"

b64="$(openssl base64 -A -in "$out_p12")"
fingerprint="$(openssl x509 -in "$work/cert.pem" -noout -fingerprint -sha1)"
expires="$(openssl x509 -in "$work/cert.pem" -noout -enddate)"

{
  echo "Certificate : CN=${CERT_CN}"
  echo "$fingerprint"
  echo "$expires"
  echo "Wrote       : $out_p12 (keep offline; this file plus its password IS the identity)"
  echo
  echo "Add these two as GitHub Actions repository secrets. They are printed once."
} >&2

printf 'MAC_SIGN_P12_B64=%s\n' "$b64"
printf 'MAC_SIGN_P12_PASS=%s\n' "$password"
