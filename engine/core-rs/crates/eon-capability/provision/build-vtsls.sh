#!/usr/bin/env bash
# Offline, operator-only. Supply vetted x86-64 glibc Node 24.22.1, the
# upstream npm tarballs named in vtsls/inputs, and a new protected installation.
set -euo pipefail
umask 077
[[ $# == 3 ]] || { echo "usage: $0 NODE ARCHIVES NEW_INSTALL" >&2; exit 2; }
node=$(realpath -e -- "$1")
archives=$(realpath -e -- "$2")
install=$(realpath -m -- "$3")
here=$(dirname -- "$(realpath -e -- "$0")")/vtsls
[[ $("$node" -v) == v24.22.1 && $(uname -m) == x86_64 && ! -L $3 ]]
mkdir -m 700 -- "$install" # Never overwrite or adopt an existing installation.
trap 'rm -rf -- "$install"' EXIT
cd "$install"
mkdir build licenses
cd build
while read -r name archive digest; do
    cp -- "$archives/$archive" input.tgz
    echo "$digest  input.tgz" | sha256sum --status -c
    pkg="node_modules/$name"
    mkdir -p "$pkg"
    tar xzf input.tgz --strip-components=1 --no-same-owner -C "$pkg"
    rm input.tgz
    dest="../licenses/$name"
    mkdir -p "$dest"
    find "$pkg" -maxdepth 1 -type f \( -iname 'license*' -o -iname '*notice*' -o -iname '*copyright*' \) -exec cp -- {} "$dest/" \;
done < "$here/inputs"
cp -- "$here/host.cjs" "$here/diagnostics.cjs" .
"$node" "$here/prepare.cjs"
node_modules/@esbuild/linux-x64/bin/esbuild entry.cjs --bundle --platform=node --format=cjs \
    --target=node24 --legal-comments=eof --outfile=main.cjs
"$node" --experimental-sea-config sea.json
cp -- "$node" ../vtsls
"$node" node_modules/postject/dist/cli.js ../vtsls NODE_SEA_BLOB sea.blob \
    --sentinel-fuse NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2
chmod 500 ../vtsls
cd ..
rm -rf build
# Store this digest separately from the image, NOT a hash recalculated at launch.
sha256sum -- "$install/vtsls"
trap - EXIT
