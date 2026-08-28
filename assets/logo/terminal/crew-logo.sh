#!/usr/bin/env sh
# Crew — print the mark in the terminal.
#   ./crew-logo.sh          full mark (32 cols)
#   ./crew-logo.sh small    compact mark (16 cols)
# Truecolor when the terminal supports it, block art otherwise.
dir=$(dirname "$0")
name="crew-banner"
[ "$1" = "small" ] && name="crew-banner-small"
case "${COLORTERM:-}" in
  truecolor|24bit) cat "$dir/$name.ans" ;;
  *) cat "$dir/$name.txt" ;;
esac
