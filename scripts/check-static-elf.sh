#!/usr/bin/env bash
# Checks that each file is a static x86_64 Linux executable: ELF magic, 64-bit little-endian,
# machine x86-64, type EXEC or DYN (static-pie) and no PT_INTERP program header, so the kernel
# runs it without a dynamic loader on any distribution. Needs only od.
# Usage: scripts/check-static-elf.sh <file>...
set -euo pipefail

if [ "$#" -eq 0 ]; then
  echo "usage: check-static-elf.sh <file>..." >&2
  exit 2
fi

# Unsigned little-endian integer of <count> bytes at <offset> of <file>.
uint() {
  local hex value="" i
  hex="$(od -An -v -tx1 -j "$2" -N "$3" "$1" | tr -d ' \n')"
  if [ "${#hex}" -ne $(( $3 * 2 )) ]; then
    echo "$1: truncated at offset $2" >&2
    return 1
  fi
  for (( i = ${#hex} - 2; i >= 0; i -= 2 )); do
    value+="${hex:i:2}"
  done
  echo $(( 16#$value ))
}

check() {
  local file="$1" magic class data type machine phoff phentsize phnum i ptype kind
  if [ ! -f "$file" ]; then
    echo "$file: no such file" >&2
    return 1
  fi
  magic="$(od -An -v -tx1 -N 4 "$file" | tr -d ' \n')"
  if [ "$magic" != "7f454c46" ]; then
    echo "$file: not an ELF file" >&2
    return 1
  fi
  class="$(uint "$file" 4 1)" || return 1
  data="$(uint "$file" 5 1)" || return 1
  if [ "$class" -ne 2 ] || [ "$data" -ne 1 ]; then
    echo "$file: not a 64-bit little-endian ELF (class $class, data $data)" >&2
    return 1
  fi
  type="$(uint "$file" 16 2)" || return 1
  machine="$(uint "$file" 18 2)" || return 1
  if [ "$machine" -ne 62 ]; then
    echo "$file: machine $machine is not x86-64 (62)" >&2
    return 1
  fi
  case "$type" in
    2) kind="static" ;;
    3) kind="static-pie" ;;
    *) echo "$file: ELF type $type is not an executable" >&2; return 1 ;;
  esac
  phoff="$(uint "$file" 32 8)" || return 1
  phentsize="$(uint "$file" 54 2)" || return 1
  phnum="$(uint "$file" 56 2)" || return 1
  for (( i = 0; i < phnum; i++ )); do
    ptype="$(uint "$file" $(( phoff + i * phentsize )) 4)" || return 1
    if [ "$ptype" -eq 3 ]; then
      echo "$file: dynamically linked (PT_INTERP present)" >&2
      return 1
    fi
  done
  echo "$file: ELF x86-64 $kind, $(wc -c < "$file" | tr -d ' ') bytes"
}

status=0
for file in "$@"; do
  check "$file" || status=1
done
exit "$status"
