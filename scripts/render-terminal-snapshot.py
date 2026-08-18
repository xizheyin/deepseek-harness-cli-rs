#!/usr/bin/env python3
"""Render a bounded dsh PTY transcript into a deterministic terminal image.

The input is produced by the installed-binary release acceptance test.  This
parser deliberately supports only the small ANSI subset emitted by dsh.  Any
unknown control sequence fails closed instead of being interpreted by a real
terminal or copied into the output.
"""

from __future__ import annotations

import argparse
import base64
import dataclasses
import hashlib
import html
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata


MAX_INPUT_BYTES = 1_048_576
MAX_COLUMNS = 160
MAX_ROWS = 60
MAX_CSI_BYTES = 16

FONT_DIGESTS = {
    "JetBrainsMono-Regular.ttf": "a0bf60ef0f83c5ed4d7a75d45838548b1f6873372dfac88f71804491898d138f",
    "JetBrainsMono-Bold.ttf": "5590990c82e097397517f275f430af4546e1c45cff408bde4255dad142479dcb",
}

PALETTE = {
    None: "#c9d1d9",
    30: "#0b0f14",
    31: "#ff7b72",
    32: "#7ee787",
    33: "#e3b341",
    34: "#79c0ff",
    35: "#d2a8ff",
    36: "#56d4dd",
    37: "#f0f6fc",
}

BACKGROUND_PALETTE = {
    None: None,
    40: "#0b0f14",
    41: "#6e2b2b",
    42: "#245c3a",
    43: "#e3b341",
    44: "#244c75",
    45: "#69436f",
    46: "#1d5960",
    47: "#c9d1d9",
}

ALLOWED_SGR = {0, 1, 2, 30, 31, 32, 33, 35, 36, 39, 43, 49}


class SnapshotError(ValueError):
    """A safe, printable rejection of an unsupported transcript."""


@dataclasses.dataclass(frozen=True)
class Style:
    foreground: int | None = None
    background: int | None = None
    bold: bool = False
    dim: bool = False


@dataclasses.dataclass
class Cell:
    text: str = " "
    style: Style = dataclasses.field(default_factory=Style)
    continuation: bool = False


def display_width(character: str) -> int:
    if unicodedata.combining(character):
        return 0
    if unicodedata.category(character) in {"Cc", "Cf", "Cs"}:
        raise SnapshotError(f"unsupported Unicode category U+{ord(character):04X}")
    return 2 if unicodedata.east_asian_width(character) in {"F", "W"} else 1


