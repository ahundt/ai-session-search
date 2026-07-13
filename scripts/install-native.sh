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
if [ -e "$destination" ] && [ ! -f "$destination" ] && [ ! -L "$destination" ]; then
    echo "error: destination is not a regular file: $destination" >&2
    exit 1
fi
if { [ -e "$destination" ] || [ -L "$destination" ]; } && [ "$replace" = false ]; then
    echo "error: destination already exists: $destination" >&2
    exit 1
fi
if [ "$replace" = true ] && { [ -e "$backup" ] || [ -L "$backup" ]; }; then
    echo "error: rollback backup already exists: $backup" >&2
    exit 1
fi

stage=$(mktemp "$bin_dir/.aise.install.XXXXXX")
rollback_symlink=false
cleanup() {
    if [ "$rollback_symlink" = true ] && { [ -e "$backup" ] || [ -L "$backup" ]; }; then
        if [ -e "$destination" ] || [ -L "$destination" ]; then
            rm -f -- "$destination"
        fi
        mv -- "$backup" "$destination" ||
            echo "error: failed to restore rollback backup: $backup" >&2
    fi
    if [ -n "${stage:-}" ] && [ -e "$stage" ]; then
        rm -f -- "$stage"
    fi
}
abort_install() {
    cleanup
    trap - EXIT HUP INT TERM
    exit 1
}
trap cleanup EXIT
trap abort_install HUP INT TERM
cp -- "$source_binary" "$stage"
chmod 755 "$stage"

if [ -e "$destination" ] || [ -L "$destination" ]; then
    mkdir -p -- "$(dirname -- "$backup")"
    if [ -L "$destination" ]; then
        rollback_symlink=true
        mv -- "$destination" "$backup"
    else
        ln -- "$destination" "$backup"
    fi
    mv -f -- "$stage" "$destination"
    rollback_symlink=false
else
    ln -- "$stage" "$destination"
    rm -f -- "$stage"
fi
stage=
trap - EXIT HUP INT TERM
printf 'installed aise: %s\n' "$destination"
