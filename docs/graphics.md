# Images in this Alacritty fork

Build and run this fork's master branch:

```sh
git clone https://github.com/Hoxiee/alacritty.git
cd alacritty
git checkout master
cargo build --release --locked
./target/release/alacritty
```

Inside the new terminal, display a photo with [Chafa](https://hpjansson.org/chafa/):

```sh
chafa --format=kitty --animate=off photo.jpg
chafa --format=sixels --animate=off photo.jpg
```

On Arch Linux, install Chafa with `sudo pacman -S chafa`. On Ubuntu use
`sudo apt install chafa`. No image-specific Alacritty configuration is required.
Use explicit output formats when a program assumes upstream Alacritty lacks graphics.
For Kitty's `kitten icat`, direct transfer also works over SSH:

```sh
kitten icat --transfer-mode=stream photo.jpg
```

For an Arch package with desktop entry, manpages and completions:

```sh
sudo pacman -S --needed base-devel rustup cmake pkgconf scdoc freetype2 fontconfig libxcb libxkbcommon libxkbcommon-x11 libglvnd
scripts/build-arch-package.sh
sudo pacman -U target/arch-package/alacritty-graphics-*.pkg.tar.zst
```

The script packages the checked-out commit. The package provides `alacritty` and
replaces Arch's upstream `alacritty` package when installed. Preview packages
can also be downloaded from [this fork's releases](https://github.com/Hoxiee/alacritty/releases).
System terminfo is supplied by Arch's `ncurses`; the package includes its terminfo
source under `/usr/share/doc/alacritty-graphics/alacritty.info`.

TUI clients can use ordinary placements or Kitty Unicode placeholders. tmux must
be configured to pass image escape sequences through; direct terminal operation
is the first step when diagnosing an application that does not display images.

## Compatibility

| Feature | Support |
| --- | --- |
| Sixel pixels, palette, raster size, repetition, XTSMGRAPHICS queries | Yes |
| Kitty `a=t,T,q,p,d` | Yes |
| Kitty RGB24, RGBA32, PNG, zlib | Yes |
| Direct chunked transfer | Yes, base64 chunks up to 4096 bytes |
| Regular and temporary image files, `O` / `S` | Yes |
| POSIX shared memory | Yes on Unix |
| Windows named shared memory | Unavailable; use stream or file transfer |
| Image and placement ids, image numbers, repeated placements | Yes |
| Crop, cell sizing, aspect ratio, pixel offsets, cursor policy | Yes |
| Foreground, below-text and below-background z-index | Yes |
| Delete by id, placement, cursor, cell, row, column, range or z-index | Yes |
| Unicode placeholders, combining marks, inherited coordinates | Yes |
| Animation and relative placements | Unavailable; return `ENOTSUP` |
| iTerm2 `File`, `MultipartFile`, `FilePart`, `FileEnd` | Inline images |
| iTerm2 PNG, JPEG, GIF, BMP, WebP, TIFF | First/static image |
| iTerm2 width/height in cells, pixels, percent; aspect ratio | Yes |

Graphics follow the terminal grid and scrollback. Erasing cells removes their image
regions; writing text preserves Kitty placements so text can appear on images.
Primary and alternate screens have separate Kitty image stores. Resetting the
terminal discards protocol state. Font changes scale existing graphics with cells;
clients can re-place images for a new layout after window resizing.

## Limits and file handling

An image or display rectangle is at most 4096 × 4096 pixels. Transfers and decoded
images are limited to 64 MiB. Each Kitty screen buffer has a 320 MiB source image
quota; normal placements, virtual placements, queued uploads and GPU textures are
also bounded. Texture/image counts are limited to 1000, with at most 20 image
references per cell. Older images may be evicted under pressure.

Only regular files are accepted for file transfer. Device files and sensitive
`/proc`, `/sys`, and `/dev` paths are refused. File read failures share the same
`EBADF` response. Temporary files are removed only in a known temporary directory
and only if their canonical path contains `tty-graphics-protocol`. POSIX shared
memory objects are unlinked after reading, as required by the protocol.

## Validation and maintenance

```sh
cargo test --workspace --locked
cargo test -p alacritty_terminal --no-default-features --locked
cargo +1.85.0 clippy --workspace --all-targets --locked
cargo +nightly fmt --all -- --check
cargo build --release --locked
```

On Linux, the pixel test starts a real Alacritty window with Mesa under Xvfb. It
checks OpenGL 3.3 and GLES2, z-index, partial erasure, odd RGB widths, Unicode
placeholders, and Chafa's Kitty, Sixel and iTerm2 output:

```sh
# Requires Xvfb, xwd, ImageMagick, Chafa and Python Pillow.
python3 scripts/graphics-smoke.py --output target/graphics-smoke
```

Linux graphics CI runs the same checks and retains the release binary, screenshots
and renderer logs. A separate Arch Linux job builds and installs the package,
then checks pixels using the packaged binary. The local
`vte-graphics` workspace member is required: the published parser ignores APC.
See [its provenance](../vte-graphics/FORK.md) before updating it.

The implementation is based on official Alacritty commit `54214706`, with Sixel
code ported from ayosec's graphics fork. Linux and Arch are the maintained targets.
Updating upstream uses the official `alacritty/alacritty` master branch.

Protocol references:

- [Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
- [iTerm2 images protocol](https://iterm2.com/documentation-images.html)
- [Xterm control sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)
