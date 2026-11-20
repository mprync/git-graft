# Install git-graft

git-graft ships as a single static binary named `git-graft` (`git-graft.exe` on Windows). Git runs it as `git graft ...` when the binary is on your `PATH`.

**Requirements:** Git 2.46 or newer on `PATH` at runtime. Check with `git --version`.

Release assets cover six targets: Linux (x86_64, aarch64; musl, statically linked), macOS (Intel, Apple silicon) and Windows (x64, ARM64). Every release publishes `SHA256SUMS` and signed build provenance for each file.

## Quick install

### macOS and Linux

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.sh | sh
```

The script downloads the archive for your platform, verifies its SHA-256 against the release `SHA256SUMS`, and installs `git-graft` to `~/.local/bin` without `sudo`. It warns if that directory is not on your `PATH` or if Git is older than 2.46.

Options:

```sh
# Install a specific version into a specific directory
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.sh \
  | sh -s -- --version 0.1.0 --to /usr/local/bin
```

| Setting | Purpose |
| --- | --- |
| `--version <v>` or `GIT_GRAFT_VERSION` | Release to install. Default: latest. |
| `--to <dir>` or `GIT_GRAFT_INSTALL_DIR` | Install directory. Default: `~/.local/bin`. |
| `GIT_GRAFT_DOWNLOAD_BASE` | Directory URL (`https://` or `file://`) holding `SHA256SUMS` and the archives, for mirrors and offline installs. |

### Windows with Git Bash

The same script works in Git Bash, MSYS2 and Cygwin and installs the Windows build:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.sh | sh
```

### Windows with PowerShell

Works on Windows PowerShell 5.1 and PowerShell 7:

```powershell
irm https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 | iex
```

To read the script before running it:

```powershell
curl.exe -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 -o install.ps1
powershell -ExecutionPolicy Bypass -File install.ps1
```

It installs to `%LOCALAPPDATA%\Programs\git-graft\bin` and adds that directory to your user `PATH` if it is missing; open a new terminal afterwards. Pass `-Version 0.1.0`, `-InstallDir <dir>` or `-NoPathUpdate`, or set `GIT_GRAFT_VERSION`, `GIT_GRAFT_INSTALL_DIR` and `GIT_GRAFT_DOWNLOAD_BASE` as described above.

## Package managers

### Homebrew (macOS and Linux)

```sh
brew install mprync/tap/git-graft
```

### Scoop (Windows)

```powershell
scoop bucket add mprync https://github.com/mprync/scoop-bucket
scoop install git-graft
```

### Chocolatey (Windows)

```powershell
choco install git-graft
```

### winget (Windows)

```powershell
winget install mprync.git-graft
```

### Debian, Ubuntu, Fedora, Alpine and other Linux distributions

Each release includes `.deb`, `.rpm`, `.apk` and Arch `.pkg.tar.zst` packages for x86_64 and aarch64. Download the file for your system from the [latest release](https://github.com/mprync/git-graft/releases/latest), then:

```sh
sudo apt install ./git-graft_0.1.0_amd64.deb          # Debian, Ubuntu
sudo dnf install ./git-graft-0.1.0-1.x86_64.rpm       # Fedora, RHEL
sudo apk add --allow-untrusted ./git-graft_0.1.0_x86_64.apk   # Alpine
sudo pacman -U ./git-graft-0.1.0-1-x86_64.pkg.tar.zst         # Arch
```

The packages install `/usr/bin/git-graft` and depend on `git`. Some distributions ship a Git older than 2.46; the package installs regardless, but git-graft needs 2.46 or newer at runtime, so upgrade Git from your distribution's backports or from git-scm.com if `git --version` is older.

### Arch User Repository

```sh
yay -S git-graft-bin
```

Any AUR helper works. The package installs the prebuilt release binary.

### Cargo

Requires Rust 1.89 or newer.

```sh
cargo install --locked git-graft
```

From a clone of the repository:

```sh
git clone https://github.com/mprync/git-graft
cd git-graft
cargo install --locked --path .
```

Confirm the install:

```sh
git graft --version
```

## Verify a download

Every release asset is listed in `SHA256SUMS`. To check a manually downloaded file:

```sh
sha256sum --check --ignore-missing SHA256SUMS            # Linux
shasum -a 256 --check --ignore-missing SHA256SUMS        # macOS
```

```powershell
(Get-FileHash .\git-graft-0.1.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash
# compare with the matching line in SHA256SUMS
```

Checksums prove the file matches the release; build provenance proves which workflow built it. Verify provenance with the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify git-graft-0.1.0-aarch64-apple-darwin.tar.gz --repo mprync/git-graft
```

The release workflow attests every asset, including `SHA256SUMS` and the install scripts. When the GitHub CLI is installed and signed in, `install.sh` and `install.ps1` run this check themselves and refuse to install an archive that fails it.

## Upgrade

| Installed with | Upgrade |
| --- | --- |
| `install.sh` / `install.ps1` | Run the install command again; it replaces the binary atomically. |
| Homebrew | `brew upgrade git-graft` |
| Scoop | `scoop update git-graft` |
| Chocolatey | `choco upgrade git-graft` |
| winget | `winget upgrade mprync.git-graft` |
| `.deb`, `.rpm`, `.apk`, Arch package | Install the newer package file. |
| AUR | Update with your AUR helper. |
| Cargo | `cargo install --locked git-graft --force` |

## Uninstall

| Installed with | Uninstall |
| --- | --- |
| `install.sh` | `rm ~/.local/bin/git-graft` (or the directory you passed to `--to`) |
| `install.ps1` | Delete `%LOCALAPPDATA%\Programs\git-graft` and remove it from your user `PATH` |
| Homebrew | `brew uninstall git-graft` |
| Scoop | `scoop uninstall git-graft` |
| Chocolatey | `choco uninstall git-graft` |
| winget | `winget uninstall mprync.git-graft` |
| Debian, Ubuntu | `sudo apt remove git-graft` |
| Fedora, RHEL | `sudo dnf remove git-graft` |
| Alpine | `sudo apk del git-graft` |
| Arch, AUR | `sudo pacman -R git-graft` or `sudo pacman -R git-graft-bin` |
| Cargo | `cargo uninstall git-graft` |

Uninstalling removes only the binary. Grafts and caches that git-graft created stay in place. To remove them first, remove your grafts and run `git graft prune --purge-cache --apply` (see [operations](operations.md)).
