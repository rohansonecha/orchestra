#!/usr/bin/env python3
"""Render a tmux pane to a PNG screenshot.

Usage: render_pane.py <session_name> <output.png>

Captures the pane with ANSI escape codes, feeds it through pyte (a terminal
emulator library), then renders the screen to a PNG using Pillow.
"""

import sys
import subprocess
import pyte
from PIL import Image, ImageDraw, ImageFont

# Basic 16-color ANSI palette
COLORS = {
    'default': (220, 220, 220),
    'black': (0, 0, 0),
    'red': (220, 50, 50),
    'green': (50, 200, 50),
    'yellow': (220, 220, 50),
    'blue': (50, 100, 220),
    'magenta': (200, 50, 200),
    'cyan': (50, 200, 200),
    'white': (220, 220, 220),
    'brightblack': (100, 100, 100),
    'brightred': (255, 100, 100),
    'brightgreen': (100, 255, 100),
    'brightyellow': (255, 255, 100),
    'brightblue': (100, 150, 255),
    'brightmagenta': (255, 100, 255),
    'brightcyan': (100, 255, 255),
    'brightwhite': (255, 255, 255),
}

BG_COLOR = (20, 20, 30)
FONT_SIZE = 14
CHAR_W = 8
CHAR_H = 16

def get_color(color_name, is_bg=False):
    if not color_name:
        return BG_COLOR if is_bg else COLORS['default']
    # pyte uses names like 'red', 'brightred', or hex like '#ff0000'
    if color_name.startswith('#'):
        r = int(color_name[1:3], 16)
        g = int(color_name[3:5], 16)
        b = int(color_name[5:7], 16)
        return (r, g, b)
    return COLORS.get(color_name, BG_COLOR if is_bg else COLORS['default'])

def render_pane(session_name, output_path):
    # Capture pane with ANSI escape codes
    result subprocess.run(
        ['tmux', 'capture-pane', '-t', session_name, '-e', '-p'],
        capture_output=True, text=True, check=True
    )
    result = subprocess.run(
        ['tmux', 'capture-pane', '-t', session_name, '-e', '-p'],
        capture_output=True, text=True, check=True
    )
    ansi_output = result.stdout

    # Get pane dimensions
    lines = ansi_output.split('\n')
    height = len(lines)
    width = max(len(line) for line in lines) if lines else 80
    width = min(max(width, 80), 200)

    # Feed through pyte terminal emulator
    screen = pyte.Screen(width, height)
    stream = pyte.Stream(screen)
    stream.feed(ansi_output)

    # Render to image
    img_w = width * CHAR_W
    img_h = height * CHAR_H
    img = Image.new('RGB', (img_w, img_h), BG_COLOR)
    draw = ImageDraw.Draw(img)

    # Try to load a monospace font
    try:
        font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", FONT_SIZE)
    except:
        font = ImageFont.load_default()

    for row_idx, line in enumerate(screen.display):
        for col_idx, char in enumerate(line):
            if char == ' ' or char == '\x00':
                continue
            x = col_idx * CHAR_W
            y = row_idx * CHAR_H
            # Get the character's attributes
            try:
                char_obj = screen.buffer[row_idx][col_idx]
                fg = get_color(char_obj.fg)
                bg = get_color(char_obj.bg, is_bg=True)
                # Draw background
                if bg != BG_COLOR:
                    draw.rectangle([x, y, x + CHAR_W, y + CHAR_H], fill=bg)
                # Draw character
                draw.text((x, y), char, fill=fg, font=font)
            except (IndexError, KeyError):
                draw.text((x, y), char, fill=COLORS['default'], font=font)

    img.save(output_path)
    print(f"Rendered to {output_path} ({img_w}x{img_h})")

if __name__ == '__main__':
    if len(sys.argv) < 3:
        print("Usage: render_pane.py <session_name> <output.png>")
        sys.exit(1)
    render_pane(sys.argv[1], sys.argv[2])
