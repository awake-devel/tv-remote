#!/usr/bin/env bash
# Build and install "TV du salon" (tv-remote GUI + tv-remote-tui) for the current user.
# No sudo; safe to run again (it rebuilds and overwrites the installed copies).
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="${HOME}/.local/bin"
app_dir="${XDG_DATA_HOME:-${HOME}/.local/share}/applications"
desktop_src="${here}/tv-remote.desktop"
binaries=(tv-remote tv-remote-tui)

if [[ ${EUID} -eq 0 ]]; then
    echo "error: run this as your normal user, not root" >&2
    exit 1
fi
command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found in PATH" >&2; exit 1; }
[[ -f ${desktop_src} ]] || { echo "error: missing ${desktop_src}" >&2; exit 1; }

echo "==> Building the release binaries (cargo build --release --workspace)"
(cd "${here}" && cargo build --release --workspace)

target_dir="${CARGO_TARGET_DIR:-${here}/target}"
for b in "${binaries[@]}"; do
    [[ -x "${target_dir}/release/${b}" ]] || { echo "error: ${target_dir}/release/${b} was not built" >&2; exit 1; }
done

mkdir -p "${bin_dir}" "${app_dir}"

for b in "${binaries[@]}"; do
    install -m755 "${target_dir}/release/${b}" "${bin_dir}/${b}"
    echo "==> Installed ${bin_dir}/${b}"
done

install -m644 "${desktop_src}" "${app_dir}/tv-remote.desktop"
echo "==> Installed ${app_dir}/tv-remote.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${app_dir}" >/dev/null 2>&1 || true
    echo "==> Refreshed the desktop database (${app_dir})"
else
    echo "==> update-desktop-database not found; the launcher will pick the entry up on its next scan"
fi

case ":${PATH}:" in
    *":${bin_dir}:"*) ;;
    *) echo "note: ${bin_dir} is not in your PATH; add it to run tv-remote / tv-remote-tui by name" ;;
esac

echo "Done. Launch \"TV du salon\" from the app launcher, or run tv-remote / tv-remote-tui."
