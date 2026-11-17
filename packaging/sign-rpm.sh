#!/usr/bin/env bash
# Sign .rpm files with the private key in $RPM_SIGNING_KEY (ASCII-armored, no passphrase), then
# check each one against packaging/RPM-GPG-KEY-magia, the public key users import.
# usage: RPM_SIGNING_KEY="$(cat key.asc)" packaging/sign-rpm.sh <file.rpm>...
# Needs gpg and rpmsign (Ubuntu: the rpm package; Fedora: rpm-sign).
set -euo pipefail
: "${RPM_SIGNING_KEY:?ASCII-armored private key}"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

export GNUPGHOME="$work/gnupg"
mkdir -m 700 "$GNUPGHOME"
printf '%s\n' "$RPM_SIGNING_KEY" | gpg --batch --quiet --import
fpr="$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr/ {print $10; exit}')"
if [ -z "$fpr" ]; then
  echo "RPM_SIGNING_KEY holds no private key: it must be the whole" >&2
  echo "-----BEGIN PGP PRIVATE KEY BLOCK----- ... -----END PGP PRIVATE KEY BLOCK----- text" >&2
  exit 1
fi

# Ubuntu's rpm macros call /usr/bin/gpg2, which is not installed there.
rpmsign --define "_gpg_name $fpr" --define "__gpg $(command -v gpg)" --addsign "$@"

# A throwaway rpm database, so the check uses only the published key.
rpm --dbpath "$work/rpmdb" --initdb
rpm --dbpath "$work/rpmdb" --import "$here/RPM-GPG-KEY-magia"
for f in "$@"; do
  rpm --dbpath "$work/rpmdb" -Kv "$f"
  rpm --dbpath "$work/rpmdb" -K "$f" | grep -q 'signatures OK' || { echo "signature check failed: $f" >&2; exit 1; }
done
