#!/usr/bin/env bash
# Move seashell-cxx onto a new Seashell commit
#
#   etc/scripts/bump.sh <commit sha>
set -euo pipefail

rev="${1:?usage: bump.sh <seashell commit sha>}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

[[ "$rev" =~ ^[0-9a-f]{40}$ ]] ||
    { echo "bump: pass a full 40-character commit sha" >&2; exit 1; }

lock_url="https://raw.githubusercontent.com/soundsonacid/seashell/${rev}/Cargo.lock"

curl -fsSL "$lock_url" -o Cargo.lock.upstream ||
    { echo "bump: no Cargo.lock at Seashell commit ${rev}" >&2; exit 1; }
mv Cargo.lock.upstream Cargo.lock

sed -E -i.bak "s/(soundsonacid\/seashell\", rev = \")[0-9a-f]+/\1${rev}/" Cargo.toml
sed -E -i.bak "s/^(#define SEASHELL_UPSTREAM_REV[[:space:]]+)\"[^\"]+\"/\1\"${rev}\"/" \
    include/seashell.h
rm -f Cargo.toml.bak include/seashell.h.bak

cargo metadata --format-version 1 >/dev/null
echo "bump: seashell ${rev}"
echo "bump: build with 'make lib test'; if Agave crates need a newer rustc,"
echo "      raise rust-toolchain.toml and rust-version in Cargo.toml together"
