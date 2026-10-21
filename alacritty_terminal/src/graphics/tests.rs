//! End-to-end tests through the same ANSI processor used by the PTY.

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use ::image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;
use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::*;
use crate::event::{Event, EventListener};
use crate::grid::Dimensions;
use crate::index::{Column, Line, Point};
use crate::term::{Config, Term};
use crate::vte::ansi::{Handler, Processor};

#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Vec<String>>>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.0.lock().unwrap().push(text);
        }
    }
}
struct Size;
impl Dimensions for Size {
    fn columns(&self) -> usize {
        20
    }

    fn screen_lines(&self) -> usize {
        10
    }

    fn total_lines(&self) -> usize {
        10
    }

    fn cell_width(&self) -> f32 {
        8.
    }

    fn cell_height(&self) -> f32 {
        16.
    }
}
fn terminal() -> Term<Listener> {
    Term::new(Config::default(), &Size, Listener::default())
}
fn feed(term: &mut Term<Listener>, bytes: &[u8]) {
    Processor::<crate::vte::ansi::StdSyncHandler>::new().advance(term, bytes);
}
fn kitty(term: &mut Term<Listener>, control: &str, data: &[u8]) {
    feed(term, format!("\x1b_G{control};{}\x1b\\", Base64.encode(data)).as_bytes());
}
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(width, height, Rgba([255, 0, 0, 255])))
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn kitty_query_validates_data_without_storing_or_moving_cursor() {
    let mut term = terminal();
    kitty(&mut term, "a=q,i=7,f=24,s=1,v=1", &[255, 0, 0]);
    assert_eq!(term.event_proxy.0.lock().unwrap().as_slice(), ["\x1b_Gi=7;OK\x1b\\"]);
    assert!(term.graphics.take_queues().is_none());
    assert_eq!(term.grid().cursor.point, Point::new(Line(0), Column(0)));
    kitty(&mut term, "a=p,i=7", &[]);
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("ENOENT"));
}

#[test]
fn chafa_empty_chunks_and_nonzero_base64_padding_bits() {
    let mut term = terminal();
    feed(&mut term, b"\x1b_Ga=T,i=1,s=1,v=1,C=1,m=1\x1b\\\x1b_Gm=1;ChR=\x1b\\\x1b_Gm=1;Hv9=\x1b\\\x1b_Gm=0\x1b\\");
    assert_eq!(term.graphics.take_queues().unwrap().pending[0].pixels, [10, 20, 30, 255]);
}

#[test]
fn kitty_transmit_then_place_scale_crop_and_preserve_text() {
    let mut term = terminal();
    feed(&mut term, b"hello\x1b[H");
    kitty(&mut term, "a=t,i=12,f=100", &png(8, 8));
    assert!(term.graphics.take_queues().is_none());
    kitty(&mut term, "a=p,i=12,p=3,C=1,c=2,r=1,x=2,w=4,z=-1", &[]);
    let queues = term.graphics.take_queues().unwrap();
    assert_eq!((queues.pending[0].width, queues.pending[0].height), (16, 16));
    assert_eq!(term.grid()[Line(0)][Column(0)].c, 'h');
    assert_eq!(term.grid().cursor.point, Point::new(Line(0), Column(0)));
    assert_eq!(term.grid()[Line(0)][Column(0)].graphics().unwrap()[0].z_index, -1);
    feed(&mut term, b"X");
    assert_eq!(term.grid()[Line(0)][Column(0)].c, 'X');
    assert!(term.grid()[Line(0)][Column(0)].graphics().is_some());
}

#[test]
fn kitty_chunks_work_across_every_pty_byte_boundary_and_ack_once() {
    let mut term = terminal();
    let data = png(40, 40);
    let encoded = Base64.encode(&data);
    let cut = (encoded.len() / 8) * 4;
    let bytes = format!(
        "\x1b_Ga=T,i=4,f=100,m=1;{}\x1b\\\x1b_Gm=0;{}\x1b\\",
        &encoded[..cut],
        &encoded[cut..]
    );
    let mut parser = Processor::<crate::vte::ansi::StdSyncHandler>::new();
    for byte in bytes.bytes() {
        parser.advance(&mut term, &[byte]);
    }
    assert_eq!(term.event_proxy.0.lock().unwrap().len(), 1);
    assert_eq!(term.graphics.take_queues().unwrap().pending.len(), 1);
    assert_eq!(term.grid().cursor.point, Point::new(Line(3), Column(5)));
}

