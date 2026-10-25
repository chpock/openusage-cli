#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    printf 'Usage: %s PACKAGE.deb\n' "$0" >&2
    exit 1
fi

depends=$(dpkg-deb --field "$1" Depends)
case "$depends" in
    *[![:space:]]*) printf '%s\n' "$depends" ;;
    *)
        printf 'Package has no runtime dependencies; automatic dependency discovery must succeed.\n' >&2
        exit 1
        ;;
esac
