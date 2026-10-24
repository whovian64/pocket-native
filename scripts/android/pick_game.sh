#!/bin/bash
# pick_game.sh <games dir> [what]: prints the title id of the one game built
# in <games dir> that <what> names: its title id, or any part of its name, in
# any case. With nothing named, the only game built, or with more than one,
# the one picked from a numbered list. Fails, saying why, when none fits.
set -euo pipefail

games="$1" what="${2:-}"

# a game's name, as its build left it, or its profile's, or its title id
name_of() {
    local id="$1" name=""
    [ -f "$games/$id/name.txt" ] && name=$(cat "$games/$id/name.txt")
    if [ -z "$name" ] && [ -f "$games/../titles/$id/profile.toml" ]; then
        name=$(sed -n '/^name = "/{s/^name = "\(.*\)"$/\1/p;q}' "$games/../titles/$id/profile.toml")
    fi
    printf '%s' "${name:-$id}"
}

# accented letters as their plain ones, each replaced as a whole, which
# works in any locale where a list of them in brackets does not
plain_letters() {
    sed -e 's/é/e/g; s/è/e/g; s/ê/e/g; s/ë/e/g; s/É/E/g; s/È/E/g; s/Ê/E/g; s/á/a/g; s/à/a/g; s/â/a/g; s/ä/a/g; s/ã/a/g; s/Á/A/g; s/À/A/g; s/Â/A/g; s/Ä/A/g; s/ó/o/g; s/ò/o/g; s/ô/o/g; s/ö/o/g; s/Ó/O/g; s/Ö/O/g; s/ú/u/g; s/ù/u/g; s/û/u/g; s/ü/u/g; s/Ú/U/g; s/Ü/U/g; s/í/i/g; s/ì/i/g; s/î/i/g; s/ï/i/g; s/Í/I/g; s/ñ/n/g; s/Ñ/N/g; s/ç/c/g; s/Ç/C/g'
}

ids=()
if [ -d "$games" ]; then
    for dir in "$games"/*/; do
        [ -f "$dir/pocket-native.apk" ] && ids+=("$(basename "$dir")")
    done
fi
if [ ${#ids[@]} -eq 0 ]; then
    echo "no game is built yet: scripts/android/build.sh path/to/game.3ds builds one" >&2
    exit 2
fi

matches=()
if [ -z "$what" ]; then
    matches=("${ids[@]}")
else
    wanted=$(printf '%s' "$what" | plain_letters | tr '[:upper:]' '[:lower:]')
    for id in "${ids[@]}"; do
        if [ "${id,,}" = "$wanted" ]; then
            matches=("$id")
            break
        fi
        # é and the like match their plain letters, so that "pokemon" finds
        # Pokémon
        plain=$(name_of "$id" | plain_letters)
        case "$(printf '%s' "$plain" | tr '[:upper:]' '[:lower:]')" in
            *"$wanted"*) matches+=("$id") ;;
        esac
    done
fi

if [ ${#matches[@]} -eq 0 ]; then
    echo "no built game is called \"$what\"; the ones built are:" >&2
    for id in "${ids[@]}"; do echo "   $(name_of "$id")" >&2; done
    exit 2
fi
if [ ${#matches[@]} -eq 1 ]; then
    echo "${matches[0]}"
    exit 0
fi
if [ ! -t 0 ]; then
    echo "\"${what:-}\" could be any of these, name one more closely:" >&2
    for id in "${matches[@]}"; do echo "   $(name_of "$id")" >&2; done
    exit 2
fi
echo "which game?" >&2
for index in "${!matches[@]}"; do
    echo "   $((index + 1))) $(name_of "${matches[$index]}")" >&2
done
while true; do
    read -rp "number: " choice >&2 || exit 2
    if [[ "$choice" =~ ^[0-9]+$ ]] && [ "$choice" -ge 1 ] && [ "$choice" -le ${#matches[@]} ]; then
        echo "${matches[$((choice - 1))]}"
        exit 0
    fi
done
