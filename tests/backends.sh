#!/bin/sh

# The `pkg` provider against the package manager of the system it runs on.
#
# `tests/core.rs` stubs every package manager, which is what lets it run
# anywhere and is also why it cannot tell whether `apk add tree=1.0-r0` is how
# Alpine pins a version.  This asks the real one, inside a container of each
# distribution (`make check-backends IMAGE=...`), and is the only thing that
# does.
#
# The provider is called the way detc calls it: the verb as the argument and the
# request as compact JSON on standard input, with `name` last.  Nothing is
# built, so a run takes as long as the package manager does.
#
# `tree` is the package, because every distribution here has one by that name,
# it is small, and nothing in a container image depends on it.

set -eu

cd "$(dirname "$0")/.."
export DETC_ROOT=/

package=tree
failed=0

check() {
    if [ "$2" = "$3" ]; then
        echo "ok      $1"
    else
        echo "FAILED  $1: expected [$3], got [$2]"
        failed=$((failed + 1))
    fi
}

inspect() {
    printf '{"desired":{"installed":true},"name":"%s"}' "$package" | providers/pkg inspect
}

apply() {
    printf '{"current":null,"desired":%s,"diff":{},"name":"%s"}' "$1" "$package" \
        | providers/pkg apply > /dev/null 2>&1
}

manager=$(cd probes/pkg && ./10-manager | sed -n 's/^manager: "\(.*\)"$/\1/p')
check "the probe names a manager" "${manager:+yes}" yes
echo "        ($manager)"

check "an absent package is installed: false" "$(inspect)" "installed: false"

apply '{"installed":true}' || true
check "it is installed" "$(inspect | head -n 1)" "installed: true"

version=$(inspect | sed -n 's/^version: "\(.*\)"$/\1/p')
check "its version is reported" "${version:+yes}" yes
echo "        ($version)"

# Installing it again, pinned to the version it has, has to be accepted by the
# manager and leave that version in place
apply "{\"installed\":true,\"version\":\"$version\"}" \
    && pinned=yes || pinned=no
check "it can be pinned to that version" "$pinned" yes
check "and still reports it" "$(inspect | sed -n 's/^version: "\(.*\)"$/\1/p')" "$version"

apply '{"installed":false}' || true
check "it is removed" "$(inspect)" "installed: false"

[ "$failed" -eq 0 ] || { echo "$failed check(s) failed"; exit 1; }
