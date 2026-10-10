#!/usr/bin/env bash
# Scan the repository for things that should not be pushed: private keys,
# access tokens, passwords and other credentials, email addresses other than
# the repository's authors, the local user name and host name, private links, and
# key-like, large or binary files.
#
# Usage:
#   scripts/check-secrets.sh            tracked files as they are in the working tree
#   scripts/check-secrets.sh --push     also the lines added by commits not yet
#                                       pushed (@{upstream}..HEAD, or a range given
#                                       after --push), since a secret added and then
#                                       deleted is still in the history pushed
#
# Exit status: 0 when nothing is found, 1 when something is, 2 on a usage error.
#
# A line that is a known false positive ends with a comment containing
# `secret-scan: allow`. Whole paths are skipped with `scripts/secret-scan-skip`
# (one git pathspec per line, `#` comments).
#
# As a pre-push hook (optional, per clone):
#   ln -s ../../scripts/check-secrets.sh .git/hooks/pre-push
# git then runs it with arguments of its own; it scans the unpushed commits.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

range=""
case "${1:-}" in
    "") ;;
    --push) range=${2:-'@{upstream}..HEAD'} ;;
    --help | -h) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)
        # Run as a pre-push hook: git passes the remote's name and URL.
        if [ -n "${GIT_DIR:-}" ] || [ "$#" -eq 2 ]; then range='@{upstream}..HEAD'; else
            echo "usage: $0 [--push [range]]" >&2; exit 2
        fi ;;
esac

skip=(':!*Cargo.lock')
if [ -f scripts/secret-scan-skip ]; then
    while IFS= read -r line; do
        line=${line%%#*}; line=${line// /}
        [ -n "$line" ] && skip+=(":!$line")
    done < scripts/secret-scan-skip
fi

user=$(id -un)
host=$(hostname -s 2>/dev/null || hostname)
allowed_emails=$(
    {
        git config user.email || true
        # The repository's own authors and committers (CI has no user.email).
        git log --format='%ae%n%ce' 2> /dev/null | sort -u || true
        echo 'noreply@anthropic.com'
        echo 'noreply@github.com'
    } | tr 'A-Z' 'a-z'
)

found=0
report() { # rule, location, text
    found=1
    printf '%-14s %s  %s\n' "$1" "$2" "$(printf '%s' "$3" | cut -c1-100)"
}

# Lines that could match a rule, as "location<TAB>text": a cheap filter
# first, the exact rules below.
candidates="@|PRIVATE KEY|PuTTY|gh[pousr]_|github_pat_|AKIA|ASIA|xox|AIza|sk-|glpat-|eyJ|pass|secret|token|key|://|claude\\.ai|$user|$host"
lines() {
    git grep -I -n -i -E -e "$candidates" -- . "${skip[@]}" | sed 's/:/\t/2' || true
    if [ -n "$range" ]; then
        if ! git rev-parse -q --verify "${range%%..*}" > /dev/null 2>&1; then
            echo "check-secrets: no upstream for $range; scanning the working tree only" >&2
            return
        fi
        git log -p --no-color --format='commit %h' "$range" -- . "${skip[@]}" | awk '
            /^commit / { c = $2; next }
            /^\+\+\+ b\// { f = substr($0, 7); next }
            /^\+/ && !/^\+\+\+/ { print c ":" f "\t" substr($0, 2) }' | grep -i -E -e "$candidates" || true
    fi
}

private_key='-----BEGIN ([A-Z0-9]+ )*PRIVATE KEY( BLOCK)?-----|PuTTY-User-Key-File' # secret-scan: allow
token='gh[pousr]_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,}|(AKIA|ASIA)[0-9A-Z]{16}|xox[abposr]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{35}|sk-ant-[A-Za-z0-9_-]{20,}|sk-[A-Za-z0-9]{32,}|glpat-[A-Za-z0-9_-]{20}|eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.' # secret-scan: allow
credential="(password|passwd|passphrase|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret)[\"']?[[:space:]]*[:=][[:space:]]*[\"'][^\"'[:space:]]{8,}[\"']"
url_credential='[a-z][a-z0-9+.-]*://[^/[:space:]:@"'\'']+:[^/[:space:]@"'\'']+@'
private_link='claude\.ai/(code/)?(artifact|chat|project|share)s?/'
identity="/home/$user([/\"' ]|\$)|/Users/$user([/\"' ]|\$)|\\b$host\\b"
email='[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'

while IFS=$'\t' read -r loc text; do
    case "$text" in *secret-scan:\ allow*) continue ;; esac
    if [[ $text =~ $private_key ]]; then report private-key "$loc" "$text"; fi
    if [[ $text =~ $token ]]; then report token "$loc" "${BASH_REMATCH[0]:0:12}…"; fi
    shopt -s nocasematch
    if [[ $text =~ $credential ]]; then report credential "$loc" "$text"; fi
    shopt -u nocasematch
    if [[ $text =~ $url_credential ]]; then report url-password "$loc" "$text"; fi
    if [[ $text =~ $private_link ]]; then report private-link "$loc" "$text"; fi
    if [[ $text =~ $identity ]]; then report local-name "$loc" "$text"; fi
    rest=$text
    while [[ $rest =~ $email ]]; do
        addr=${BASH_REMATCH[0]}
        rest=${rest#*"$addr"}
        lower=$(printf '%s' "$addr" | tr 'A-Z' 'a-z')
        case "$lower" in
            *@example.com | *@example.org | *@example.net | *.example | *@localhost) continue ;;
        esac
        # Version requirements and the like (`name@1.2.3`) are not addresses.
        [[ $lower =~ @[0-9.]+$ ]] && continue
        grep -qxF "$lower" <<< "$allowed_emails" || report email "$loc" "$addr"
    done
done < <(lines)

# Files: names that usually hold keys, and large or binary files.
while IFS= read -r path; do
    base=${path##*/}
    case "$base" in
        *.key | *.pem | *.p12 | *.pfx | *.jks | *.keystore | *.kdbx | *.ppk | .env | .env.* | \
        id_rsa* | id_dsa* | id_ecdsa* | id_ed25519* | .netrc | .npmrc | .pypirc | credentials*)
            report key-file "$path" "file name" ;;
    esac
    [ -f "$path" ] || continue
    size=$(wc -c < "$path")
    if [ "$size" -gt 1048576 ]; then report large-file "$path" "$size bytes"; fi
    if [ "$size" -gt 0 ] && ! grep -qI . "$path"; then report binary-file "$path" "binary"; fi
done < <(git ls-files -- . "${skip[@]}")

if [ "$found" -eq 0 ]; then
    echo "check-secrets: nothing found${range:+ (including $range)}"
else
    echo "check-secrets: review the lines above; mark false positives with 'secret-scan: allow'" >&2
    exit 1
fi
