
# ---------------------------------------------------------------------
# bougie telemetry consent block.
#
# Appended to the dist-generated installer by publish-mirror.yml when
# promoting to installers/bougie/latest/ — only that channel carries
# it; the versioned mirror artifacts and GitHub release assets stay
# byte-identical to dist's output. Runs only after a successful
# install (the dist entrypoint line above is `... || exit 1`).
#
# Contract: writes the same mode file bougie reads
# (`${XDG_CONFIG_HOME:-$HOME/.config}/bougie/telemetry`, single line
# `<mode> <yyyy-mm-dd> <consent-version>`); see TELEMETRY.md. Must be
# POSIX sh (dash-clean), `set -u`-safe, and must never affect the
# installer's exit status.
# ---------------------------------------------------------------------
bougie_telemetry_consent() {
    # An explicit setting always wins; never re-ask on reinstall;
    # never prompt in CI.
    if [ -n "${BOUGIE_TELEMETRY:-}" ]; then return 0; fi
    if [ -n "${CI:-}" ]; then return 0; fi
    _bougie_cfg_dir="${XDG_CONFIG_HOME:-$HOME/.config}/bougie"
    _bougie_mode_file="$_bougie_cfg_dir/telemetry"
    if [ -f "$_bougie_mode_file" ]; then return 0; fi
    _bougie_date="$(date -u +%Y-%m-%d 2>/dev/null || echo 1970-01-01)"

    # DO_NOT_TRACK is a decline, recorded so bougie never asks either.
    if [ -n "${DO_NOT_TRACK:-}" ] && [ "${DO_NOT_TRACK}" != "0" ]; then
        mkdir -p "$_bougie_cfg_dir" 2>/dev/null || return 0
        printf 'off %s 1\n' "$_bougie_date" > "$_bougie_mode_file" 2>/dev/null
        return 0
    fi

    # `curl | sh` consumes stdin, so the question goes to the
    # controlling terminal (rustup's pattern). No terminal → leave the
    # mode unset; bougie may ask interactively on first run. The probe
    # must live in a subshell: a redirection error on a brace group or
    # on the special builtin `:` is *fatal* in non-interactive POSIX
    # shells (dash exits 2); the subshell contains the blast.
    if ! ( : < /dev/tty ) 2>/dev/null; then return 0; fi

    printf '\n%s\n' 'bougie can send anonymous usage statistics and crash reports to the' >&2
    printf '%s\n' 'bougie developers. This never includes project names, package names,' >&2
    printf '%s\n' 'paths, or IP addresses, and nothing is sent without your consent.' >&2
    printf '%s\n\n' 'Details + full field list: https://bougie.tools/telemetry' >&2
    printf '%s' '  Enable anonymous telemetry? [Y/n] ' >&2
    if ! read -r _bougie_answer < /dev/tty; then return 0; fi

    case "$_bougie_answer" in
        [nN]*)
            mkdir -p "$_bougie_cfg_dir" 2>/dev/null || return 0
            printf 'off %s 1\n' "$_bougie_date" > "$_bougie_mode_file" 2>/dev/null
            printf '%s\n' 'ok — telemetry is off. Enable later with: bougie telemetry on' >&2
            ;;
        ''|[yY]*)
            mkdir -p "$_bougie_cfg_dir" 2>/dev/null || return 0
            printf 'on %s 1\n' "$_bougie_date" > "$_bougie_mode_file" 2>/dev/null
            printf '%s\n' 'telemetry enabled — inspect events anytime with: bougie telemetry log' >&2
            ;;
        *)
            # Unclassifiable reply: record nothing; bougie may ask later.
            ;;
    esac
    return 0
}
bougie_telemetry_consent || true
# ------------------------- end consent block -------------------------
