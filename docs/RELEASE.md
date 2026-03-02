# Release Process

## First-Time Setup

### 1. Create the Homebrew tap repository

Create a public GitHub repo: **aitechnerd/homebrew-sshore**

```bash
gh repo create aitechnerd/homebrew-sshore --public --description "Homebrew tap for sshore"
# Create the Formula directory
git clone https://github.com/aitechnerd/homebrew-sshore.git
cd homebrew-sshore
mkdir Formula
touch Formula/.gitkeep
git add . && git commit -m "Init tap" && git push
```

### 2. Create a GitHub PAT for the tap

Create a fine-grained Personal Access Token:
- Go to https://github.com/settings/tokens?type=beta
- Scope: **Contents read/write** on `aitechnerd/homebrew-sshore` only
- Name it something like `sshore-homebrew-tap`

### 3. Add the secret to the sshore repo

Go to https://github.com/aitechnerd/sshore/settings/secrets/actions and add:
- **Name:** `HOMEBREW_TAP_TOKEN`
- **Value:** the PAT from step 2

### 4. (Optional) AUR account

If you want to publish to AUR:
- Create an account at https://aur.archlinux.org
- Set up SSH keys for AUR push access
- Register the `sshore-bin` package

## How to Release

### 1. Update version in Cargo.toml

```bash
# Edit Cargo.toml: version = "X.Y.Z"
cargo build  # verify it compiles
```

### 2. Tag and push

```bash
git add Cargo.toml Cargo.lock
git commit -m "Bump version to X.Y.Z"
git push origin master
git tag vX.Y.Z
git push origin vX.Y.Z
```

## What Happens Automatically

When the tag is pushed, the `Release` workflow runs:

1. **Build** — compiles for 5 targets (x86_64/aarch64 Linux, x86_64/aarch64 macOS, x86_64 Windows)
2. **Release** — downloads all artifacts, generates `checksums.sha256`, creates a GitHub Release with release notes
3. **Publish Homebrew** — downloads the 4 Unix archives, computes SHA256 hashes, generates `Formula/sshore.rb`, pushes to `aitechnerd/homebrew-sshore`

After ~5–10 minutes, users can:

```bash
brew tap aitechnerd/sshore && brew install sshore
```

## Manual Steps

### AUR (Arch Linux)

The AUR PKGBUILD is in `packaging/aur/PKGBUILD`. After the GitHub release is published:

```bash
cd packaging/aur/

# Update version
sed -i "s/pkgver=.*/pkgver=X.Y.Z/" PKGBUILD

# Update checksums (downloads the archives and computes sha256)
updpkgsums

# Test the build locally
makepkg -si

# Push to AUR
# (requires AUR SSH key setup and package registration)
```

### crates.io

```bash
cargo publish --dry-run  # verify
cargo publish
```

## Verification Checklist

After a release, verify these work:

- [ ] GitHub Release page shows all 5 archives + `checksums.sha256`
- [ ] `checksums.sha256` matches actual file hashes
- [ ] `brew tap aitechnerd/sshore && brew install sshore` installs successfully
- [ ] `sshore --version` shows the correct version
- [ ] Direct binary download works:
  ```bash
  curl -L https://github.com/aitechnerd/sshore/releases/latest/download/sshore-aarch64-apple-darwin.tar.gz | tar xz
  ./sshore-aarch64-apple-darwin/sshore --version
  ```
- [ ] Shell completions work: `sshore completions bash` produces output
