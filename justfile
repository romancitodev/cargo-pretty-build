set shell := ["nu", "-c"]

DEFAULT_BUMP := "patch"

[private]
default:
    just --list

# Bump the version in Cargo.toml (level: patch | minor | major), sync Cargo.lock and commit.
[private]
bump level=DEFAULT_BUMP:
    #!nu
    let content = (open --raw Cargo.toml)
    let current = ($content | parse -r 'version = "(?P<v>[0-9]+\.[0-9]+\.[0-9]+)"').v.0
    let parts = ($current | split row "." | each { |x| $x | into int })
    let major = $parts.0
    let minor = $parts.1
    let patch = $parts.2
    let new = if "{{level}}" == "major" {
        $"($major + 1).0.0"
    } else if "{{level}}" == "minor" {
        $"($major).($minor + 1).0"
    } else if "{{level}}" == "patch" {
        $"($major).($minor).($patch + 1)"
    } else {
        error make { msg: $"unknown level: {{level}} \(use patch, minor o major\)" }
    }
    $content | str replace $"version = \"($current)\"" $"version = \"($new)\"" | save -f Cargo.toml
    cargo check --quiet
    let msg_file = $"($env.TEMP)/cargo-pretty-commit-msg.txt"
    $"🔖 chore: bump version to v($new)" | save -f $msg_file
    git add Cargo.toml Cargo.lock
    git commit -F $msg_file
    rm $msg_file
    print $"✅ ($current) -> ($new)"

[group('bump')]
bump-patch: (bump "patch")
[group('bump')]
bump-minor: (bump "minor")
[group('bump')]
bump-major: (bump "major")

# Create an annotated tag from the current Cargo.toml version and push branch + tag.
# Pushing the tag triggers .github/workflows/release.yml (publish + GH release + binaries).
[group('tags')]
tag:
    #!nu
    let version = ((open Cargo.toml).package.version)
    let tagname = $"v($version)"
    let msg_file = $"($env.TEMP)/cargo-pretty-tag-msg.txt"
    $"Release ($tagname)" | save -f $msg_file
    git tag -a $tagname -F $msg_file
    rm $msg_file
    git push
    git push origin $tagname
    print $"🏷️ pusheado ($tagname)"

# Bump + tag + push in one shot. level: patch | minor | major
[private]
release level=DEFAULT_BUMP: (bump level) tag

[group('release')]
release-patch: (release "patch")
[group('release')]
release-minor: (release "minor")
[group('release')]
release-major: (release "major")