#[test]
fn kitty_zlib_and_quiet_errors() {
    let mut term = terminal();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[255, 0, 0, 255]).unwrap();
    kitty(&mut term, "a=T,i=9,f=32,s=1,v=1,o=z,C=1,q=1", &encoder.finish().unwrap());
    assert!(term.event_proxy.0.lock().unwrap().is_empty());
    assert_eq!(term.graphics.take_queues().unwrap().pending[0].pixels, [255, 0, 0, 255]);
    kitty(&mut term, "a=T,i=8,s=4097,v=1,q=2", &[]);
    assert!(term.event_proxy.0.lock().unwrap().is_empty());
    kitty(&mut term, "a=T,i=8,s=4097,v=1,q=1", &[]);
    assert!(term.event_proxy.0.lock().unwrap()[0].contains("EINVAL"));
}

#[test]
fn kitty_multiple_placements_replace_and_delete_without_erasing_text() {
    let mut term = terminal();
    kitty(&mut term, "a=T,i=5,p=1,s=1,v=1,C=1", &[0, 255, 0, 255]);
    feed(&mut term, b"\x1b[2;2H");
    kitty(&mut term, "a=p,i=5,p=2,C=1", &[]);
    let queues = term.graphics.take_queues().unwrap();
    assert_eq!(queues.pending.len(), 2);
    kitty(&mut term, "a=p,i=5,p=1,C=1", &[]);
    assert!(term.grid()[Line(0)][Column(0)].graphics().is_none());
    assert_eq!(term.grid()[Line(1)][Column(1)].graphics().unwrap().len(), 2);
    kitty(&mut term, "a=d,d=i,i=5,p=2", &[]);
    assert_eq!(term.grid()[Line(1)][Column(1)].graphics().unwrap().len(), 1);
    kitty(&mut term, "a=d,d=I,i=5", &[]);
    assert!(term.grid()[Line(1)][Column(1)].graphics().is_none());
    kitty(&mut term, "a=p,i=5", &[]);
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("ENOENT"));
}

#[test]
fn kitty_delete_keeps_scrollback_and_buffer_ids_are_isolated() {
    let mut term = terminal();
    kitty(&mut term, "a=T,i=1,s=1,v=1,C=1", &[255; 4]);
    feed(&mut term, b"\x1b[10;1H\n");
    assert!(term.grid()[Line(-1)][Column(0)].graphics().is_some());
    kitty(&mut term, "a=d,d=A", &[]);
    assert!(term.grid()[Line(-1)][Column(0)].graphics().is_some());
    feed(&mut term, b"\x1b[?1049h");
    kitty(&mut term, "a=p,i=1", &[]);
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("ENOENT"));
    feed(&mut term, b"\x1b[?1049l");
    kitty(&mut term, "a=p,i=1,C=1", &[]);
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("OK"));
    feed(&mut term, b"\x1bc");
    kitty(&mut term, "a=p,i=1", &[]);
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("ENOENT"));
}

#[test]
fn canceled_apc_and_bad_data_do_not_leak_or_render() {
    let mut term = terminal();
    let mut parser = Processor::<crate::vte::ansi::StdSyncHandler>::new();
    parser.advance(&mut term, b"\x1b_Ga=T,i=1,s=1,v=1;////\x18hello");
    assert!(term.graphics.take_queues().is_none());
    assert_eq!(term.grid()[Line(0)][Column(0)].c, 'h');
    feed(&mut term, b"\x1b_Ga=T,i=2,s=1,v=1;!bad!\x1b\\");
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("EINVAL"));
    assert!(term.graphics.take_queues().is_none());
}

#[test]
fn render_snapshots_do_not_clear_live_graphics() {
    let mut term = terminal();
    kitty(&mut term, "a=T,i=1,s=8,v=16,C=1", &[255; 8 * 16 * 4]);
    term.graphics.take_queues();
    let snapshot = term.grid()[Line(0)][Column(0)].clone();
    let copied = snapshot.graphics().unwrap()[0].clone();
    drop(snapshot);
    drop(copied);
    assert!(term.graphics.take_queues().is_none());
    term.clear_screen(crate::vte::ansi::ClearMode::All);
    term.clear_screen(crate::vte::ansi::ClearMode::Saved);
    let queues = term.graphics.take_queues().unwrap();
    assert_eq!(queues.remove_queue.len(), 1);
    assert!(queues.clear_subregions.is_empty());
}

