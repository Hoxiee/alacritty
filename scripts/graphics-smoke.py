#!/usr/bin/env python3
"""Verify graphics in a real X11 window using Xvfb, ImageMagick, and Pillow."""

import argparse
import base64
import math
import os
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from PIL import Image


def kitty(control, pixels):
    encoded = base64.b64encode(pixels).decode()
    parts = [encoded[i:i + 4096] for i in range(0, len(encoded), 4096)] or [""]
    return "".join(
        "\x1b_G" + (control + "," if i == 0 else "")
        + f"m={int(i != len(parts) - 1)};{part}\x1b\\"
        for i, part in enumerate(parts)
    ).encode()


def placeholder_pixels(cell_width, cell_height):
    scale = min(8 * cell_width / 72, 2 * cell_height / 42)
    area = math.floor(72 * scale + 0.5) * math.floor(42 * scale + 0.5)
    return area, area


def run_case(binary, directory, name, sequence, expected, renderer=None):
    script = directory / "child.py"
    log_path = directory / f"{name}.log"
    args = [str(binary), "--config-file", str(directory / "config.toml"), "-vv"]
    if renderer:
        args += ["-o", f'debug.renderer="{renderer}"']
    args += ["-e", "python3", str(script)]
    with log_path.open("w") as log:
        script.write_text(f"import sys,time\nsys.stdout.buffer.write(b'\\x1b[?25l' + {sequence!r} + b'\\x1b[?25l');sys.stdout.flush();time.sleep(30)\n")
        proc = subprocess.Popen(args, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if proc.poll() is not None:
                    raise RuntimeError(log_path.read_text())
                time.sleep(0.1)
                if "Initialisation complete" in log_path.read_text():
                    break
            else:
                raise RuntimeError("Alacritty did not initialize within 10 seconds")
            capture = directory / f"{name}.xwd"
            png = directory / f"{name}.png"
            if callable(expected):
                match = re.search(r"Cell size: (\d+) x (\d+)", log_path.read_text())
                assert match, "Missing cell dimensions in renderer log"
                expected = expected(*map(int, match.groups()))
            # Initialization can precede PTY output and the first image frame on CI.
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                if proc.poll() is not None:
                    raise RuntimeError(log_path.read_text())
                subprocess.run(["xwd", "-root", "-silent", "-out", str(capture)], check=True)
                subprocess.run([shutil.which("magick") or "convert", str(capture), str(png)], check=True)
                with Image.open(png) as image:
                    rgb = image.convert("RGB")
                    data = rgb.get_flattened_data() if hasattr(rgb, "get_flattened_data") else rgb.getdata()
                    red = sum(r > 220 and g < 20 and b < 20 for r, g, b in data)
                if expected[0] <= red <= expected[1]:
                    break
                time.sleep(0.2)
            else:
                raise AssertionError(f"{name}: {red} red pixels, expected {expected}")
            assert "[ERROR]" not in log_path.read_text(), log_path.read_text()
            print(f"{name}: {red} red pixels OK", flush=True)
        finally:
            proc.terminate()
            proc.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/release/alacritty"))
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    for program in ["Xvfb", "xwd", "chafa"]:
        if not shutil.which(program):
            parser.error(f"Missing {program}")
    if not (shutil.which("magick") or shutil.which("convert")):
        parser.error("Missing ImageMagick")
    with tempfile.TemporaryDirectory(prefix="alacritty-graphics-") as temp:
        directory = (args.output or Path(temp)).resolve()
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "config.toml").write_text('[font]\nsize=12\n[cursor]\nstyle="Beam"\n[window]\ndimensions={columns=80,lines=25}\n')
        fixture = directory / "fixture.png"
        Image.new("RGB", (72, 42), (255, 0, 0)).save(fixture)
        # Let Xvfb allocate a display instead of relying on a fixed display number.
        read_fd, write_fd = os.pipe()
        xvfb = subprocess.Popen(["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "800x600x24", "-nolisten", "tcp"], pass_fds=(write_fd,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.close(write_fd)
        display = os.read(read_fd, 32).decode().strip()
        os.close(read_fd)
        os.environ.update(DISPLAY=":" + display, WINIT_UNIX_BACKEND="x11", LIBGL_ALWAYS_SOFTWARE="1")
        os.environ.pop("WAYLAND_DISPLAY", None)
        try:
            pixels = bytes([255, 0, 0]) * 80 * 40
            sequence = kitty("a=T,i=1,f=24,s=80,v=40,C=1,q=2", pixels)
            for renderer in ["glsl3", "gles2"]:
                run_case(binary, directory, f"kitty-{renderer}", sequence, (3200, 3200), renderer)
            # Negative z-index above backgrounds, and the lower band behind them.
            background = b"\x1b[44m" + b" " * 8 + b"\x1b[H"
            for z_index, expected in [(-1, (3200, 3200)), (-1073741825, lambda w, h: (3200 - min(80, 8 * w) * min(40, h),) * 2)]:
                run_case(binary, directory, f"kitty-layer-{z_index}", background + kitty(f"a=T,f=24,s=80,v=40,C=1,q=2,z={z_index}", pixels), expected)
            # Erasing part of an image removes only those cells.
            erase = sequence + b"\x1b[1;5H\x1b[K"
            run_case(binary, directory, "kitty-erase", erase, lambda w, h: (3200 - max(0, 80 - 4 * w) * min(40, h),) * 2)
            # An RGB width not divisible by four detects GL unpack alignment errors.
            run_case(binary, directory, "kitty-odd-width", kitty("a=T,f=24,s=71,v=42,C=1,q=2", bytes([255, 0, 0]) * 71 * 42), (2982, 2982))
            virtual = kitty("a=T,i=42,U=1,c=8,r=2,f=24,s=72,v=42,q=2", bytes([255, 0, 0]) * 72 * 42)
            placeholders = "\x1b[38;5;42m"
            for row in ["\u0305", "\u030d"]:
                placeholders += "\U0010eeee" + row + "\u0305" + "\U0010eeee" * 7 + "\r\n"
            run_case(binary, directory, "kitty-placeholders", virtual + placeholders.encode(), placeholder_pixels)
            chafa_options = ["--passthrough=none", "--animate=off", "--exact-size=on", "--size=8x2", "--font-ratio=9/21"]
            # Ubuntu 24.04 ships Chafa 1.14, before the --probe option was added.
            if "--probe" in subprocess.check_output(["chafa", "--help"], text=True):
                chafa_options.append("--probe=off")
            for fmt in ["kitty", "sixels", "iterm"]:
                sequence = subprocess.check_output(["chafa", *chafa_options, "--format=" + fmt, str(fixture)])
                run_case(binary, directory, "chafa-" + fmt, sequence, (2500, 6000))
        finally:
            xvfb.terminate()
            xvfb.wait(timeout=10)


if __name__ == "__main__":
    main()
