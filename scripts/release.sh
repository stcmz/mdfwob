#!/bin/sh
# POSIX sh port of release.ps1. Usage: scripts/release.sh major|minor|patch [--no-push]
#   --no-push  do everything except pushing, and print the push command to run.
set -eu

die() { echo "error: $*" >&2; exit 1; }

level=${1:-}
case "$level" in
    major|minor|patch) ;;
    *) die "usage: $0 major|minor|patch [--no-push]" ;;
esac
push=1
case "${2:-}" in
    "") ;;
    --no-push) push=0 ;;
    *) die "unknown option: $2" ;;
esac

cd "$(dirname "$0")/.."

[ "$(git branch --show-current)" = "main" ] || die "Releases must be created from the main branch."
[ -z "$(git status --porcelain)" ] || die "The working tree must be clean before creating a release."

git fetch origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "Local main must exactly match origin/main."

# First `version = "x.y.z"` inside [workspace.package].
old=$(awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]"); next }
    in_pkg && /^version[ \t]*=[ \t]*"[0-9]+\.[0-9]+\.[0-9]+"/ {
        match($0, /[0-9]+\.[0-9]+\.[0-9]+/)
        print substr($0, RSTART, RLENGTH)
        exit
    }' Cargo.toml)
[ -n "$old" ] || die "Could not find workspace.package.version in Cargo.toml."

IFS=. read -r major minor patch <<EOV
$old
EOV
case "$level" in
    major) major=$((major + 1)); minor=0; patch=0 ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    patch) patch=$((patch + 1)) ;;
esac
new="$major.$minor.$patch"
tag="v$new"

if git show-ref --verify --quiet "refs/tags/$tag"; then
    die "Tag $tag already exists."
fi

# Rewrite `version = "OLD"` -> `version = "NEW"` in a file; fail if nothing changed.
bump() {
    sed "s/version = \"$old\"/version = \"$new\"/g" "$1" > "$1.tmp"
    if cmp -s "$1" "$1.tmp"; then
        rm -f "$1.tmp"
        die "$1: version $old not found."
    fi
    mv "$1.tmp" "$1"
}

bump Cargo.toml

# Keep the mdfwob -> mdfwob-core path dependency's version requirement in lockstep with the release
# (the members inherit the workspace version, but this internal requirement is a separate literal).
core=crates/mdfwob/Cargo.toml
sed "s/\(mdfwob-core = { version = \"\)[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\(\"\)/\1$new\2/" "$core" > "$core.tmp"
if cmp -s "$core" "$core.tmp"; then
    rm -f "$core.tmp"
    die "Could not update the mdfwob-core dependency version in $core."
fi
mv "$core.tmp" "$core"

cargo update --workspace
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
cargo build --release --locked

git add Cargo.toml Cargo.lock crates/mdfwob/Cargo.toml
git commit -m "Release $new"
git tag -a "$tag" -m "mdfwob $new"

if [ "$push" -eq 1 ]; then
    git push --atomic origin main "$tag"
    echo "Released $tag."
else
    echo "Tagged $tag locally. Publish with: git push --atomic origin main $tag"
fi
