#!/bin/sh
# Installs Reason Network for macOS or Linux: with the Reason CLI (sign in), or on its own (no account).
#
#   curl -fsSL <this url> | sh                      install, then set up (with Reason, or start or join a team)
#   curl -fsSL <this url> | sh -s -- <team link>    install, then join that team
#   curl -fsSL <this url> | sh -s -- <team link> --as <name>     the same, without questions (for agents)
#   curl -fsSL <this url> | sh -s -- init <team> --as <name>     install and start a team, without questions
#
# Environment: REASON_NETWORK_BIN_DIR (default ~/.local/bin), REASON_NETWORK_VERSION (a tag like v0.10.4; default latest), REASON_NETWORK_REPO,
# REASON_NETWORK_DOWNLOAD_BASE (a mirror serving the release files), REASON_NETWORK_NO_PROMPT (never ask anything: updates).
set -eu

repo="${REASON_NETWORK_REPO:-reason-machines/reason-network}"
bin_dir="${REASON_NETWORK_BIN_DIR:-$HOME/.local/bin}"
version="${REASON_NETWORK_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() { say "reason-network install: $*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  *) die "reason-network runs on macOS and Linux here (on Windows, use install.ps1)" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch=aarch64 ;;
  x86_64 | amd64) arch=x86_64 ;;
  *) die "unsupported CPU: $(uname -m)" ;;
esac
target="$arch-$os"

# Starting fresh in a terminal: Reason first. With it, your Reason workspace is the team (no links to pass around),
# your agents reach Reason's cloud agents, and the Reason CLI installs and updates this engine. Without it, a team
# link and no account. Never asked when joining with a link, for agents, or for updates.
if [ $# -eq 0 ] && [ -z "${REASON_NETWORK_NO_PROMPT:-}" ] && [ ! -e "$bin_dir/reason-network" ] && (exec </dev/tty) 2>/dev/null; then
  say "Use your Reason account? Your workspace becomes the team, no links to pass around."
  printf 'Answer n for no account and a team link. [Y/n] '
  read -r answer </dev/tty || answer=n
  case "$answer" in
    [nN]*) ;;
    *)
      reason="$(command -v reason || true)"
      if [ -z "$reason" ]; then
        installer="$(curl -fsSL https://reasonmachines.com/install.sh)" && printf '%s\n' "$installer" | bash -s -- --minimal --no-login ||
          die "installing Reason failed; run this again and answer n to go without an account"
        reason="${REASON_INSTALL_DIR:-${ARA_INSTALL_DIR:-$HOME/.local/bin}}/reason"
        [ -x "$reason" ] || die "Reason installed, but not at $reason; run: reason network"
      fi
      exec "$reason" network </dev/tty
      ;;
  esac
fi

if [ -n "${REASON_NETWORK_DOWNLOAD_BASE:-}" ]; then
  base="$REASON_NETWORK_DOWNLOAD_BASE"
elif [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading reason-network for $target..."
curl -fsSL "$base/reason-network-$target.tar.gz" -o "$tmp/reason-network.tar.gz" || die "download failed ($base/reason-network-$target.tar.gz)"
curl -fsSL "$base/reason-network-$target.tar.gz.sha256" -o "$tmp/sum" || die "checksum download failed; not installing"
want="$(cut -d' ' -f1 < "$tmp/sum")"
if command -v shasum >/dev/null 2>&1; then
  got="$(shasum -a 256 "$tmp/reason-network.tar.gz" | cut -d' ' -f1)"
else
  got="$(sha256sum "$tmp/reason-network.tar.gz" | cut -d' ' -f1)"
fi
[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch; not installing"
tar -xzf "$tmp/reason-network.tar.gz" -C "$tmp"
"$tmp/reason-network" --version >/dev/null 2>&1 || die "the downloaded binary doesn't run here; not installing"
mkdir -p "$bin_dir"
# A new file moved into place: whatever runs the old one (a hook, an agent's tools) never sees a half-written binary.
install -m 755 "$tmp/reason-network" "$bin_dir/.reason-network.new"
mv -f "$bin_dir/.reason-network.new" "$bin_dir/reason-network"
say "Installed $bin_dir/reason-network ($("$bin_dir/reason-network" --version))"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) say "Note: $bin_dir is not on your PATH. Add this to your shell profile:"
     say "  export PATH=\"$bin_dir:\$PATH\"" ;;
esac

rm -rf "$tmp"
trap - EXIT INT TERM

# Hand over to reason-network: it asks a few questions (name, invite code, which agents) in your terminal. Under
# `curl | sh` our stdin is the script, so read answers from the terminal itself when there is one.
# (In a subshell: in dash, Ubuntu's sh, a failed redirection on `:` would end the whole script.)
if [ -z "${REASON_NETWORK_NO_PROMPT:-}" ] && (exec </dev/tty) 2>/dev/null; then
  case "${1:-}" in
    "") exec "$bin_dir/reason-network" </dev/tty ;;
    -*|init|join|setup) exec "$bin_dir/reason-network" "$@" </dev/tty ;;
    *) exec "$bin_dir/reason-network" join "$@" </dev/tty ;;
  esac
fi
case "${1:-}" in
  # Nothing could be asked (an agent's shell, say): what to run instead. Not on an update, which runs this too.
  "") say ""; say "Next, in a terminal: reason-network"
      if [ -z "${REASON_NETWORK_NO_PROMPT:-}" ]; then
        say "From an agent's shell, with a Reason account: curl -fsSL https://reasonmachines.com/install.sh | bash -s -- --minimal,"
        say "then reason network setup. Without one: $bin_dir/reason-network join '<team link>' --as <name>, or"
        say "$bin_dir/reason-network init '<team>' --as <name> to start a team."
      fi ;;
  -*|init|join|setup) exec "$bin_dir/reason-network" "$@" ;;
  *) exec "$bin_dir/reason-network" join "$@" ;;
esac
