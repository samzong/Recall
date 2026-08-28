use anyhow::Result;

use crate::args::{CompletionShell, CompletionsCommand};
use crate::config::Paths;
use crate::launch::EnvLookup;
use crate::providers;

const BASH: &str = r#"_rx_bin() {
  local invoked=${COMP_WORDS[0]}
  local name=${invoked##*/}
  case $name in
    rxc|rxx|rxo|rxp|rxd|rxk)
      if [[ $invoked == */* ]]; then
        printf '%s' "${invoked%/*}/rx"
      else
        printf '%s' rx
      fi
      ;;
    *) printf '%s' "$invoked" ;;
  esac
}

_rx_ids() {
  local ids
  ids=$("$(_rx_bin)" completions "$1" 2>/dev/null) || return
  if [[ $cur == --provider=* ]]; then
    COMPREPLY=($(compgen -P --provider= -W "$ids" -- "${cur#--provider=}"))
  else
    COMPREPLY=($(compgen -W "$ids" -- "$cur"))
  fi
}

_rx_has_provider() {
  local i w
  for ((i = 1; i < COMP_CWORD; i++)); do
    w=${COMP_WORDS[i]}
    [[ $w == -- ]] && return 1
    [[ $w == --provider || $w == --provider=* ]] && return 0
  done
  return 1
}

_rx_positionals() {
  local i=1 pending=0 w
  while ((i < COMP_CWORD)); do
    w=${COMP_WORDS[i]}
    if [[ $w == -- ]]; then
      break
    elif [[ $w == --provider ]]; then
      pending=1
    elif [[ $w == --provider=* ]]; then
      :
    elif ((pending)); then
      pending=0
    elif [[ $w != -* ]]; then
      printf '%s\n' "$w"
    fi
    i=$((i + 1))
  done
}

_rx() {
  local cur prev cmd
  cmd=${COMP_WORDS[0]##*/}
  cur=${COMP_WORDS[COMP_CWORD]}
  prev=${COMP_WORDS[COMP_CWORD - 1]}
  COMPREPLY=()

  if [[ $cur == --provider=* ]]; then
    _rx_ids --targets
    return
  fi
  if [[ $prev == --provider ]]; then
    _rx_ids --targets
    return
  fi

  local pos=()
  local line
  while IFS= read -r line; do
    pos+=("$line")
  done < <(_rx_positionals)

  case $cmd in
    rxc|rxx|rxo|rxp|rxd|rxk)
      if [[ $cur == -* ]] && ! _rx_has_provider; then
        COMPREPLY=($(compgen -W '--provider' -- "$cur"))
      fi
      return
      ;;
  esac

  if ((${#pos[@]} == 0)); then
    if [[ $cur == -* ]]; then
      local flags='--help --version -h -V'
      _rx_has_provider || flags+=' --provider'
      COMPREPLY=($(compgen -W "$flags" -- "$cur"))
    elif _rx_has_provider; then
      COMPREPLY=($(compgen -W 'claude codex opencode pi dsh kimi' -- "$cur"))
    else
      COMPREPLY=($(compgen -W 'claude codex opencode pi dsh kimi providers update completions' -- "$cur"))
    fi
    return
  fi

  case ${pos[0]} in
    providers)
      case ${pos[1]-} in
        '')
          COMPREPLY=($(compgen -W 'list login logout use models' -- "$cur"))
          ;;
        login)
          ((${#pos[@]} == 2)) && _rx_ids --providers
          ;;
        logout)
          ((${#pos[@]} == 2)) && _rx_ids --configured
          ;;
        use)
          ((${#pos[@]} == 2)) && _rx_ids --targets
          ;;
        models)
          case ${pos[2]-} in
            '')
              COMPREPLY=($(compgen -W 'update' -- "$cur"))
              ;;
            update)
              ((${#pos[@]} == 3)) && _rx_ids --configured
              ;;
          esac
          ;;
      esac
      ;;
    update)
      [[ $cur == -* ]] && COMPREPLY=($(compgen -W '--yes -y' -- "$cur"))
      ;;
    completions)
      ((${#pos[@]} == 1)) && COMPREPLY=($(compgen -W 'bash zsh fish' -- "$cur"))
      ;;
    claude|codex|opencode|pi|dsh|kimi)
      if [[ $cur == -* ]] && ! _rx_has_provider; then
        COMPREPLY=($(compgen -W '--provider' -- "$cur"))
      fi
      ;;
  esac
}

complete -F _rx rx rxc rxx rxo rxp rxd rxk
"#;

const ZSH: &str = r#"#compdef rx rxc rxx rxo rxp rxd rxk

_rx_bin() {
  local invoked=$words[1]
  local name=${invoked:t}
  case $name in
    rxc|rxx|rxo|rxp|rxd|rxk)
      if [[ $invoked == */* ]]; then
        print -rn -- ${invoked:h}/rx
      else
        print -rn -- rx
      fi
      ;;
    *) print -rn -- $invoked ;;
  esac
}

_rx_ids() {
  local -a ids
  ids=(${(f)"$("$(_rx_bin)" completions "$1" 2>/dev/null)"})
  (( $#ids )) || return 1
  if [[ $PREFIX == --provider=* ]]; then
    compset -P '--provider='
    _describe -t providers provider ids
    return
  fi
  _describe -t providers provider ids
}

_rx_has_provider() {
  local w
  for w in $words[2,CURRENT-1]; do
    [[ $w == -- ]] && return 1
    [[ $w == --provider || $w == --provider=* ]] && return 0
  done
  return 1
}

_rx_positionals() {
  local -a out
  local i=2 pending=0 w
  while (( i < CURRENT )); do
    w=$words[i]
    if [[ $w == -- ]]; then
      break
    elif [[ $w == --provider ]]; then
      pending=1
    elif [[ $w == --provider=* ]]; then
      :
    elif (( pending )); then
      pending=0
    elif [[ $w != -* ]]; then
      out+=($w)
    fi
    (( i++ ))
  done
  (( $#out )) && print -l -- $out
}

_rx() {
  local cur=$words[CURRENT]
  local prev=$words[CURRENT-1]
  local cmd=${words[1]:t}

  if [[ $cur == --provider=* ]]; then
    _rx_ids --targets
    return
  fi
  if [[ $prev == --provider ]]; then
    _rx_ids --targets
    return
  fi

  local -a pos
  local output
  output=$(_rx_positionals)
  [[ -n $output ]] && pos=(${(f)output})

  case $cmd in
    rxc|rxx|rxo|rxp|rxd|rxk)
      if [[ $cur == -* ]] && ! _rx_has_provider; then
        compadd -- --provider
      fi
      return
      ;;
  esac

  if (( $#pos == 0 )); then
    if [[ $cur == -* ]]; then
      local -a flags
      flags=(--help --version -h -V)
      _rx_has_provider || flags+=(--provider)
      compadd -- $flags
    elif _rx_has_provider; then
      compadd -- claude codex opencode pi dsh kimi
    else
      compadd -- claude codex opencode pi dsh kimi providers update completions
    fi
    return
  fi

  case $pos[1] in
    providers)
      case ${pos[2]:-} in
        '')
          compadd -- list login logout use models
          ;;
        login)
          (( $#pos == 2 )) && _rx_ids --providers
          ;;
        logout)
          (( $#pos == 2 )) && _rx_ids --configured
          ;;
        use)
          (( $#pos == 2 )) && _rx_ids --targets
          ;;
        models)
          case ${pos[3]:-} in
            '')
              compadd -- update
              ;;
            update)
              (( $#pos == 3 )) && _rx_ids --configured
              ;;
          esac
          ;;
      esac
      ;;
    update)
      [[ $cur == -* ]] && compadd -- --yes -y
      ;;
    completions)
      (( $#pos == 1 )) && compadd -- bash zsh fish
      ;;
    claude|codex|opencode|pi|dsh|kimi)
      if [[ $cur == -* ]] && ! _rx_has_provider; then
        compadd -- --provider
      fi
      ;;
  esac
}

compdef _rx rx rxc rxx rxo rxp rxd rxk
"#;

const FISH: &str = r#"function __rx_bin
    set -l invoked (commandline -opc)[1]
    set -l name (basename $invoked)
    switch $name
        case rxc rxx rxo rxp rxd rxk
            if string match -q '*/*' -- $invoked
                echo (dirname $invoked)/rx
            else
                echo rx
            end
        case '*'
            echo $invoked
    end
end

function __rx_ids
    set -l bin (__rx_bin)
    $bin completions $argv[1] 2>/dev/null
end

function __rx_pos
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l out
    set -l pending 0
    for w in $tokens
        if test "$w" = --
            break
        else if test "$w" = --provider
            set pending 1
        else if string match -q -- '--provider=*' "$w"
            true
        else if test $pending -eq 1
            set pending 0
        else if not string match -q -- '-*' "$w"
            set -a out $w
        end
    end
    if test (count $out) -gt 0
        printf '%s\n' $out
    end
end

function __rx_n
    test (count (__rx_pos)) -eq $argv[1]
end

function __rx_is
    set -l pos (__rx_pos)
    test (count $pos) -ge (count $argv)
    or return 1
    set -l i 1
    for expected in $argv
        test $pos[$i] = $expected
        or return 1
        set i (math $i + 1)
    end
    return 0
end

function __rx_has_provider
    set -l tokens (commandline -opc)
    set -e tokens[1]
    for w in $tokens
        if test "$w" = --
            return 1
        else if test "$w" = --provider
            return 0
        else if string match -q -- '--provider=*' "$w"
            return 0
        end
    end
    return 1
end

function __rx_has_harness
    set -l pos (__rx_pos)
    test (count $pos) -ge 1
    and contains -- $pos[1] claude codex opencode pi dsh kimi
end

complete -c rx -f
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'claude' -d 'Claude Code'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'codex' -d 'Codex'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'opencode' -d 'OpenCode'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'pi' -d 'Pi'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'dsh' -d 'DeepSeek Harness'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'kimi' -d 'Kimi Code'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'providers' -d 'Manage providers'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'update' -d 'Update rx'
complete -c rx -n '__rx_n 0; and not __rx_has_provider' -a 'completions' -d 'Generate shell completion script'
complete -c rx -n '__rx_n 0; and __rx_has_provider' -a 'claude codex opencode pi dsh kimi'
complete -c rx -n '__rx_n 0' -s h -l help
complete -c rx -n '__rx_n 0' -s V -l version
complete -c rx -n 'not __rx_has_provider; and __rx_n 0' -l provider -xa '(__rx_ids --targets)'
complete -c rx -n 'not __rx_has_provider; and __rx_has_harness' -l provider -xa '(__rx_ids --targets)'
complete -c rx -n '__rx_is providers; and __rx_n 1' -a 'list login logout use models'
complete -c rx -n '__rx_is providers login; and __rx_n 2' -a '(__rx_ids --providers)'
complete -c rx -n '__rx_is providers logout; and __rx_n 2' -a '(__rx_ids --configured)'
complete -c rx -n '__rx_is providers use; and __rx_n 2' -a '(__rx_ids --targets)'
complete -c rx -n '__rx_is providers models; and __rx_n 2' -a 'update'
complete -c rx -n '__rx_is providers models update; and __rx_n 3' -a '(__rx_ids --configured)'
complete -c rx -n '__rx_is update; and __rx_n 1' -l yes -s y
complete -c rx -n '__rx_is completions; and __rx_n 1' -a 'bash zsh fish'

for cmd in rxc rxx rxo rxp rxd rxk
    complete -c $cmd -f
    complete -c $cmd -l provider -xa '(__rx_ids --targets)'
end
"#;

pub(crate) fn help() -> &'static str {
    concat!(
        "usage: rx completions <bash|zsh|fish>\n\n",
        "Write a shell completion script to stdout.\n"
    )
}

pub(crate) fn script(shell: CompletionShell) -> &'static str {
    match shell {
        CompletionShell::Bash => BASH,
        CompletionShell::Zsh => ZSH,
        CompletionShell::Fish => FISH,
    }
}

pub(crate) fn run(command: CompletionsCommand, paths: &Paths, env: &EnvLookup) -> Result<()> {
    match command {
        CompletionsCommand::Help => {
            print!("{}", help());
            Ok(())
        }
        CompletionsCommand::Generate { shell } => {
            print!("{}", script(shell));
            Ok(())
        }
        CompletionsCommand::ListProviders(filter) => {
            for id in providers::completion_ids(paths, env, filter)? {
                println!("{id}");
            }
            Ok(())
        }
    }
}