#[test]
fn iterm_png_jpeg_dimensions_and_multipart() {
    let mut term = terminal();
    let data = png(8, 8);
    feed(
        &mut term,
        format!("\x1b]1337;File=inline=1;width=2;height=1:{}\x07", Base64.encode(&data)).as_bytes(),
    );
    let queues = term.graphics.take_queues().unwrap();
    assert_eq!((queues.pending[0].width, queues.pending[0].height), (16, 16));
    let encoded = Base64.encode(&data);
    let cut = encoded.len() / 2;
    let sequence = format!(
        "\x1b]1337;MultipartFile=inline=1;width=8px\x1b\\\x1b]1337;FilePart={}\x1b\\\x1b]1337;\
         FilePart={}\x1b\\\x1b]1337;FileEnd\x1b\\",
        &encoded[..cut],
        &encoded[cut..]
    );
    let mut parser = Processor::<crate::vte::ansi::StdSyncHandler>::new();
    for byte in sequence.bytes() {
        parser.advance(&mut term, &[byte]);
    }
    assert_eq!(term.graphics.take_queues().unwrap().pending[0].width, 8);
    let mut jpeg = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(::image::RgbImage::from_pixel(4, 2, ::image::Rgb([50, 100, 150])))
        .write_to(&mut jpeg, ImageFormat::Jpeg)
        .unwrap();
    feed(
        &mut term,
        format!("\x1b]1337;File=inline=1;width=50%:{}\x07", Base64.encode(jpeg.into_inner()))
            .as_bytes(),
    );
    assert_eq!(term.graphics.take_queues().unwrap().pending[0].width, 80);
}

#[test]
fn iterm_noninline_bad_dimensions_and_canceled_transfers_are_ignored() {
    let mut term = terminal();
    for options in
        ["inline=0", "inline=1;width=0", "inline=1;height=999999px", "inline=1;width=bad"]
    {
        feed(
            &mut term,
            format!("\x1b]1337;File={options}:{}\x07", Base64.encode(png(1, 1))).as_bytes(),
        );
        assert!(term.graphics.take_queues().is_none());
    }
    feed(&mut term, format!("\x1b]1337;File=inline=1:{}\x18", Base64.encode(png(1, 1))).as_bytes());
    assert!(term.graphics.take_queues().is_none());
}

#[test]
fn opaque_region_uses_x_and_includes_the_right_bottom_edges() {
    let mut graphic = GraphicData {
        id: GraphicId(0),
        width: 4,
        height: 2,
        color_type: ColorType::Rgba,
        pixels: vec![255; 32],
        is_opaque: false,
    };
    graphic.pixels[3] = 0;
    assert!(graphic.is_filled(1, 0, 3, 2));
    assert!(!graphic.is_filled(0, 0, 1, 1));
    assert!(!graphic.is_filled(usize::MAX, 0, 1, 1));
}

#[test]
fn zero_cell_dimensions_and_ignored_sixel_header_do_not_panic() {
    let mut term = terminal();
    term.graphics.cell_width = 0.;
    term.graphics.cell_height = 0.;
    feed(&mut term, b"\x1bPq#1;2;100;0;0~\x1b\\");
    assert!(term.graphics.take_queues().is_some());
    feed(&mut term, b"\x1bP?1q~\x1b\\");
    assert!(term.graphics.take_queues().is_none());
}

#[test]
fn kitty_unicode_placeholders_support_inheritance_high_ids_and_deletion() {
    let mut term = terminal();
    kitty(&mut term, "a=T,i=33554474,p=7,U=1,c=2,r=2,f=100,q=2", &png(16, 32));
    assert_eq!(term.grid().cursor.point, Point::new(Line(0), Column(0)));
    term.graphics.take_queues();
    feed(
        &mut term,
        "\x1b[38;5;42;58;5;7m\u{10eeee}\u{305}\u{305}\u{30e}\u{10eeee}\x1b[0m".as_bytes(),
    );
    let first = term.graphics.placeholder(&term.grid()[Line(0)][Column(0)], None).unwrap();
    let second = term.graphics.placeholder(&term.grid()[Line(0)][Column(1)], Some(&first)).unwrap();
    assert_eq!(first.image_id, 33554474);
    assert_eq!(second.offset_x, 8);
    kitty(&mut term, "a=d,d=A", &[]);
    assert!(term.graphics.placeholder(&term.grid()[Line(0)][Column(0)], None).is_some());
    kitty(&mut term, "a=d,d=i,i=33554474,p=7", &[]);
    assert!(term.graphics.placeholder(&term.grid()[Line(0)][Column(0)], None).is_none());
}

