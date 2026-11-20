#!/bin/sh
# git-graft installer for macOS, Linux and Windows (Git Bash / MSYS2 / Cygwin).
#
#   curl --proto '=https' --tlsv1.2 -fsSL \
#     https://raw.githubusercontent.com/mprync/git-graft/main/install.sh | sh
#
# Options:
#   --version <v>   Install a specific release (for example 0.1.0). Default: latest.
#   --to <dir>      Install directory. Default: $GIT_GRAFT_INSTALL_DIR or ~/.local/bin.
#   --help          Show this help.
#
# Environment:
#   GIT_GRAFT_VERSION        Same as --version.
#   GIT_GRAFT_INSTALL_DIR    Same as --to.
#   GIT_GRAFT_DOWNLOAD_BASE  Directory URL holding SHA256SUMS and the archives, instead
#                            of GitHub Releases. https:// or file:// (used by smoke tests).
#
# The archive is verified against the release SHA256SUMS before anything is installed.
# No sudo is used. The whole script lives in functions and `main "$@"` is the last line,
# so a truncated download never runs partially.
set -eu

# Change the repository here; everything else derives from it.
REPO="mprync/git-graft"
BINARY="git-graft"
MIN_GIT="2.46"

say() { printf '%s\n' "$*"; }
warn() { printf 'git-graft install: warning: %s\n' "$*" >&2; }
die() {
  printf 'git-graft install: error: %s\n' "$*" >&2
  exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }

usage() {
  cat <<EOF
Install git-graft (${REPO}).

Usage: install.sh [--version <v>] [--to <dir>] [--help]

  --version <v>   Release to install (for example 0.1.0). Default: latest.
  --to <dir>      Install directory. Default: \$GIT_GRAFT_INSTALL_DIR or \$HOME/.local/bin.
  --help          Show this help.

Environment: GIT_GRAFT_VERSION, GIT_GRAFT_INSTALL_DIR, GIT_GRAFT_DOWNLOAD_BASE.
Requires Git ${MIN_GIT}+ on PATH at runtime. No sudo is used.
EOF
}

cleanup() {
  if [ -n "${TMP_ROOT:-}" ]; then
    rm -rf "$TMP_ROOT"
  fi
  if [ -n "${STAGED_FILE:-}" ]; then
    rm -f "$STAGED_FILE"
  fi
  return 0
}

# Sets TARGET (Rust triple), EXT (archive extension) and EXE (binary file name).
detect_target() {
  os=$(uname -s 2>/dev/null || echo unknown)
  arch=$(uname -m 2>/dev/null || echo unknown)
  case "$os" in
    Linux)
      EXT="tar.gz"
      EXE="$BINARY"
      case "$arch" in
        x86_64 | amd64) TARGET="x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) TARGET="aarch64-unknown-linux-musl" ;;
        *) die "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    Darwin)
      EXT="tar.gz"
      EXE="$BINARY"
      # A shell running under Rosetta reports x86_64 on Apple silicon; prefer native.
      if [ "$arch" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
        arch="arm64"
      fi
      case "$arch" in
        x86_64) TARGET="x86_64-apple-darwin" ;;
        arm64 | aarch64) TARGET="aarch64-apple-darwin" ;;
        *) die "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    MINGW* | MSYS* | CYGWIN*)
      EXT="zip"
      EXE="$BINARY.exe"
      # uname reports the emulated architecture under Git Bash on ARM64; ask Windows.
      winarch="${PROCESSOR_ARCHITEW6432:-${PROCESSOR_ARCHITECTURE:-$arch}}"
      case "$winarch" in
        AMD64 | x86_64 | amd64) TARGET="x86_64-pc-windows-msvc" ;;
        ARM64 | aarch64 | arm64) TARGET="aarch64-pc-windows-msvc" ;;
        *) die "unsupported Windows architecture: $winarch" ;;
      esac
      ;;
    *) die "unsupported operating system: $os (see docs/install.md for other options)" ;;
  esac
}

