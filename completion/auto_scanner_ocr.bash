# bash completion for auto-scanner-ocr
# Install: source this file from ~/.bashrc, e.g.
#   source ~/programmingProjects/auto-scanner-ocr/completion/auto_scanner_ocr.bash
# Registers completion for "auto_scanner_ocr.py" and the "scan" alias
# suggested in the README.

_auto_scanner_ocr() {
    local cur prev
    if declare -F _init_completion >/dev/null; then
        _init_completion || return
    else
        # minimal fallback if bash-completion is not installed
        cur="${COMP_WORDS[COMP_CWORD]}"
        prev="${COMP_WORDS[COMP_CWORD-1]}"
    fi

    case "$prev" in
        -m|--multi|--no-unpaper|--no-notify|--doctor|--verbose|--version|--help)
            return
            ;;
        --mode)
            COMPREPLY=($(compgen -W "gray color lineart" -- "$cur"))
            return
            ;;
        --dpi)
            COMPREPLY=($(compgen -W "150 200 300 600 1200" -- "$cur"))
            return
            ;;
        --langs)
            # complete the last plus-separated segment against installed
            # tesseract languages, keeping any "eng+" prefix
            local langs prefix="" last
            langs=$(tesseract --list-langs 2>/dev/null | tail -n +2)
            last="${cur##*+}"
            [[ "$cur" == *+* ]] && prefix="${cur%+}+"
            COMPREPLY=($(compgen -W "$langs" -P "$prefix" -- "$last"))
            return
            ;;
        --device)
            # device names as reported by SANE, e.g. hpaio:/usb/Deskjet_1050_J410?serial=...
            local dev devs=()
            while IFS= read -r dev; do
                devs+=("${dev#device \`}")
            done < <(scanimage -L 2>/dev/null | grep -oE "device \`[^']+")
            COMPREPLY=($(compgen -W "${devs[*]}" -- "$cur"))
            return
            ;;
        --output)
            declare -F _filedir >/dev/null && { _filedir -d; return; }
            return
            ;;
        --config)
            declare -F _filedir >/dev/null && { _filedir toml; return; }
            return
            ;;
    esac

    if [[ "$cur" == -* ]]; then
        COMPREPLY=($(compgen -W \
            "-m --multi --dpi --mode --langs --output --device --config \
             --no-unpaper --no-notify --doctor --verbose --version --help" \
            -- "$cur"))
        return
    fi

    # fall back to default (file) completion otherwise
    COMPREPLY=($(compgen -f -- "$cur"))
}

complete -o bashdefault -o default -F _auto_scanner_ocr auto_scanner_ocr.py scan