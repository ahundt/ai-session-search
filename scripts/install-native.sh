#!/bin/sh
# SPDX-FileCopyrightText: 2026 Andrew Hundt
# SPDX-License-Identifier: Apache-2.0

set -eu

usage() {
    cat <<'EOF'
Usage: install.sh [--bin-dir DIR] [--replace --backup PATH]

Install the archive-local aise executable. Existing files and symbolic links are
never replaced unless --replace is paired with an absent rollback --backup path.
When an ownership receipt exists, its rollback copy is BACKUP.aise-native-install.json.
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
source_receipt=$script_dir/aise-native-install.json
[ -f "$source_binary" ] && [ -x "$source_binary" ] || {
    echo "error: archive-local executable is missing or not executable: $source_binary" >&2
    exit 1
}
[ -f "$source_receipt" ] && [ ! -L "$source_receipt" ] || {
    echo "error: archive-local install receipt is missing or not a regular file: $source_receipt" >&2
    exit 1
}

mkdir -p -- "$bin_dir"
destination=$bin_dir/aise
receipt_destination=$bin_dir/aise-native-install.json
backup_receipt=$backup.aise-native-install.json
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
if { [ -e "$receipt_destination" ] || [ -L "$receipt_destination" ]; } &&
   { [ ! -f "$receipt_destination" ] || [ -L "$receipt_destination" ]; }; then
    echo "error: native install receipt destination is not a regular file: $receipt_destination" >&2
    exit 1
fi
if [ "$replace" = false ] && [ -e "$receipt_destination" ]; then
    echo "error: native install receipt already exists: $receipt_destination" >&2
    exit 1
fi
if [ "$replace" = true ] && [ -e "$receipt_destination" ] &&
   { [ -e "$backup_receipt" ] || [ -L "$backup_receipt" ]; }; then
    echo "error: rollback receipt backup already exists: $backup_receipt" >&2
    exit 1
fi

stage=$(mktemp "$bin_dir/.aise.install.XXXXXX")
receipt_stage=$(mktemp "$bin_dir/.aise.receipt.XXXXXX")
rollback_symlink=false
published_binary=false
published_receipt=false
had_destination=false
had_receipt=false
receipt_backup_created=false
cleanup() {
    if [ "$published_receipt" = true ]; then
        if [ -e "$receipt_destination" ] || [ -L "$receipt_destination" ]; then
            rm -f -- "$receipt_destination"
        fi
        if [ "$had_receipt" = true ] && [ -e "$backup_receipt" ]; then
            mv -- "$backup_receipt" "$receipt_destination" ||
                echo "error: failed to restore rollback receipt: $backup_receipt" >&2
        fi
    elif [ "$receipt_backup_created" = true ] && [ -e "$backup_receipt" ]; then
        rm -f -- "$backup_receipt"
    fi
    if [ "$published_binary" = true ]; then
        if [ -e "$destination" ] || [ -L "$destination" ]; then
            rm -f -- "$destination"
        fi
        if [ "$had_destination" = true ] && { [ -e "$backup" ] || [ -L "$backup" ]; }; then
            mv -- "$backup" "$destination" ||
                echo "error: failed to restore rollback backup: $backup" >&2
        fi
    elif [ "$rollback_symlink" = true ] && { [ -e "$backup" ] || [ -L "$backup" ]; }; then
        mv -- "$backup" "$destination" ||
            echo "error: failed to restore rollback backup: $backup" >&2
    fi
    if [ -n "${stage:-}" ] && [ -e "$stage" ]; then
        rm -f -- "$stage"
    fi
    if [ -n "${receipt_stage:-}" ] && [ -e "$receipt_stage" ]; then
        rm -f -- "$receipt_stage"
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
cp -- "$source_receipt" "$receipt_stage"
chmod 644 "$receipt_stage"

if [ -e "$destination" ] || [ -L "$destination" ]; then
    had_destination=true
    mkdir -p -- "$(dirname -- "$backup")"
    if [ -L "$destination" ]; then
        rollback_symlink=true
        mv -- "$destination" "$backup"
    else
        ln -- "$destination" "$backup"
    fi
    published_binary=true
    mv -f -- "$stage" "$destination"
    rollback_symlink=false
else
    published_binary=true
    ln -- "$stage" "$destination"
    rm -f -- "$stage"
fi
stage=
if [ -e "$receipt_destination" ]; then
    had_receipt=true
    ln -- "$receipt_destination" "$backup_receipt"
    receipt_backup_created=true
fi
published_receipt=true
mv -f -- "$receipt_stage" "$receipt_destination"
receipt_stage=
trap - EXIT HUP INT TERM
printf 'installed aise: %s\n' "$destination"
