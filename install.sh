#!/usr/bin/env bash
# Build and install "TV Remote" (tv-remote GUI + tv-remote-tui) for the current user,
# plus a daily systemd *user* timer that keeps the SmartThings OAuth login alive.
# No sudo; safe to run again (it rebuilds and overwrites the installed copies).
#
#   ./install.sh              build and install
#   ./install.sh --uninstall  remove the binaries, the launcher entry and the timer
#                             (the config in ~/.config/samsung-tv is kept)
set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="${HOME}/.local/bin"
app_dir="${XDG_DATA_HOME:-${HOME}/.local/share}/applications"
unit_dir="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"
desktop_src="${here}/tv-remote.desktop"
binaries=(tv-remote tv-remote-tui)
timer_name="tv-remote-st-refresh"

if [[ ${EUID} -eq 0 ]]; then
    echo "error: run this as your normal user, not root" >&2
    exit 1
fi

# systemctl --user, tolerating a missing user bus (e.g. over ssh or in a container).
user_systemctl() {
    systemctl --user "$@" >/dev/null 2>&1
}

# True when the systemd user manager loads the timer from *our* ${unit_dir}. With a HOME
# or XDG_CONFIG_HOME that differs from the session's (e.g. HOME=$(mktemp -d) to try the
# script), `systemctl --user` would otherwise enable or disable the session's own timer.
unit_is_ours() {
    local frag
    frag=$(systemctl --user show -P FragmentPath "${timer_name}.timer" 2>/dev/null) || return 1
    [[ -n ${frag} && ${frag} -ef "${unit_dir}/${timer_name}.timer" ]]
}

refresh_desktop_db() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "${app_dir}" >/dev/null 2>&1 || true
        echo "==> Refreshed the desktop database (${app_dir})"
    fi
}

# Daily SmartThings token refresh (refresh tokens are single-use and last ~30 days).
install_timer() {
    if ! command -v systemctl >/dev/null 2>&1; then
        echo "==> systemctl not found: no refresh timer. Run 'tv-remote-tui smartthings refresh' daily yourself"
        echo "    (the GUI also refreshes while it runs)."
        return 0
    fi
    mkdir -p "${unit_dir}"
    cat >"${unit_dir}/${timer_name}.service" <<'EOF'
[Unit]
Description=Refresh the TV Remote SmartThings OAuth tokens
# Retry a failed refresh (e.g. no network yet) every 15 min, at most 4 times in 6 h.
StartLimitIntervalSec=6h
StartLimitBurst=4

[Service]
Type=oneshot
ExecStart=%h/.local/bin/tv-remote-tui smartthings refresh --quiet
Restart=on-failure
RestartSec=15min
# Exit status 4: the login expired; retrying cannot help (log in again).
RestartPreventExitStatus=4
EOF
    cat >"${unit_dir}/${timer_name}.timer" <<'EOF'
[Unit]
Description=Daily SmartThings OAuth token refresh for TV Remote

[Timer]
OnCalendar=daily
Persistent=true
RandomizedDelaySec=1h

[Install]
WantedBy=timers.target
EOF
    echo "==> Installed ${unit_dir}/${timer_name}.{service,timer}"
    if ! systemctl --user show -P FragmentPath "${timer_name}.timer" >/dev/null 2>&1; then
        echo "note: could not reach the systemd user manager; enable the timer later with:"
        echo "      systemctl --user daemon-reload && systemctl --user enable --now ${timer_name}.timer"
    elif ! unit_is_ours && ! { user_systemctl daemon-reload && unit_is_ours; }; then
        echo "note: the systemd user manager does not load units from ${unit_dir}"
        echo "      (HOME or XDG_CONFIG_HOME differs from your session's), so the timer was not enabled."
    else
        # Pick up the rewritten unit files, then enable.
        user_systemctl daemon-reload || true
        if user_systemctl enable --now "${timer_name}.timer"; then
            echo "==> Enabled ${timer_name}.timer (daily)"
        else
            echo "note: could not enable the timer; try: systemctl --user enable --now ${timer_name}.timer"
        fi
    fi
}

uninstall() {
    local ours=0
    if command -v systemctl >/dev/null 2>&1 && unit_is_ours; then
        ours=1
        user_systemctl disable --now "${timer_name}.timer" || true
    fi
    for f in "${unit_dir}/${timer_name}.timer" "${unit_dir}/${timer_name}.service" \
        "${unit_dir}/timers.target.wants/${timer_name}.timer"; do
        if [[ -e ${f} || -L ${f} ]]; then
            rm -f -- "${f}"
            echo "==> Removed ${f}"
        fi
    done
    if ((ours)); then
        user_systemctl daemon-reload || true
    fi
    for b in "${binaries[@]}"; do
        if [[ -e "${bin_dir}/${b}" ]]; then
            rm -f -- "${bin_dir}/${b}"
            echo "==> Removed ${bin_dir}/${b}"
        fi
    done
    if [[ -e "${app_dir}/tv-remote.desktop" ]]; then
        rm -f -- "${app_dir}/tv-remote.desktop"
        echo "==> Removed ${app_dir}/tv-remote.desktop"
        refresh_desktop_db
    fi
    echo "Uninstalled. Your settings and tokens stay in ~/.config/samsung-tv/ (delete it to forget them)."
}

case "${1:-}" in
    "") ;;
    --uninstall | uninstall)
        uninstall
        exit 0
        ;;
    -h | --help)
        sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "error: unknown argument '$1' (try --help)" >&2
        exit 2
        ;;
esac

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
    refresh_desktop_db
else
    echo "==> update-desktop-database not found; the launcher will pick the entry up on its next scan"
fi

install_timer

case ":${PATH}:" in
    *":${bin_dir}:"*) ;;
    *) echo "note: ${bin_dir} is not in your PATH; add it to run tv-remote / tv-remote-tui by name" ;;
esac

echo "Done. Launch \"TV Remote\" from the app launcher, or run tv-remote / tv-remote-tui."
