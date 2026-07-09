param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("major", "minor", "patch")]
    [string] $Level
)

$ErrorActionPreference = "Stop"

function Assert-NativeSuccess([string] $Operation) {
    if ($LASTEXITCODE -ne 0) {
        throw "$Operation failed with exit code $LASTEXITCODE."
    }
}

$branch = git branch --show-current
Assert-NativeSuccess "Reading the current branch"
if ($branch -ne "main") {
    throw "Releases must be created from the main branch."
}
$status = git status --porcelain
Assert-NativeSuccess "Reading the working tree status"
if ($status) {
    throw "The working tree must be clean before creating a release."
}

git fetch origin main
Assert-NativeSuccess "Fetching origin/main"
$head = git rev-parse HEAD
Assert-NativeSuccess "Reading local HEAD"
$originHead = git rev-parse origin/main
Assert-NativeSuccess "Reading origin/main"
if ($head -ne $originHead) {
    throw "Local main must exactly match origin/main."
}

$manifest = Get-Content Cargo.toml -Raw
$match = [regex]::Match(
    $manifest,
    '(?ms)^\[workspace\.package\]\s+version = "(\d+)\.(\d+)\.(\d+)"'
)
if (-not $match.Success) {
    throw "Could not find the workspace version in Cargo.toml."
}

$major = [int] $match.Groups[1].Value
$minor = [int] $match.Groups[2].Value
$patch = [int] $match.Groups[3].Value
switch ($Level) {
    "major" {
        $major++
        $minor = 0
        $patch = 0
    }
    "minor" {
        $minor++
        $patch = 0
    }
    "patch" {
        $patch++
    }
}

$oldVersion = $match.Groups[1].Value + "." +
    $match.Groups[2].Value + "." +
    $match.Groups[3].Value
$newVersion = "$major.$minor.$patch"
$tag = "v$newVersion"

git show-ref --verify --quiet "refs/tags/$tag"
if ($LASTEXITCODE -eq 0) {
    throw "Tag $tag already exists."
}
if ($LASTEXITCODE -ne 1) {
    throw "Checking tag $tag failed with exit code $LASTEXITCODE."
}

$updatedManifest = $manifest.Replace(
    "version = `"$oldVersion`"",
    "version = `"$newVersion`""
)
if ($updatedManifest -eq $manifest) {
    throw "Cargo.toml version was not updated."
}
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
[System.IO.File]::WriteAllText(
    (Resolve-Path "Cargo.toml"),
    $updatedManifest,
    $utf8NoBom
)

# Keep the mdfwob -> mdfwob-core path dependency's version requirement in lockstep with the release
# (the members inherit the workspace version, but this internal requirement is a separate literal).
$corePath = "crates/mdfwob/Cargo.toml"
$coreManifest = Get-Content $corePath -Raw
$updatedCore = [regex]::Replace(
    $coreManifest,
    '(mdfwob-core = \{ version = ")\d+\.\d+\.\d+(")',
    "`${1}$newVersion`${2}"
)
if ($updatedCore -eq $coreManifest) {
    throw "Could not update the mdfwob-core dependency version in $corePath."
}
[System.IO.File]::WriteAllText((Resolve-Path $corePath), $updatedCore, $utf8NoBom)

cargo update --workspace
Assert-NativeSuccess "Updating Cargo.lock"
cargo fmt --all --check
Assert-NativeSuccess "Checking formatting"
cargo clippy --all-targets --all-features --locked -- -D warnings
Assert-NativeSuccess "Running clippy"
cargo test --all-features --locked
Assert-NativeSuccess "Running tests"
cargo build --release --locked
Assert-NativeSuccess "Building the release binary"

git add Cargo.toml Cargo.lock crates/mdfwob/Cargo.toml
Assert-NativeSuccess "Staging the version update"
git commit -m "Release $newVersion"
Assert-NativeSuccess "Committing the version update"
git tag -a $tag -m "mdfwob $newVersion"
Assert-NativeSuccess "Creating tag $tag"
git push --atomic origin main $tag
Assert-NativeSuccess "Pushing the release commit and tag"

Write-Host "Released $tag."
