#!/usr/bin/env bash
# Builds a signed APT repository (suite `stable`, component `main`, amd64) in SITE from the
# given .deb files, replacing whatever SITE held: the release workflow publishes it to the
# gh-pages branch. Run from the release workflow, or locally to test it.
#
# Usage: APT_GPG_PRIVATE_KEY=<armored secret key> [APT_GPG_PASSPHRASE=…] \
#          build-apt-repo.sh SITE [DEB...]
# Needs apt-ftparchive (apt-utils) and gpg. Layout:
#   pool/main/*.deb
#   dists/stable/{Release,InRelease,Release.gpg}
#   dists/stable/main/binary-amd64/Packages{,.gz}
#   live-keys.gpg        the public key (binary, for /etc/apt/keyrings)
set -euo pipefail

site=$1
shift
: "${APT_GPG_PRIVATE_KEY:?the signing key is missing}"

# A keyring of its own, so this never touches (or depends on) the caller's.
GNUPGHOME=$(mktemp -d)
export GNUPGHOME
trap 'gpgconf --kill gpg-agent 2>/dev/null || true; rm -rf "$GNUPGHOME"' EXIT
gpg --batch --quiet --import <<<"$APT_GPG_PRIVATE_KEY"
fpr=$(gpg --batch --with-colons --list-secret-keys | awk -F: '$1 == "fpr" { print $10; exit }')
sign=(gpg --batch --yes --local-user "$fpr")
if [ -n "${APT_GPG_PASSPHRASE:-}" ]; then
  printf '%s' "$APT_GPG_PASSPHRASE" >"$GNUPGHOME/passphrase"
  sign+=(--pinentry-mode loopback --passphrase-file "$GNUPGHOME/passphrase")
fi

rm -rf "$site"
mkdir -p "$site/pool/main" "$site/dists/stable/main/binary-amd64"
if [ $# -gt 0 ]; then
  cp -- "$@" "$site/pool/main/"
fi
cd "$site"

# Paths in Packages are relative to the repository root, so run it from there.
apt-ftparchive packages pool >dists/stable/main/binary-amd64/Packages
gzip -9 --keep --no-name dists/stable/main/binary-amd64/Packages
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin="Live Keys" \
  -o APT::FTPArchive::Release::Label="Live Keys" \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Architectures=amd64 \
  -o APT::FTPArchive::Release::Components=main \
  -o APT::FTPArchive::Release::Description="Live Keys releases" \
  release dists/stable >Release.tmp
mv Release.tmp dists/stable/Release

"${sign[@]}" --clearsign --output dists/stable/InRelease dists/stable/Release
"${sign[@]}" --armor --detach-sign --output dists/stable/Release.gpg dists/stable/Release
gpg --batch --export "$fpr" >live-keys.gpg
# Served as is: no Jekyll run over it.
touch .nojekyll

echo "APT repository in $site signed by $fpr with $# package(s)"