#[test]
fn kitty_file_transport_rejects_special_files_and_honors_offset() {
    let mut term = terminal();
    let path = std::env::temp_dir().join(format!("alacritty-image-test-{}", std::process::id()));
    std::fs::write(&path, [1, 2, 3, 255, 0, 0, 255]).unwrap();
    kitty(&mut term, "a=T,i=1,t=f,s=1,v=1,O=3,S=4,C=1", path.to_str().unwrap().as_bytes());
    assert_eq!(term.graphics.take_queues().unwrap().pending[0].pixels, [255, 0, 0, 255]);
    std::fs::remove_file(&path).unwrap();
    for file in ["/dev/zero", "/proc/self/mem", "/tmp/no-such-alacritty-image"] {
        kitty(&mut term, "a=q,i=2,t=f,f=100", file.as_bytes());
        assert!(
            term.event_proxy
                .0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains("EBADF:Failed to read image file")
        );
    }
}

#[test]
fn oversized_commands_abort_chunks_and_new_images_recover() {
    let mut term = terminal();
    feed(&mut term, b"\x1b_Ga=T,i=1,m=1,s=1,v=1;////\x1b\\");
    let oversized = format!("\x1b_Gm=0;{}\x1b\\", "A".repeat(9000));
    feed(&mut term, oversized.as_bytes());
    assert!(term.graphics.take_queues().is_none());
    kitty(&mut term, "a=T,i=2,s=1,v=1,C=1", &[255; 4]);
    assert_eq!(term.graphics.take_queues().unwrap().pending.len(), 1);
    feed(
        &mut term,
        format!("\x1b]1337;File=inline=1:{}\x1b[H", Base64.encode(png(1, 1))).as_bytes(),
    );
    assert!(term.graphics.take_queues().is_none());
}

#[test]
fn dimensions_cannot_overflow_resize_arithmetic() {
    let mut term = terminal();
    feed(
        &mut term,
        format!("\x1b]1337;File=inline=1;width={}px:{}\x07", usize::MAX, Base64.encode(png(2, 2)))
            .as_bytes(),
    );
    assert!(term.graphics.take_queues().is_none());
    kitty(&mut term, "a=T,i=1,c=4294967295,r=4294967295,f=100", &png(2, 2));
    assert!(term.event_proxy.0.lock().unwrap().last().unwrap().contains("EINVAL"));
}

#[test]
fn canceled_sixel_is_discarded() {
    let mut term = terminal();
    feed(&mut term, b"\x1bPq#1;2;100;0;0~\x18hello");
    assert!(term.graphics.take_queues().is_none());
    assert_eq!(term.grid()[Line(0)][Column(0)].c, 'h');
}

#[test]
fn image_numbers_resolve_to_the_newest_transmission() {
    let mut term = terminal();
    kitty(&mut term, "a=t,i=100,s=1,v=1", &[255, 0, 0, 255]);
    kitty(&mut term, "a=t,I=7,s=1,v=1", &[0, 255, 0, 255]);
    let first = term.event_proxy.0.lock().unwrap().last().unwrap().clone();
    kitty(&mut term, "a=t,I=7,s=1,v=1", &[0, 0, 255, 255]);
    let second = term.event_proxy.0.lock().unwrap().last().unwrap().clone();
    assert_ne!(first, second);
    kitty(&mut term, "a=p,I=7,C=1", &[]);
    let queues = term.graphics.take_queues().unwrap();
    assert_eq!(queues.pending[0].pixels, [0, 0, 255, 255]);
}

#[cfg(unix)]
#[test]
fn kitty_posix_shared_memory_is_read_and_unlinked() {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    let mut term = terminal();
    // Darwin limits shared memory names to 31 characters.
    let name = CString::new(format!("/agfx-test-{}", std::process::id())).unwrap();
    let fd = unsafe {
        libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
    };
    assert!(fd >= 0);
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.set_len(7).unwrap();
    let pointer = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            7,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_ne!(pointer, libc::MAP_FAILED);
    unsafe {
        std::slice::from_raw_parts_mut(pointer.cast::<u8>(), 7)
            .copy_from_slice(&[1, 2, 3, 10, 20, 30, 255]);
        assert_eq!(libc::munmap(pointer, 7), 0);
    }
    kitty(&mut term, "a=T,i=1,t=s,s=1,v=1,O=3,S=4,C=1", name.as_bytes());
    assert_eq!(term.graphics.take_queues().unwrap().pending[0].pixels, [10, 20, 30, 255]);
    let reopened = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    assert!(reopened < 0);
}