class Screen:
    def __init__(self, columns: int, rows: int) -> None:
        if not 1 <= columns <= MAX_COLUMNS or not 1 <= rows <= MAX_ROWS:
            raise SnapshotError(
                f"geometry must be within {MAX_COLUMNS} columns and {MAX_ROWS} rows"
            )
        self.columns = columns
        self.rows = rows
        self.lines = [[Cell() for _ in range(columns)] for _ in range(rows)]
        self.row = 0
        self.column = 0
        self.pending_wrap = False
        self.style = Style()

    def _scroll_if_needed(self) -> None:
        while self.row >= self.rows:
            self.lines.pop(0)
            self.lines.append([Cell() for _ in range(self.columns)])
            self.row -= 1

    def line_feed(self) -> None:
        self.row += 1
        self.pending_wrap = False
        self._scroll_if_needed()

    def carriage_return(self) -> None:
        self.column = 0
        self.pending_wrap = False

    def backspace(self) -> None:
        self.column = max(0, self.column - 1)
        self.pending_wrap = False

    def tab(self) -> None:
        target = min(self.columns - 1, ((self.column // 8) + 1) * 8)
        while self.column < target:
            self.put(" ")

    def cursor_up(self, count: int) -> None:
        self.row = max(0, self.row - count)
        self.pending_wrap = False

    def erase_line(self) -> None:
        self.lines[self.row] = [Cell() for _ in range(self.columns)]
        self.pending_wrap = False

    def set_graphics(self, parameters: list[int]) -> None:
        style = self.style
        for parameter in parameters or [0]:
            if parameter not in ALLOWED_SGR:
                raise SnapshotError(f"unsupported SGR parameter {parameter}")
            if parameter == 0:
                style = Style()
            elif parameter == 1:
                style = dataclasses.replace(style, bold=True, dim=False)
            elif parameter == 2:
                style = dataclasses.replace(style, dim=True, bold=False)
            elif 30 <= parameter <= 37:
                style = dataclasses.replace(style, foreground=parameter)
            elif parameter == 39:
                style = dataclasses.replace(style, foreground=None)
            elif 40 <= parameter <= 47:
                style = dataclasses.replace(style, background=parameter)
            elif parameter == 49:
                style = dataclasses.replace(style, background=None)
        self.style = style

    def put(self, character: str) -> None:
        width = display_width(character)
        if width == 0:
            previous = max(0, self.column - 1)
            self.lines[self.row][previous].text += character
            return
        if self.pending_wrap:
            self.column = 0
            self.line_feed()
        if width > self.columns:
            raise SnapshotError("glyph is wider than the configured terminal")
        if self.column + width > self.columns:
            self.column = 0
            self.line_feed()

        self.lines[self.row][self.column] = Cell(character, self.style)
        if width == 2:
            self.lines[self.row][self.column + 1] = Cell("", self.style, True)
        end = self.column + width
        if end == self.columns:
            self.column = self.columns - 1
            self.pending_wrap = True
        else:
            self.column = end


def parse_csi(screen: Screen, sequence: bytes) -> None:
    if len(sequence) > MAX_CSI_BYTES:
        raise SnapshotError("CSI sequence exceeds the 16-byte limit")
    final = chr(sequence[-1])
    body = sequence[:-1]
    if final == "A" and body == b"5":
        screen.cursor_up(5)
        return
    if final == "K" and body == b"2":
        screen.erase_line()
        return
    if final != "m":
        raise SnapshotError(f"unsupported CSI final 0x{sequence[-1]:02x}")
    try:
        text = body.decode("ascii")
    except UnicodeDecodeError as error:
        raise SnapshotError("non-ASCII SGR parameter") from error
    if text and any(part == "" for part in text.split(";")):
        raise SnapshotError("empty SGR parameter")
    try:
        parameters = [] if not text else [int(part) for part in text.split(";")]
    except ValueError as error:
        raise SnapshotError("non-numeric SGR parameter") from error
    screen.set_graphics(parameters)


def parse_transcript(data: bytes, columns: int, rows: int) -> Screen:
    if len(data) > MAX_INPUT_BYTES:
        raise SnapshotError("PTY transcript exceeds the 1 MiB limit")
    screen = Screen(columns, rows)
    index = 0
    printable = bytearray()

    def flush_printable() -> None:
        nonlocal printable
        if not printable:
            return
        try:
            text = printable.decode("utf-8", errors="strict")
        except UnicodeDecodeError as error:
            raise SnapshotError("PTY transcript is not valid UTF-8") from error
        for character in text:
            screen.put(character)
        printable = bytearray()

    while index < len(data):
        byte = data[index]
        if byte >= 0x20 and byte != 0x7F:
            printable.append(byte)
            index += 1
            continue
        flush_printable()
        if byte == 0x1B:
            if index + 1 >= len(data) or data[index + 1] != ord("["):
                raise SnapshotError("unsupported escape sequence")
            end = index + 2
            while end < len(data) and not 0x40 <= data[end] <= 0x7E:
                end += 1
                if end - (index + 2) > MAX_CSI_BYTES:
                    raise SnapshotError("CSI sequence exceeds the 16-byte limit")
            if end >= len(data):
                raise SnapshotError("truncated CSI sequence")
            parse_csi(screen, data[index + 2 : end + 1])
            index = end + 1
        elif byte == 0x0D:
            screen.carriage_return()
            index += 1
        elif byte == 0x0A:
            screen.line_feed()
            index += 1
        elif byte == 0x09:
            screen.tab()
            index += 1
        elif byte == 0x08:
            screen.backspace()
            index += 1
        else:
            raise SnapshotError(f"unsupported control byte 0x{byte:02x}")
    flush_printable()
    return screen


def checked_font_data(font_directory: Path, name: str) -> bytes:
    path = font_directory / name
    data = path.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if digest != FONT_DIGESTS[name]:
        raise SnapshotError(f"font digest mismatch for {name}")
    return data


def render_svg(screen: Screen, font_directory: Path) -> str:
    regular = base64.b64encode(
        checked_font_data(font_directory, "JetBrainsMono-Regular.ttf")
    ).decode("ascii")
    bold = base64.b64encode(
        checked_font_data(font_directory, "JetBrainsMono-Bold.ttf")
    ).decode("ascii")

    cell_width = 9
    cell_height = 20
    pad_x = 24
    pad_y = 22
    chrome_height = 34
    content_width = screen.columns * cell_width
    content_height = screen.rows * cell_height
    width = content_width + pad_x * 2
    height = content_height + pad_y * 2 + chrome_height
    baseline = chrome_height + pad_y + 15

    output = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        (
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" '
            f'height="{height}" viewBox="0 0 {width} {height}">'
        ),
        "<defs>",
        "<style>",
        "@font-face{font-family:'JetBrains Mono';font-style:normal;font-weight:400;"
        f"src:url(data:font/ttf;base64,{regular}) format('truetype');}}",
        "@font-face{font-family:'JetBrains Mono';font-style:normal;font-weight:700;"
        f"src:url(data:font/ttf;base64,{bold}) format('truetype');}}",
        "text{font-family:'JetBrains Mono',monospace;font-size:14px;"
        "font-variant-ligatures:none;white-space:pre}",
        "</style>",
        "</defs>",
        f'<rect width="{width}" height="{height}" rx="14" fill="#161b22"/>',
        f'<rect x="1" y="1" width="{width - 2}" height="{height - 2}" rx="13" '
        'fill="none" stroke="#30363d"/>',
        '<circle cx="20" cy="17" r="5" fill="#ff5f57"/>',
        '<circle cx="38" cy="17" r="5" fill="#febc2e"/>',
        '<circle cx="56" cy="17" r="5" fill="#28c840"/>',
        (
            f'<rect x="{pad_x - 8}" y="{chrome_height}" '
            f'width="{content_width + 16}" height="{content_height + pad_y}" '
            'rx="8" fill="#0b0f14"/>'
        ),
    ]

    for row_index, line in enumerate(screen.lines):
        y = baseline + row_index * cell_height
        column = 0
        while column < screen.columns:
            cell = line[column]
            if cell.continuation:
                column += 1
                continue
            run_start = column
            run_style = cell.style
            run_text = []
            while column < screen.columns:
                current = line[column]
                if current.continuation or current.style != run_style:
                    break
                run_text.append(current.text)
                width = max(1, display_width(current.text[0])) if current.text else 1
                column += width
            if not run_text:
                column += 1
                continue
            text_value = "".join(run_text)
            run_cells = column - run_start
            background = BACKGROUND_PALETTE[run_style.background]
            x = pad_x + run_start * cell_width
            if background is not None:
                output.append(
                    f'<rect x="{x}" y="{y - 15}" width="{run_cells * cell_width}" '
                    f'height="{cell_height}" fill="{background}"/>'
                )
            if text_value.strip(" "):
                foreground = PALETTE[run_style.foreground]
                weight = "700" if run_style.bold else "400"
                opacity = "0.62" if run_style.dim else "1"
                output.append(
                    f'<text x="{x}" y="{y}" fill="{foreground}" '
                    f'font-weight="{weight}" opacity="{opacity}">'
                    f"{html.escape(text_value)}</text>"
                )
    output.append("</svg>")
    return "\n".join(output) + "\n"


def chromium_executable() -> str | None:
    override = os.environ.get("DSH_CHROMIUM")
    if override:
        return override
    for name in ("chromium", "chromium-browser", "google-chrome", "google-chrome-stable"):
        executable = shutil.which(name)
        if executable is not None:
            return executable
    macos_chrome = Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    return str(macos_chrome) if macos_chrome.is_file() else None


def render_png(svg: str, output_path: Path) -> None:
    executable = chromium_executable()
    if executable is None:
        raise SnapshotError(
            "Chromium is required to render PNG; set DSH_CHROMIUM or request SVG output"
        )
    dimensions = re.search(r'<svg[^>]+width="(\d+)"[^>]+height="(\d+)"', svg)
    if dimensions is None:
        raise SnapshotError("generated SVG is missing integer dimensions")
    width, height = dimensions.groups()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="dsh-terminal-render-") as directory:
        svg_path = Path(directory) / "snapshot.svg"
        svg_path.write_text(svg, encoding="utf-8", newline="\n")
        command = [
            executable,
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--force-device-scale-factor=1",
            f"--window-size={width},{height}",
            f"--screenshot={output_path.resolve()}",
            svg_path.resolve().as_uri(),
        ]
        result = subprocess.run(command, capture_output=True, check=False, timeout=30)
        if result.returncode != 0:
            stderr = result.stderr.decode("utf-8", errors="replace")[-1_000:]
            raise SnapshotError(f"Chromium failed: {stderr}")


def self_test() -> None:
    screen = parse_transcript(
        b"old\r\n\x1b[32mnew\x1b[0m\r\n\x1b[1;30;43m ok \x1b[0m",
        12,
        4,
    )
    assert screen.lines[1][0].text == "n"
    assert screen.lines[1][0].style.foreground == 32
    assert screen.lines[2][1].style.bold
    assert screen.lines[2][1].style.background == 43

    redraw = parse_transcript(b"one\r\ntwo\r\nthree\x1b[5A\r\x1b[2Ktop", 8, 6)
    assert "top" == "".join(cell.text for cell in redraw.lines[0][:3])

    wrapped = parse_transcript(b"123456789", 4, 2)
    assert "5678" == "".join(cell.text for cell in wrapped.lines[0])
    assert wrapped.lines[1][0].text == "9"

    rejected = [
        b"\x1b]52;c;secret\x07",
        b"\x1bPpayload\x1b\\",
        b"\x1b[?1049h",
        b"\x1b[1H",
        b"\xff",
    ]
    for case in rejected:
        try:
            parse_transcript(case, 10, 2)
        except SnapshotError:
            pass
        else:
            raise AssertionError(f"unsafe transcript was accepted: {case!r}")

    try:
        parse_transcript(b"x" * (MAX_INPUT_BYTES + 1), 10, 2)
    except SnapshotError:
        pass
    else:
        raise AssertionError("oversized transcript was accepted")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--columns", type=int, default=120)
    parser.add_argument("--rows", type=int, default=24)
    parser.add_argument("--self-test", action="store_true")
    arguments = parser.parse_args()

    if arguments.self_test:
        self_test()
        print("terminal snapshot parser: ok")
        return 0
    if arguments.input is None or arguments.output is None:
        parser.error("--input and --output are required unless --self-test is used")

    data = arguments.input.read_bytes()
    screen = parse_transcript(data, arguments.columns, arguments.rows)
    font_directory = Path(__file__).resolve().parent / "assets" / "fonts"
    svg = render_svg(screen, font_directory)
    if arguments.output.suffix.lower() == ".svg":
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(svg, encoding="utf-8", newline="\n")
    elif arguments.output.suffix.lower() == ".png":
        render_png(svg, arguments.output)
    else:
        raise SnapshotError("output must end in .svg or .png")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, SnapshotError, subprocess.SubprocessError) as error:
        print(f"snapshot render failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
