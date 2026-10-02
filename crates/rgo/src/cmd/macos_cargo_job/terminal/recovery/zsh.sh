# Explicit private pilot; evaluate in the interactive zsh that owns this terminal.
() {
    emulate -L zsh
    [[ -o interactive && $ZSH_VERSION == 5.9 ]] || return 1
    if [[ $__rgo_terminal_owner == v1 ]]; then
        [[ ${functions[__rgo_terminal_preexec]} == $__rgo_terminal_preexec_definition &&
           ${functions[__rgo_terminal_precmd]} == $__rgo_terminal_precmd_definition &&
           ${functions[__rgo_terminal_counter]} == $__rgo_terminal_counter_definition &&
           ${functions[__rgo_terminal_undo]} == $__rgo_terminal_undo_definition ]] && return 0
        return 1
    fi
    (( ${+functions[__rgo_terminal_preexec]} || ${+functions[__rgo_terminal_precmd]} ||
       ${+functions[__rgo_terminal_counter]} || ${+functions[__rgo_terminal_undo]} )) && return 1
    zmodload -F zsh/system b:sysopen b:syswrite || return 1
    zmodload -F zsh/files b:zf_mv || return 1
# rgo initialization variables
    local registration
    registration=$("$__rgo_terminal_executable" macos-terminal-host --home "$__rgo_terminal_root" register --shell-pid "$$") || return 1
    eval "$registration" || return 1
    autoload -Uz add-zsh-hook
    __rgo_terminal_counter() {
        emulate -L zsh
        [[ ! -e "${__rgo_terminal_directory:h}/.retiring-${__rgo_terminal_token}.json" ]] || return 1
        local counter="$__rgo_terminal_directory/generation.$$.${__rgo_terminal_generation}.$1.tmp" descriptor
        builtin sysopen -w -m 600 -o creat,excl,cloexec,nofollow -u descriptor "$counter" || return 1
        if ! builtin syswrite -o "$descriptor" "$1"; then
            exec {descriptor}>&-
            return 1
        fi
        exec {descriptor}>&-
        builtin zf_mv -f -- "$counter" "$__rgo_terminal_directory/generation"
    }
    __rgo_terminal_preexec() {
        emulate -L zsh
        [[ -n $RGO_TERMINAL_HOST && -z $__rgo_terminal_disabled ]] || return 0
        (( ++__rgo_terminal_generation ))
        if (( __rgo_terminal_generation <= 0 || ${precmd_functions[(Ie)__rgo_terminal_precmd]} == 0 )) ||
           [[ ${functions[__rgo_terminal_precmd]} != $__rgo_terminal_precmd_definition ||
              ${functions[__rgo_terminal_counter]} != $__rgo_terminal_counter_definition ]] ||
           ! __rgo_terminal_counter "$__rgo_terminal_generation"; then
            typeset -g __rgo_terminal_disabled=1
            RGO_TERMINAL_HOST=disabled
        fi
        return 0
    }
    __rgo_terminal_precmd() {
        emulate -L zsh
        [[ -z $__rgo_terminal_disabled ]] || return 0
        if [[ -e "${__rgo_terminal_directory:h}/.retiring-${__rgo_terminal_token}.json" ]]; then
            typeset -g __rgo_terminal_disabled=1
            RGO_TERMINAL_HOST=disabled
            return 0
        fi
        if [[ -f "$__rgo_terminal_directory/lease.json" ]]; then
            if ! "$__rgo_terminal_executable" macos-terminal-host --home "$__rgo_terminal_root" finish --token "$__rgo_terminal_token" --generation "$__rgo_terminal_generation"; then
                typeset -g __rgo_terminal_disabled=1
                RGO_TERMINAL_HOST=disabled
            fi
        fi
        # A prompt ends the command generation. Without a working preexec
        # hook, the next Cargo invocation cannot reuse a stale generation.
        if ! __rgo_terminal_counter 0; then
            typeset -g __rgo_terminal_disabled=1
            RGO_TERMINAL_HOST=disabled
        fi
        return 0
    }
    __rgo_terminal_undo() {
        emulate -L zsh
        [[ ${functions[__rgo_terminal_preexec]} == $__rgo_terminal_preexec_definition &&
           ${functions[__rgo_terminal_precmd]} == $__rgo_terminal_precmd_definition &&
           ${functions[__rgo_terminal_counter]} == $__rgo_terminal_counter_definition &&
           ${functions[__rgo_terminal_undo]} == $__rgo_terminal_undo_definition ]] || return 1
        "$__rgo_terminal_executable" macos-terminal-host --home "$__rgo_terminal_root" unregister --token "$__rgo_terminal_token" || return 1
        add-zsh-hook -d preexec __rgo_terminal_preexec
        add-zsh-hook -d precmd __rgo_terminal_precmd
        add-zsh-hook -d zshexit __rgo_terminal_undo
        unfunction __rgo_terminal_preexec __rgo_terminal_precmd __rgo_terminal_counter __rgo_terminal_undo
        unset RGO_TERMINAL_HOST __rgo_terminal_owner __rgo_terminal_directory __rgo_terminal_disabled
    }
    typeset -g __rgo_terminal_preexec_definition="${functions[__rgo_terminal_preexec]}"
    typeset -g __rgo_terminal_precmd_definition="${functions[__rgo_terminal_precmd]}"
    typeset -g __rgo_terminal_counter_definition="${functions[__rgo_terminal_counter]}"
    typeset -g __rgo_terminal_undo_definition="${functions[__rgo_terminal_undo]}"
    typeset -g __rgo_terminal_owner=v1
    add-zsh-hook preexec __rgo_terminal_preexec
    add-zsh-hook precmd __rgo_terminal_precmd
    add-zsh-hook zshexit __rgo_terminal_undo
}