# fetch <url> <destination file>
fetch() {
  url=$1
  out=$2
  case "$url" in
    file://*)
      # Local directory override, used by smoke tests. file:///C:/x -> C:/x on Windows.
      src=${url#file://}
      case "$src" in
        /[A-Za-z]:*) src=${src#/} ;;
      esac
      [ -f "$src" ] || die "not found: $src"
      cp "$src" "$out" || die "could not read $src"
      ;;
    https://*)
      if have curl; then
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$out" "$url" ||
          die "download failed: $url"
      elif have wget; then
        if wget --help 2>&1 | grep -q 'secure-protocol'; then
          wget --https-only --secure-protocol=TLSv1_2 -q -O "$out" "$url" ||
            die "download failed: $url"
        else
          wget --https-only -q -O "$out" "$url" || die "download failed: $url"
        fi
      else
        die "curl or wget is required"
      fi
      ;;
    *) die "refusing to download from a non-https URL: $url" ;;
  esac
}

# SHA256SUMS comes from the same release as the archive. When the GitHub CLI
# is installed and signed in, also check the archive's signed build provenance,
# which proves it was built by this repository's release workflow.
verify_provenance() {
  case $BASE in https://github.com/*) ;; *) return 0 ;; esac
  if have gh && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$1" --repo "$REPO" >/dev/null 2>&1 ||
      die "build provenance check failed for $(basename "$1"); nothing was installed"
    say "Verified build provenance"
  fi
}

sha256_of() {
  if have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif have shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif have openssl; then
    openssl dgst -sha256 "$1" | awk '{print $NF}'
  else
    die "sha256sum, shasum or openssl is required to verify the download"
  fi
}

# Prints "<hash> <file name>" for the single SHA256SUMS entry matching this platform.
resolve_asset() {
  sums=$1
  if [ -n "$VERSION" ]; then
    matches=$(awk -v want="$BINARY-$VERSION-$TARGET.$EXT" \
      '{ n = $2; sub(/^\*/, "", n); if (n == want) print $1 " " n }' "$sums")
  else
    matches=$(awk -v prefix="$BINARY-" -v suffix="-$TARGET.$EXT" \
      '{ n = $2; sub(/^\*/, "", n);
         if (index(n, prefix) == 1 && length(n) > length(suffix) &&
             substr(n, length(n) - length(suffix) + 1) == suffix) print $1 " " n }' "$sums")
  fi
  [ -n "$matches" ] || die "SHA256SUMS has no entry for ${VERSION:+version $VERSION, }target $TARGET"
  if [ "$(printf '%s\n' "$matches" | awk 'END { print NR }')" != "1" ]; then
    die "SHA256SUMS has more than one entry for target $TARGET"
  fi
  printf '%s\n' "$matches"
}

# extract <archive> <destination directory>
extract() {
  archive=$1
  dest=$2
  case "$EXT" in
    tar.gz) tar -xzf "$archive" -C "$dest" || die "could not extract $archive" ;;
    zip)
      if have unzip; then
        unzip -q "$archive" -d "$dest" || die "could not extract $archive"
      elif [ -x "${SYSTEMROOT:-/c/Windows}/System32/tar.exe" ]; then
        # Windows ships bsdtar, which reads zip archives; Git Bash's GNU tar does not.
        "${SYSTEMROOT:-/c/Windows}/System32/tar.exe" -xf "$archive" -C "$dest" ||
          die "could not extract $archive"
      elif have powershell.exe && have cygpath; then
        powershell.exe -NoProfile -Command \
          "Expand-Archive -LiteralPath '$(cygpath -w "$archive")' -DestinationPath '$(cygpath -w "$dest")' -Force" ||
          die "could not extract $archive"
      else
        die "unzip (or Windows tar.exe) is required to extract the zip archive"
      fi
      ;;
  esac
}

# install_binary <source file> <destination directory>: write beside the target, then rename.
install_binary() {
  src=$1
  dest_dir=$2
  mkdir -p "$dest_dir" || die "cannot create $dest_dir"
  STAGED_FILE=$(mktemp "$dest_dir/.$EXE.XXXXXX") || die "cannot write to $dest_dir"
  cp "$src" "$STAGED_FILE" || die "cannot write to $dest_dir"
  chmod 755 "$STAGED_FILE"
  mv -f "$STAGED_FILE" "$dest_dir/$EXE" || die "cannot replace $dest_dir/$EXE (is it running?)"
  STAGED_FILE=""
}

