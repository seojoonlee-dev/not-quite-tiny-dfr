#!/usr/bin/env bash
# Pull the latest not-quite-tiny-dfr and reinstall it.
#
# Usage: ./update.sh [install.sh options]
#
# Fast-forwards the current branch, then runs ./install.sh with any options
# given. Stops without changing anything if the clone has local changes or
# has diverged from upstream.

# Everything runs inside main, which bash has fully read before the pull can
# rewrite this file.
main() {
    set -euo pipefail
    cd "$(dirname "$(readlink -f "$0")")"

    case ${1:-} in
        -h | --help)
            sed -n '2,8s/^# \{0,1\}//p' "$0"
            exit 0
            ;;
    esac

    err() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; }

    if [[ -n $(git status --porcelain --untracked-files=no) ]]; then
        err "the clone has uncommitted changes; commit or stash them first"
        exit 1
    fi

    printf '\033[1;34m::\033[0m %s\n' "Pulling the latest changes"
    if ! git pull --ff-only; then
        err "could not fast-forward; your branch has diverged from upstream"
        exit 1
    fi

    # The pull may have updated install.sh too; run the new one.
    exec ./install.sh "$@"
}

main "$@"
