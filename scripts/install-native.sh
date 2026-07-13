#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
Usage: install.sh [--bin-dir DIR] [--replace --backup PATH]

Install the archive-local aise executable. Existing files and symbolic links are
never replaced unless --replace is paired with an absent rollback --backup path.
EOF
}

bin_dir=${AI_SESSION_SEARCH_BIN_DIR:-${XDG_BIN_HOME:-${HOME:+$HOME/.local/bin}}}
replace=false
backup=

while [ "$#" -gt 0 ]; do
    case "$1" in
        --bin-dir)
            [ "$#" -ge 2 ] || { echo "error: --bin-dir requires a value" >&2; exit 2; }
            bin_dir=$2
            shift 2
            ;;
        --replace)
            replace=true
            shift
            ;;
        --backup)
            [ "$#" -ge 2 ] || { echo "error: --backup requires a value" >&2; exit 2; }
            backup=$2
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

[ -n "$bin_dir" ] || { echo "error: set --bin-dir, AI_SESSION_SEARCH_BIN_DIR, XDG_BIN_HOME, or HOME" >&2; exit 2; }
if [ "$replace" = true ] && [ -z "$backup" ]; then
    echo "error: --replace requires an explicit --backup path" >&2
    exit 2
fi
if [ "$replace" = false ] && [ -n "$backup" ]; then
    echo "error: --backup is valid only with --replace" >&2
    exit 2
fi

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
source_binary=$script_dir/aise
[ -f "$source_binary" ] && [ -x "$source_binary" ] || {
    echo "error: archive-local executable is missing or not executable: $source_binary" >&2
    exit 1
}

mkdir -p -- "$bin_dir"
destination=$bin_dir/aise
if [ -L "$destination" ]; then
    echo "error: refusing symbolic-link destination: $destination" >&2
    exit 1
fi
if [ -e "$destination" ] && [ ! -f "$destination" ]; then
    echo "error: destination is not a regular file: $destination" >&2
    exit 1
fi

stage=$(mktemp "$bin_dir/.aise.install.XXXXXX")
cleanup() {
    if [ -n "${stage:-}" ] && [ -e "$stage" ]; then
        rm -f -- "$stage"
    fi
}
trap cleanup EXIT HUP INT TERM
cp -- "$source_binary" "$stage"
chmod 755 "$stage"

if [ -e "$destination" ]; then
    [ "$replace" = true ] || {
        echo "error: destination already exists: $destination" >&2
        exit 1
    }
    if [ -e "$backup" ] || [ -L "$backup" ]; then
        echo "error: rollback backup already exists: $backup" >&2
        exit 1
    fi
    mkdir -p -- "$(dirname -- "$backup")"
    ln -- "$destination" "$backup"
    mv -f -- "$stage" "$destination"
else
    ln -- "$stage" "$destination"
    rm -f -- "$stage"
fi
stage=
trap - EXIT HUP INT TERM
printf 'installed aise: %s\n' "$destination"