check_git() {
  if ! have git; then
    warn "git was not found on PATH; git-graft needs Git ${MIN_GIT} or newer at runtime."
    return 0
  fi
  git_version=$(git --version 2>/dev/null | awk '{ print $3 }')
  major=${git_version%%.*}
  rest=${git_version#*.}
  minor=${rest%%.*}
  case "$major$minor" in
    '' | *[!0-9]*) return 0 ;;
  esac
  want_major=${MIN_GIT%%.*}
  want_minor=${MIN_GIT#*.}
  if [ "$major" -lt "$want_major" ] || { [ "$major" -eq "$want_major" ] && [ "$minor" -lt "$want_minor" ]; }; then
    warn "found Git $git_version; git-graft needs Git ${MIN_GIT} or newer at runtime."
  fi
}

path_hint() {
  case ":${PATH:-}:" in
    *":$1:"*) ;;
    *)
      say ""
      say "$1 is not on your PATH. Add it, for example:"
      say "  export PATH=\"$1:\$PATH\""
      ;;
  esac
}

main() {
  TMP_ROOT=""
  STAGED_FILE=""
  VERSION="${GIT_GRAFT_VERSION:-}"
  INSTALL_DIR="${GIT_GRAFT_INSTALL_DIR:-${HOME:-}/.local/bin}"
  BASE="${GIT_GRAFT_DOWNLOAD_BASE:-}"

  while [ $# -gt 0 ]; do
    case "$1" in
      -h | --help)
        usage
        return 0
        ;;
      --version | -v)
        [ $# -ge 2 ] || die "--version needs a value"
        VERSION=$2
        shift 2
        ;;
      --version=*)
        VERSION=${1#--version=}
        shift
        ;;
      --to)
        [ $# -ge 2 ] || die "--to needs a directory"
        INSTALL_DIR=$2
        shift 2
        ;;
      --to=*)
        INSTALL_DIR=${1#--to=}
        shift
        ;;
      *) die "unknown option: $1 (try --help)" ;;
    esac
  done

  VERSION=${VERSION#v}
  case "$VERSION" in
    *[!0-9A-Za-z.+-]*) die "invalid version: $VERSION" ;;
  esac
  [ -n "$INSTALL_DIR" ] || die "no install directory: set HOME, GIT_GRAFT_INSTALL_DIR or pass --to"

  detect_target

  if [ -n "$BASE" ]; then
    BASE=${BASE%/}
    SUMS_URL="$BASE/SHA256SUMS"
  elif [ -n "$VERSION" ]; then
    BASE="https://github.com/$REPO/releases/download/v$VERSION"
    SUMS_URL="$BASE/SHA256SUMS"
  else
    BASE="https://github.com/$REPO/releases/latest/download"
    SUMS_URL="$BASE/SHA256SUMS"
  fi

  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/git-graft-install.XXXXXX") || die "cannot create a temporary directory"

  say "Installing git-graft for $TARGET"
  fetch "$SUMS_URL" "$TMP_ROOT/SHA256SUMS"
  entry=$(resolve_asset "$TMP_ROOT/SHA256SUMS")
  expected=${entry%% *}
  asset=${entry#* }
  case "$asset" in
    */* | *\\*) die "unexpected file name in SHA256SUMS: $asset" ;;
  esac
  [ "${#expected}" -eq 64 ] || die "malformed checksum for $asset in SHA256SUMS"
  case "$expected" in
    *[!0-9a-fA-F]*) die "malformed checksum for $asset in SHA256SUMS" ;;
  esac

  fetch "$BASE/$asset" "$TMP_ROOT/$asset"
  actual=$(sha256_of "$TMP_ROOT/$asset")
  expected_lc=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
  actual_lc=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
  if [ "$actual_lc" != "$expected_lc" ]; then
    die "checksum mismatch for $asset (expected $expected_lc, got ${actual_lc:-nothing}); nothing was installed"
  fi
  say "Verified SHA-256 of $asset"
  verify_provenance "$TMP_ROOT/$asset"

  mkdir "$TMP_ROOT/extract"
  extract "$TMP_ROOT/$asset" "$TMP_ROOT/extract"
  stem=${asset%."$EXT"}
  if [ -f "$TMP_ROOT/extract/$stem/$EXE" ]; then
    src="$TMP_ROOT/extract/$stem/$EXE"
  elif [ -f "$TMP_ROOT/extract/$EXE" ]; then
    src="$TMP_ROOT/extract/$EXE"
  else
    die "$EXE not found inside $asset"
  fi

  install_binary "$src" "$INSTALL_DIR"
  say "Installed $INSTALL_DIR/$EXE"
  if "$INSTALL_DIR/$EXE" --version 2>/dev/null; then
    :
  else
    warn "installed binary did not run here; check that it matches this machine"
  fi

  check_git
  path_hint "$INSTALL_DIR"
  say ""
  say "Run: git graft --help"
}

main "$@"
