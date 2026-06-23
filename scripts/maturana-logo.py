#!/usr/bin/env python3
"""Render the Maturana wordmark as a truecolor ANSI logo.

Used to generate the banner shown by `maturana tui` and the .ans/SVG assets
embedded in the README. Pure stdlib, no deps.

    python3 scripts/maturana-logo.py [cool|ember|ice|mono] [--no-sub]

The wordmark is the "ANSI Shadow" figlet font; the gradient is applied per
column so it sweeps horizontally across the whole mark.
"""
import sys

WORDMARK = [
    "███╗   ███╗ █████╗ ████████╗██╗   ██╗██████╗  █████╗ ███╗   ██╗ █████╗ ",
    "████╗ ████║██╔══██╗╚══██╔══╝██║   ██║██╔══██╗██╔══██╗████╗  ██║██╔══██╗",
    "██╔████╔██║███████║   ██║   ██║   ██║██████╔╝███████║██╔██╗ ██║███████║",
    "██║╚██╔╝██║██╔══██║   ██║   ██║   ██║██╔══██╗██╔══██║██║╚██╗██║██╔══██║",
    "██║ ╚═╝ ██║██║  ██║   ██║   ╚██████╔╝██║  ██║██║  ██║██║ ╚████║██║  ██║",
    "╚═╝     ╚═╝╚═╝  ╚═╝   ╚═╝    ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝╚═╝  ╚═╝",
]

SUBTITLE = "Secure, lean, codex-native"

# Each palette is a list of RGB stops the gradient interpolates between.
PALETTES = {
    "cool":  [(45, 212, 191), (56, 189, 248), (167, 139, 250)],   # teal → sky → violet
    "ember": [(251, 191, 36), (249, 115, 22), (239, 68, 68)],     # amber → orange → red
    "ice":   [(125, 211, 252), (96, 165, 250), (129, 140, 248)],  # light cyan → blue
    "mono":  [(220, 220, 220), (160, 160, 160), (110, 110, 110)], # plain greyscale
}

RESET = "\x1b[0m"


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def color_at(stops, t):
    if t <= 0:
        return stops[0]
    if t >= 1:
        return stops[-1]
    span = 1 / (len(stops) - 1)
    seg = min(int(t / span), len(stops) - 2)
    return lerp(stops[seg], stops[seg + 1], (t - seg * span) / span)


def render(palette="cool", subtitle=True):
    stops = PALETTES[palette]
    width = max(len(line) for line in WORDMARK)
    out = ["\n"]
    for line in WORDMARK:
        buf = "  "
        for col, ch in enumerate(line):
            if ch == " ":
                buf += " "
                continue
            r, g, b = color_at(stops, col / (width - 1))
            buf += f"\x1b[38;2;{r};{g};{b}m{ch}"
        out.append(buf + RESET + "\n")
    if subtitle:
        out.append(f"  \x1b[2;37m{SUBTITLE}{RESET}\n")
    return "".join(out)


def main(argv):
    palette = "cool"
    subtitle = True
    for arg in argv[1:]:
        if arg == "--no-sub":
            subtitle = False
        elif arg in PALETTES:
            palette = arg
        else:
            sys.stderr.write(f"unknown arg: {arg}\n")
            return 2
    sys.stdout.write(render(palette, subtitle))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
