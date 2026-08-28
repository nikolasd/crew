# Crew logo — terminal assets

    crew-banner.ans          32 cols x 16 rows, 24-bit colour
    crew-banner-small.ans    16 cols x 8 rows, 24-bit colour
    crew-banner.txt          same mark, block art, no colour
    crew-banner-small.txt    compact block art
    crew-logo.sh             prints whichever the terminal can handle

Print it:

    cat crew-banner.ans
    sh crew-logo.sh small

Both .ans files use half-block characters (upper/lower halves of one cell), so
the pixels stay square in a terminal's 1:2 character cell. Empty pixels emit a
space with the background reset, so the mark sits on whatever the terminal
already paints — nothing is filled in behind it.

The .txt files use full and half blocks with no escape codes at all: safe for
logs, CI output, piping, and terminals without colour.

For a TUI that draws its own colours, ../crew-logo-mono.svg uses currentColor
and will take the surrounding text colour.
