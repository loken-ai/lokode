"""Turn the screens the `screens` test writes into pictures.

Each JSON file holds one scene, cell by cell: the symbol, its colours, and whether it is
bold, italic, underlined, reversed or dim. Named colours are painted with the palette a
terminal of the scene's theme would use.

Usage: python3 scripts/screens.py <directory of json> <directory for png>
"""

import json
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

FONT_DIR = Path("/usr/share/fonts/truetype/dejavu")
SIZE = 15

PALETTES = {
    "dark": {
        "Black": "#282c34", "Red": "#e06c75", "Green": "#98c379", "Yellow": "#e5c07b",
        "Blue": "#61afef", "Magenta": "#c678dd", "Cyan": "#56b6c2", "Gray": "#abb2bf",
        "DarkGray": "#5c6370", "LightRed": "#ff7b86", "LightGreen": "#b5e890",
        "LightYellow": "#f0d197", "LightBlue": "#82c4ff", "LightMagenta": "#d898eb",
        "LightCyan": "#7fd3df", "White": "#ffffff",
    },
    "light": {
        "Black": "#383a42", "Red": "#e45649", "Green": "#50a14f", "Yellow": "#c18401",
        "Blue": "#4078f2", "Magenta": "#a626a4", "Cyan": "#0184bc", "Gray": "#696c77",
        "DarkGray": "#a0a1a7", "LightRed": "#ff6b5f", "LightGreen": "#6cc06b",
        "LightYellow": "#d9a21e", "LightBlue": "#5d8ff5", "LightMagenta": "#c13fc0",
        "LightCyan": "#2ba6d6", "White": "#fafafa",
    },
}


def colour(name, palette, default):
    if name == "reset":
        return default
    if name.startswith("#"):
        return name
    return palette.get(name, default)


def render(doc, out):
    palette = PALETTES[doc["theme"]]
    fonts = {
        (False, False): ImageFont.truetype(str(FONT_DIR / "DejaVuSansMono.ttf"), SIZE),
        (True, False): ImageFont.truetype(str(FONT_DIR / "DejaVuSansMono-Bold.ttf"), SIZE),
        (False, True): ImageFont.truetype(str(FONT_DIR / "DejaVuSansMono-Oblique.ttf"), SIZE),
        (True, True): ImageFont.truetype(str(FONT_DIR / "DejaVuSansMono-BoldOblique.ttf"), SIZE),
    }
    regular = fonts[(False, False)]
    cell_w = round(regular.getlength("M"))
    cell_h = round(SIZE * 1.4)
    rows = doc["rows"]
    pad = 12
    image = Image.new(
        "RGB",
        (cell_w * len(rows[0]) + pad * 2, cell_h * len(rows) + pad * 2),
        doc["bg"],
    )
    draw = ImageDraw.Draw(image)
    for y, row in enumerate(rows):
        for x, (symbol, fg, bg, bold, italic, underline, reversed_, dim) in enumerate(row):
            fg = colour(fg, palette, doc["fg"])
            bg = colour(bg, palette, doc["bg"])
            if reversed_:
                fg, bg = bg, fg
            left, top = pad + x * cell_w, pad + y * cell_h
            if bg != doc["bg"]:
                draw.rectangle([left, top, left + cell_w, top + cell_h], fill=bg)
            if symbol.strip():
                font = fonts[(bool(bold), bool(italic))]
                draw.text((left, top + (cell_h - SIZE) // 2 - 1), symbol, font=font, fill=fg)
            if underline:
                draw.line([left, top + cell_h - 3, left + cell_w, top + cell_h - 3], fill=fg)
    image.save(out)


def main():
    source, target = Path(sys.argv[1]), Path(sys.argv[2])
    target.mkdir(parents=True, exist_ok=True)
    for path in sorted(source.glob("*.json")):
        render(json.loads(path.read_text()), target / (path.stem + ".png"))
        print(target / (path.stem + ".png"))


if __name__ == "__main__":
    main()
