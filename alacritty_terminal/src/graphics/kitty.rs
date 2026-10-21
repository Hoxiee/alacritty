//! Static images from the Kitty graphics protocol.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use base64::Engine;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use flate2::read::ZlibDecoder;
use image::{RgbaImage, imageops};

use super::image::{self as bitmap, MAX_IMAGE_BYTES};
use super::{
    ColorType, GraphicData, GraphicId, MAX_GRAPHIC_DIMENSIONS, PlacementOptions, TextureRef,
};
use crate::event::{Event, EventListener};
use crate::grid::Dimensions;
use crate::index::{Column, Line};
use crate::term::Term;
use crate::term::cell::Cell;
use crate::vte::ansi::Color;

const MAX_COMMAND_BYTES: usize = 8192;
const STORAGE_QUOTA: usize = 320 * 1024 * 1024;
const MAX_STORED_IMAGES: usize = 1000;

// Chafa 1.14 emits nonzero padding bits in some otherwise valid base64 chunks.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

#[derive(Debug, Default)]
pub(crate) struct State {
    command: Vec<u8>,
    overflow: bool,
    transfer: Option<Transfer>,
    images: BTreeMap<u32, StoredImage>,
    placements: HashMap<GraphicId, Placement>,
    virtual_placements: BTreeMap<(u32, u32), VirtualPlacement>,
    last_id: u32,
    used_bytes: usize,
    generation: u64,
    virtual_bytes: usize,
}

#[derive(Debug)]
struct StoredImage {
    data: GraphicData,
    number: u32,
    generation: u64,
}

#[derive(Debug)]
struct Placement {
    texture: Weak<TextureRef>,
    bytes: usize,
    image_id: u32,
    placement_id: u32,
    z_index: i32,
}

#[derive(Debug)]
struct VirtualPlacement {
    texture: Arc<TextureRef>,
    columns: usize,
    rows: usize,
    z_index: i32,
}

/// Resolved Unicode placeholder image cell.
#[derive(Clone, Debug)]
pub struct Placeholder {
    pub texture_id: GraphicId,
    pub offset_x: u16,
    pub offset_y: u16,
    pub z_index: i32,
    pub image_id: u32,
    row: usize,
    column: usize,
    high: u32,
    foreground: Color,
    underline: Option<Color>,
}

#[derive(Debug)]
struct Transfer {
    command: Command,
    data: Vec<u8>,
}

#[derive(Debug, Default)]
struct Command {
    keys: HashMap<u8, String>,
}

impl Command {
    fn parse(data: &[u8]) -> Result<(Self, &[u8]), &'static str> {
        let data = data.strip_prefix(b"G").ok_or("EINVAL:Not a graphics command")?;
        let (header, payload) = match data.iter().position(|&b| b == b';') {
            Some(i) => (&data[..i], &data[i + 1..]),
            None => (data, &[][..]),
        };
        let mut command = Self::default();
        if !header.is_empty() {
            for field in header.split(|&b| b == b',') {
                if field.len() < 3 || field[1] != b'=' || !field[0].is_ascii_alphabetic() {
                    return Err("EINVAL:Invalid control data");
                }
                let value =
                    std::str::from_utf8(&field[2..]).map_err(|_| "EINVAL:Invalid control data")?;
                if command.keys.insert(field[0], value.to_owned()).is_some() {
                    return Err("EINVAL:Duplicate control key");
                }
            }
        }
        Ok((command, payload))
    }

    fn character(&self, key: u8, default: u8) -> Result<u8, &'static str> {
        match self.keys.get(&key) {
            None => Ok(default),
            Some(value) if value.len() == 1 => Ok(value.as_bytes()[0]),
            _ => Err("EINVAL:Invalid character parameter"),
        }
    }

    fn number(&self, key: u8, default: u32) -> Result<u32, &'static str> {
        self.keys
            .get(&key)
            .map_or(Ok(default), |v| v.parse().map_err(|_| "EINVAL:Invalid numeric parameter"))
    }

    fn z_index(&self) -> Result<i32, &'static str> {
        self.keys.get(&b'z').map_or(Ok(0), |v| v.parse().map_err(|_| "EINVAL:Invalid z-index"))
    }

    fn reply<L: EventListener>(&self, term: &Term<L>, result: Result<(), &'static str>) {
        let quiet = self.number(b'q', 0).unwrap_or(0);
        let id = self.number(b'i', 0).unwrap_or(0);
        let number = self.number(b'I', 0).unwrap_or(0);
        let query = self.character(b'a', b't') == Ok(b'q');
        if !query && (quiet == 2 || (quiet == 1 && result.is_ok()) || (id == 0 && number == 0)) {
            return;
        }
        let mut response = format!("\x1b_Gi={id}");
        if number != 0 {
            response.push_str(&format!(",I={number}"));
        }
        if let Ok(p) = self.number(b'p', 0) {
            if p != 0 {
                response.push_str(&format!(",p={p}"));
            }
        }
        response.push(';');
        response.push_str(result.err().unwrap_or("OK"));
        response.push_str("\x1b\\");
        term.event_proxy.send_event(Event::PtyWrite(response));
    }
}

impl State {
    pub(crate) fn start_command(&mut self) {
        self.command.clear();
        self.overflow = false;
    }

    pub(crate) fn put(&mut self, byte: u8) {
        if self.command.len() >= MAX_COMMAND_BYTES {
            self.overflow = true;
        }
        if !self.overflow {
            self.command.push(byte);
        }
    }

    fn resolve_id(&self, command: &Command) -> Result<u32, &'static str> {
        let id = command.number(b'i', 0)?;
        let number = command.number(b'I', 0)?;
        if id != 0 && number != 0 {
            return Err("EINVAL:Both image id and number specified");
        }
        if number == 0 {
            return Ok(id);
        }
        self.images
            .iter()
            .filter(|(_, i)| i.number == number)
            .max_by_key(|(_, i)| i.generation)
            .map(|(&id, _)| id)
            .ok_or("ENOENT:Image not found")
    }

    fn allocate_id(&mut self) -> u32 {
        loop {
            self.last_id = self.last_id.wrapping_add(1).max(1);
            if !self.images.contains_key(&self.last_id) {
                return self.last_id;
            }
        }
    }

    fn recount_virtual_bytes(&mut self) {
        self.virtual_bytes = self
            .virtual_placements
            .values()
            .map(|p| p.texture.width as usize * p.texture.height as usize * 4)
            .sum();
    }

    fn remove_data(&mut self, id: u32) {
        self.virtual_placements.retain(|(image, _), _| *image != id);
        self.recount_virtual_bytes();
        if let Some(image) = self.images.remove(&id) {
            self.used_bytes -= image.data.pixels.len();
        }
    }
}

pub(crate) fn finish_command<L: EventListener>(term: &mut Term<L>, terminated: bool) {
    let mut state = std::mem::take(&mut term.graphics.kitty);
    state.placements.retain(|_, p| p.texture.strong_count() > 0);
    if !terminated || state.overflow {
        state.transfer = None;
    } else if state.command.first() == Some(&b'G') {
        match Command::parse(&state.command) {
            Ok((command, payload)) => {
                // Own the payload before mutating protocol state.
                let payload = payload.to_vec();
                execute(term, &mut state, command, &payload);
            },
            Err(err) => {
                state.transfer = None;
                log::debug!("Invalid Kitty graphics command: {err}");
            },
        }
    }
    state.command.clear();
    term.graphics.kitty = state;
}

fn execute<L: EventListener>(
    term: &mut Term<L>,
    state: &mut State,
    mut command: Command,
    payload: &[u8],
) {
    let action = match command.character(b'a', b't') {
        Ok(action) => action,
        Err(err) => {
            command.reply(term, Err(err));
            return;
        },
    };
    if action == b'd' {
        state.transfer = None;
        let result = delete(term, state, &command);
        // Deletion commands do not produce acknowledgements.
        if let Err(err) = result {
            log::debug!("Invalid Kitty deletion: {err}");
        }
        return;
    }
    if action == b'p' {
        state.transfer = None;
        let result = state.resolve_id(&command).and_then(|id| {
            let result = place(term, state, &command, id);
            command.keys.insert(b'i', id.to_string());
            result
        });
        command.reply(term, result);
        return;
    }
    if !matches!(action, b't' | b'T' | b'q') {
        state.transfer = None;
        command.reply(term, Err("ENOTSUP:Animation is not supported"));
        return;
    }
    let more = command.number(b'm', 0);
    if !matches!(more, Ok(0 | 1)) || !matches!(command.number(b'q', 0), Ok(0..=2)) {
        state.transfer = None;
        command.reply(term, Err("EINVAL:Invalid chunk or quiet parameter"));
        return;
    }
    let medium = command.character(b't', b'd').unwrap_or(0);
    let mut transfer = if let Some(mut transfer) = state.transfer.take() {
        if command.keys.keys().any(|k| !matches!(k, b'm' | b'q')) {
            transfer.command.reply(term, Err("EINVAL:Invalid continuation chunk"));
            return;
        }
        if let Some(q) = command.keys.remove(&b'q') {
            transfer.command.keys.insert(b'q', q);
        }
        transfer
    } else {
        Transfer { command, data: Vec::new() }
    };
    let actual_medium = transfer.command.character(b't', b'd').unwrap_or(0);
    let chunk = if actual_medium == b'd' {
        if payload.len() > 4096 || (more == Ok(1) && payload.len() % 4 != 0) {
            Err("EINVAL:Invalid chunk size")
        } else {
            BASE64.decode(payload).map_err(|_| "EINVAL:Invalid base64 data")
        }
    } else if more == Ok(1) || medium == 0 {
        Err("EINVAL:Invalid transmission medium")
    } else {
        read_medium(&transfer.command, payload)
    };
    let chunk = match chunk {
        Ok(chunk) => chunk,
        Err(err) => {
            transfer.command.reply(term, Err(err));
            return;
        },
    };
    if transfer.data.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
        transfer.command.reply(term, Err("E2BIG:Image data exceeds limit"));
        return;
    }
    transfer.data.extend_from_slice(&chunk);
    if more == Ok(1) {
        state.transfer = Some(transfer);
        return;
    }
    command = transfer.command;
    let result = decode(&command, transfer.data).and_then(|graphic| {
        if command.character(b'a', b't')? == b'q' {
            return Ok(());
        }
        let number = command.number(b'I', 0)?;
        let mut id = command.number(b'i', 0)?;
        if id != 0 && number != 0 {
            return Err("EINVAL:Both image id and number specified");
        }
        if id == 0 {
            id = state.allocate_id();
        }
        remove_placements(term, state, |_, placement| placement.image_id == id, true);
        state.remove_data(id);
        // Prefer evicting images without placements before older visible images.
        while state.used_bytes + graphic.pixels.len() > STORAGE_QUOTA
            || state.images.len() >= MAX_STORED_IMAGES
        {
            let victim = state
                .images
                .keys()
                .copied()
                .find(|id| {
                    !state.placements.values().any(|p| p.image_id == *id)
                        && !state.virtual_placements.keys().any(|(i, _)| i == id)
                })
                .or_else(|| state.images.keys().next().copied())
                .ok_or("ENOSPC:Image storage full")?;
            remove_placements(term, state, |_, p| p.image_id == victim, true);
            state.remove_data(victim);
        }
        state.used_bytes += graphic.pixels.len();
        state.generation += 1;
        state.images.insert(id, StoredImage {
            data: graphic,
            number,
            generation: state.generation,
        });
        let display = command.character(b'a', b't')? == b'T';
        let anonymous = command.number(b'i', 0)? == 0 && number == 0;
        if !anonymous {
            command.keys.insert(b'i', id.to_string());
        }
        if display {
            place(term, state, &command, id)?;
        }
        Ok(())
    });
    command.reply(term, result);
}

fn decode(command: &Command, mut data: Vec<u8>) -> Result<GraphicData, &'static str> {
    match command.character(b'o', 0)? {
        0 => (),
        b'z' => {
            let mut decoded = Vec::new();
            ZlibDecoder::new(data.as_slice())
                .take(MAX_IMAGE_BYTES as u64 + 1)
                .read_to_end(&mut decoded)
                .map_err(|_| "EINVAL:Invalid compressed data")?;
            if decoded.len() > MAX_IMAGE_BYTES {
                return Err("E2BIG:Decompressed image exceeds limit");
            }
            data = decoded;
        },
        _ => return Err("ENOTSUP:Unsupported compression"),
    }
    let format = command.number(b'f', 32)?;
    if format == 100 {
        if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err("EINVAL:Expected PNG data");
        }
        return bitmap::decode(&data);
    }
    let channels = match format {
        24 => 3,
        32 => 4,
        _ => return Err("ENOTSUP:Unsupported pixel format"),
    };
    let width = command.number(b's', 0)? as usize;
    let height = command.number(b'v', 0)? as usize;
    if width == 0
        || height == 0
        || width > MAX_GRAPHIC_DIMENSIONS[0]
        || height > MAX_GRAPHIC_DIMENSIONS[1]
        || data.len() != width * height * channels
    {
        return Err("EINVAL:Invalid pixel dimensions or data size");
    }
    Ok(GraphicData {
        id: GraphicId(0),
        width,
        height,
        color_type: if channels == 3 { ColorType::Rgb } else { ColorType::Rgba },
        is_opaque: channels == 3 || data.chunks_exact(4).all(|p| p[3] == 255),
        pixels: data,
    })
}

fn read_medium(command: &Command, payload: &[u8]) -> Result<Vec<u8>, &'static str> {
    let name = BASE64.decode(payload).map_err(|_| "EBADF:Failed to read image file")?;
    let name = std::str::from_utf8(&name).map_err(|_| "EBADF:Failed to read image file")?;
    let medium = command.character(b't', b'd')?;
    let offset = command.number(b'O', 0)? as u64;
    let size = command.number(b'S', 0)? as usize;
    if size > MAX_IMAGE_BYTES {
        return Err("E2BIG:Image data exceeds limit");
    }
    if medium == b's' {
        return read_shared_memory(name, offset, size);
    }
    if !matches!(medium, b'f' | b't') {
        return Err("ENOTSUP:Unsupported transmission medium");
    }
    let path = Path::new(name);
    let result = (|| {
        let path = fs::canonicalize(path).ok()?;
        if ["/proc", "/sys", "/dev"].iter().any(|s| path.starts_with(s)) {
            return None;
        }
        if !fs::metadata(&path).ok()?.is_file() {
            return None;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let data = read_file(&mut file, offset, size).ok()?;
        if medium == b't' && safe_temporary_path(&path) {
            let _ = fs::remove_file(&path);
        }
        Some(data)
    })();
    result.ok_or("EBADF:Failed to read image file")
}

fn safe_temporary_path(path: &Path) -> bool {
    path.to_string_lossy().contains("tty-graphics-protocol")
        && [std::env::temp_dir(), PathBuf::from("/tmp"), PathBuf::from("/dev/shm")]
            .iter()
            .filter_map(|p| fs::canonicalize(p).ok())
            .any(|p| path.starts_with(p))
}

fn read_file(file: &mut File, offset: u64, size: usize) -> Result<Vec<u8>, std::io::Error> {
    file.seek(SeekFrom::Start(offset))?;
    let available =
        file.metadata()?.len().checked_sub(offset).ok_or(std::io::ErrorKind::UnexpectedEof)?;
    let length = if size == 0 { available } else { size as u64 };
    if length > available || length > MAX_IMAGE_BYTES as u64 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut data = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut data)?;
    if data.len() != length as usize {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(data)
}

#[cfg(unix)]
fn read_shared_memory(name: &str, offset: u64, size: usize) -> Result<Vec<u8>, &'static str> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    if !name.starts_with('/') || name[1..].contains('/') || name.len() < 2 || name.len() > 255 {
        return Err("EBADF:Failed to read image file");
    }
    let name = CString::new(name).map_err(|_| "EBADF:Failed to read image file")?;
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err("EBADF:Failed to read image file");
    }
    // The descriptor is now owned by File, including all error paths below.
    let file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        // Darwin shared memory descriptors require mmap rather than read/seek.
        let available = file.metadata().ok()?.len().checked_sub(offset)?;
        let length = if size == 0 { available } else { size as u64 };
        if length == 0 || length > available || length > MAX_IMAGE_BYTES as u64 {
            return None;
        }
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return None;
        }
        let aligned_offset = offset / page_size as u64 * page_size as u64;
        let prefix = (offset - aligned_offset) as usize;
        let map_length = prefix.checked_add(length as usize)?;
        let map_offset = libc::off_t::try_from(aligned_offset).ok()?;
        // The offset is page-aligned and the range fits the object's reported size.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                map_offset,
            )
        };
        if pointer == libc::MAP_FAILED {
            return None;
        }
        // Copy the bounded readable mapping before unmapping it.
        let data = unsafe {
            std::slice::from_raw_parts(pointer.cast::<u8>().add(prefix), length as usize).to_vec()
        };
        unsafe {
            libc::munmap(pointer, map_length);
        }
        Some(data)
    })()
    .ok_or("EBADF:Failed to read image file");
    unsafe {
        libc::shm_unlink(name.as_ptr());
    }
    result
}

#[cfg(not(unix))]
fn read_shared_memory(_: &str, _: u64, _: usize) -> Result<Vec<u8>, &'static str> {
    Err("ENOTSUP:Shared memory transport is unavailable")
}

fn place<L: EventListener>(
    term: &mut Term<L>,
    state: &mut State,
    command: &Command,
    id: u32,
) -> Result<(), &'static str> {
    if command.number(b'U', 0)? > 1
        || command.number(b'P', 0)? != 0
        || command.number(b'Q', 0)? != 0
    {
        return Err("ENOTSUP:Relative placements are unavailable");
    }
    let source = &state.images.get(&id).ok_or("ENOENT:Image not found")?.data;
    let x = command.number(b'x', 0)? as usize;
    let y = command.number(b'y', 0)? as usize;
    if x >= source.width || y >= source.height {
        return Err("EINVAL:Empty source rectangle");
    }
    let width = command.number(b'w', 0)? as usize;
    let height = command.number(b'h', 0)? as usize;
    let width = if width == 0 { source.width - x } else { width.min(source.width - x) };
    let height = if height == 0 { source.height - y } else { height.min(source.height - y) };
    let cropped =
        imageops::crop_imm(&bitmap::rgba(source), x as u32, y as u32, width as u32, height as u32)
            .to_image();
    let cell_width = (term.graphics.cell_width as usize).max(1);
    let cell_height = (term.graphics.cell_height as usize).max(1);
    let cols = command.number(b'c', 0)? as usize;
    let rows = command.number(b'r', 0)? as usize;
    let target_width = if cols == 0 {
        None
    } else {
        Some(cols.checked_mul(cell_width).ok_or("EINVAL:Display too wide")?)
    };
    let target_height = if rows == 0 {
        None
    } else {
        Some(rows.checked_mul(cell_height).ok_or("EINVAL:Display too tall")?)
    };
    let graphic = bitmap::resize(bitmap::from_rgba(cropped), target_width, target_height, true)?;
    let offset_x = command.number(b'X', 0)? as usize;
    let offset_y = command.number(b'Y', 0)? as usize;
    if offset_x >= cell_width || offset_y >= cell_height {
        return Err("EINVAL:Invalid cell offset");
    }
    let box_width = target_width.unwrap_or(graphic.width).max(graphic.width);
    let box_height = target_height.unwrap_or(graphic.height).max(graphic.height);
    let width = box_width.checked_add(offset_x).ok_or("EINVAL:Display too wide")?;
    let height = box_height.checked_add(offset_y).ok_or("EINVAL:Display too tall")?;
    if width > MAX_GRAPHIC_DIMENSIONS[0] || height > MAX_GRAPHIC_DIMENSIONS[1] {
        return Err("EINVAL:Display too large");
    }
    let mut canvas = RgbaImage::new(width as u32, height as u32);
    imageops::overlay(
        &mut canvas,
        &bitmap::rgba(&graphic),
        (offset_x + (box_width - graphic.width) / 2) as i64,
        (offset_y + (box_height - graphic.height) / 2) as i64,
    );
    let placement_id = command.number(b'p', 0)?;
    let z_index = command.z_index()?;
    let no_cursor = command.number(b'C', 0)?;
    if no_cursor > 1 {
        return Err("EINVAL:Invalid cursor policy");
    }
    if command.number(b'U', 0)? == 1 {
        if cols == 0 || rows == 0 {
            return Err("EINVAL:Virtual placement requires rows and columns");
        }
        let mut graphic = bitmap::from_rgba(canvas);
        let texture = Arc::new(TextureRef {
            id: term.graphics.next_id(),
            width: width as u16,
            height: height as u16,
            cell_height,
            texture_operations: Arc::downgrade(&term.graphics.texture_operations),
        });
        graphic.id = texture.id;
        state.virtual_placements.remove(&(id, placement_id));
        state.recount_virtual_bytes();
        while state.virtual_placements.len() >= MAX_STORED_IMAGES
            || state.virtual_bytes + graphic.pixels.len() > STORAGE_QUOTA
        {
            let key = *state.virtual_placements.keys().next().unwrap();
            state.virtual_placements.remove(&key);
            state.recount_virtual_bytes();
        }
        state.virtual_placements.insert((id, placement_id), VirtualPlacement {
            texture,
            columns: cols,
            rows,
            z_index,
        });
        state.recount_virtual_bytes();
        term.graphics.queue_upload(graphic);
        term.mark_fully_damaged();
        return Ok(());
    }
    if placement_id != 0 {
        remove_placements(
            term,
            state,
            |_, p| p.image_id == id && p.placement_id == placement_id,
            true,
        );
    }
    let options = PlacementOptions { no_cursor_movement: no_cursor == 1, z_index, image_id: id };
    let bytes = width * height * 4;
    while state.placements.len() >= MAX_STORED_IMAGES
        || state.placements.values().map(|p| p.bytes).sum::<usize>() + bytes > STORAGE_QUOTA
    {
        let victim = *state.placements.keys().min().ok_or("ENOSPC:Placement storage full")?;
        remove_placements(term, state, |id, _| id == victim, true);
    }
    let texture =
        super::insert_graphic_with_options(term, bitmap::from_rgba(canvas), None, Some(options))
            .ok_or("EINVAL:Could not place image")?;
    state.placements.insert(texture.id, Placement {
        texture: Arc::downgrade(&texture),
        bytes,
        image_id: id,
        placement_id,
        z_index,
    });
    Ok(())
}

/// Remove whole placements selected by their texture id, preserving unrelated cell data.
fn remove_placements<L: EventListener>(
    term: &mut Term<L>,
    state: &mut State,
    predicate: impl Fn(GraphicId, &Placement) -> bool,
    include_history: bool,
) {
    let ids: HashSet<_> =
        state.placements.iter().filter(|(id, p)| predicate(**id, p)).map(|(&id, _)| id).collect();
    if ids.is_empty() {
        return;
    }
    let start = if include_history { -(term.grid().history_size() as i32) } else { 0 };
    for line in start..term.screen_lines() as i32 {
        for col in 0..term.columns() {
            let cell = &mut term.grid_mut()[Line(line)][Column(col)];
            if let Some(mut graphics) = cell.take_graphics() {
                graphics.retain(|g| !ids.contains(&g.texture.id));
                if !graphics.is_empty() {
                    cell.set_graphics(graphics);
                }
            }
        }
    }
    // Leave placement metadata if its cells are still in scrollback.
    let mut retained = HashSet::new();
    for line in -(term.grid().history_size() as i32)..term.screen_lines() as i32 {
        for col in 0..term.columns() {
            retained.extend(
                term.grid()[Line(line)][Column(col)]
                    .graphics()
                    .into_iter()
                    .flatten()
                    .map(|g| g.texture.id),
            );
        }
    }
    state.placements.retain(|id, _| !ids.contains(id) || retained.contains(id));
    term.mark_fully_damaged();
}

fn delete<L: EventListener>(
    term: &mut Term<L>,
    state: &mut State,
    command: &Command,
) -> Result<(), &'static str> {
    let selector = command.character(b'd', b'a')?;
    let free = selector.is_ascii_uppercase();
    let kind = selector.to_ascii_lowercase();
    if !matches!(kind, b'a' | b'i' | b'n' | b'c' | b'p' | b'q' | b'r' | b'x' | b'y' | b'z') {
        return Err("EINVAL:Unsupported deletion selector");
    }
    let id = if matches!(kind, b'i' | b'n') { state.resolve_id(command)? } else { 0 };
    let placement = command.number(b'p', 0)?;
    let x = command.number(b'x', 0)?;
    let y = command.number(b'y', 0)?;
    let z = command.z_index()?;
    let cursor = term.grid().cursor.point;
    let mut visible = HashSet::new();
    for line in 0..term.screen_lines() {
        for col in 0..term.columns() {
            let cell = &term.grid()[Line(line as i32)][Column(col)];
            let intersection = match kind {
                b'c' => line as i32 == cursor.line.0 && col == cursor.column.0,
                b'p' | b'q' => col as u32 + 1 == x && line as u32 + 1 == y,
                b'x' => col as u32 + 1 == x,
                b'y' => line as u32 + 1 == y,
                _ => true,
            };
            if intersection {
                visible.extend(cell.graphics().into_iter().flatten().map(|g| g.texture.id));
            }
        }
    }
    let mut affected = HashSet::new();
    let selected: HashSet<_> = state
        .placements
        .iter()
        .filter(|(texture, p)| {
            let select = match kind {
                b'i' | b'n' => p.image_id == id && (placement == 0 || p.placement_id == placement),
                b'r' => p.image_id >= x && p.image_id <= y,
                b'z' => p.z_index == z && visible.contains(texture),
                b'q' => p.z_index == z && visible.contains(texture),
                _ => visible.contains(texture),
            };
            if select {
                affected.insert(p.image_id);
            }
            select
        })
        .map(|(&id, _)| id)
        .collect();
    if matches!(kind, b'i' | b'n') {
        affected.insert(id);
    }
    if kind == b'r' {
        affected.extend(state.images.keys().copied().filter(|id| *id >= x && *id <= y));
    }
    remove_placements(
        term,
        state,
        |id, _| selected.contains(&id),
        matches!(kind, b'i' | b'n' | b'r'),
    );
    if matches!(kind, b'i' | b'n' | b'r') {
        state.virtual_placements.retain(|(image, p), _| {
            if kind == b'r' {
                !(*image >= x && *image <= y)
            } else {
                !(*image == id && (placement == 0 || *p == placement))
            }
        });
    }
    state.recount_virtual_bytes();
    if free {
        for id in affected {
            if !state.placements.values().any(|p| p.image_id == id)
                && !state.virtual_placements.keys().any(|(i, _)| *i == id)
            {
                state.remove_data(id);
            }
        }
    }
    Ok(())
}

impl State {
    pub(super) fn placeholder(
        &self,
        cell: &Cell,
        previous: Option<&Placeholder>,
    ) -> Option<Placeholder> {
        if cell.c != '\u{10eeee}' {
            return None;
        }
        let low = color_id(cell.fg)?;
        let placement_id = cell.underline_color().and_then(color_id).unwrap_or(0);
        let marks = cell.zerowidth().unwrap_or_default();
        let mark = |i| marks.get(i).and_then(|c| DIACRITICS.iter().position(|d| d == c));
        let compatible =
            previous.filter(|p| p.foreground == cell.fg && p.underline == cell.underline_color());
        let row = mark(0).or_else(|| compatible.map(|p| p.row)).unwrap_or(0);
        let compatible = compatible.filter(|p| p.row == row);
        let column = mark(1).or_else(|| compatible.map(|p| p.column + 1)).unwrap_or(0);
        let high = mark(2)
            .map(|v| v as u32)
            .or_else(|| compatible.filter(|p| p.column + 1 == column).map(|p| p.high))
            .unwrap_or(0);
        if high > 255 {
            return None;
        }
        let image_id = low | (high << 24);
        let placement = if placement_id == 0 {
            self.virtual_placements
                .range((image_id, 0)..=(image_id, u32::MAX))
                .next()
                .map(|(_, p)| p)?
        } else {
            self.virtual_placements.get(&(image_id, placement_id))?
        };
        if row >= placement.rows || column >= placement.columns {
            return None;
        }
        Some(Placeholder {
            texture_id: placement.texture.id,
            offset_x: (column * placement.texture.width as usize / placement.columns) as u16,
            offset_y: (row * placement.texture.height as usize / placement.rows) as u16,
            z_index: placement.z_index,
            image_id,
            row,
            column,
            high,
            foreground: cell.fg,
            underline: cell.underline_color(),
        })
    }
}

fn color_id(color: Color) -> Option<u32> {
    match color {
        Color::Indexed(i) => Some(i as u32),
        Color::Spec(rgb) => Some(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32),
        _ => None,
    }
}

// Unicode combining marks from Kitty's protocol reference, in protocol order.
const DIACRITICS: &[char] = &[
    '\u{0305}',
    '\u{030d}',
    '\u{030e}',
    '\u{0310}',
    '\u{0312}',
    '\u{033d}',
    '\u{033e}',
    '\u{033f}',
    '\u{0346}',
    '\u{034a}',
    '\u{034b}',
    '\u{034c}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035b}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036a}',
    '\u{036b}',
    '\u{036c}',
    '\u{036d}',
    '\u{036e}',
    '\u{036f}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059c}',
    '\u{059d}',
    '\u{059e}',
    '\u{059f}',
    '\u{05a0}',
    '\u{05a1}',
    '\u{05a8}',
    '\u{05a9}',
    '\u{05ab}',
    '\u{05ac}',
    '\u{05af}',
    '\u{05c4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065a}',
    '\u{065b}',
    '\u{065d}',
    '\u{065e}',
    '\u{06d6}',
    '\u{06d7}',
    '\u{06d8}',
    '\u{06d9}',
    '\u{06da}',
    '\u{06db}',
    '\u{06dc}',
    '\u{06df}',
    '\u{06e0}',
    '\u{06e1}',
    '\u{06e2}',
    '\u{06e4}',
    '\u{06e7}',
    '\u{06e8}',
    '\u{06eb}',
    '\u{06ec}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073a}',
    '\u{073d}',
    '\u{073f}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074a}',
    '\u{07eb}',
    '\u{07ec}',
    '\u{07ed}',
    '\u{07ee}',
    '\u{07ef}',
    '\u{07f0}',
    '\u{07f1}',
    '\u{07f3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081b}',
    '\u{081c}',
    '\u{081d}',
    '\u{081e}',
    '\u{081f}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082a}',
    '\u{082b}',
    '\u{082c}',
    '\u{082d}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0f82}',
    '\u{0f83}',
    '\u{0f86}',
    '\u{0f87}',
    '\u{135d}',
    '\u{135e}',
    '\u{135f}',
    '\u{17dd}',
    '\u{193a}',
    '\u{1a17}',
    '\u{1a75}',
    '\u{1a76}',
    '\u{1a77}',
    '\u{1a78}',
    '\u{1a79}',
    '\u{1a7a}',
    '\u{1a7b}',
    '\u{1a7c}',
    '\u{1b6b}',
    '\u{1b6d}',
    '\u{1b6e}',
    '\u{1b6f}',
    '\u{1b70}',
    '\u{1b71}',
    '\u{1b72}',
    '\u{1b73}',
    '\u{1cd0}',
    '\u{1cd1}',
    '\u{1cd2}',
    '\u{1cda}',
    '\u{1cdb}',
    '\u{1ce0}',
    '\u{1dc0}',
    '\u{1dc1}',
    '\u{1dc3}',
    '\u{1dc4}',
    '\u{1dc5}',
    '\u{1dc6}',
    '\u{1dc7}',
    '\u{1dc8}',
    '\u{1dc9}',
    '\u{1dcb}',
    '\u{1dcc}',
    '\u{1dd1}',
    '\u{1dd2}',
    '\u{1dd3}',
    '\u{1dd4}',
    '\u{1dd5}',
    '\u{1dd6}',
    '\u{1dd7}',
    '\u{1dd8}',
    '\u{1dd9}',
    '\u{1dda}',
    '\u{1ddb}',
    '\u{1ddc}',
    '\u{1ddd}',
    '\u{1dde}',
    '\u{1ddf}',
    '\u{1de0}',
    '\u{1de1}',
    '\u{1de2}',
    '\u{1de3}',
    '\u{1de4}',
    '\u{1de5}',
    '\u{1de6}',
    '\u{1dfe}',
    '\u{20d0}',
    '\u{20d1}',
    '\u{20d4}',
    '\u{20d5}',
    '\u{20d6}',
    '\u{20d7}',
    '\u{20db}',
    '\u{20dc}',
    '\u{20e1}',
    '\u{20e7}',
    '\u{20e9}',
    '\u{20f0}',
    '\u{2cef}',
    '\u{2cf0}',
    '\u{2cf1}',
    '\u{2de0}',
    '\u{2de1}',
    '\u{2de2}',
    '\u{2de3}',
    '\u{2de4}',
    '\u{2de5}',
    '\u{2de6}',
    '\u{2de7}',
    '\u{2de8}',
    '\u{2de9}',
    '\u{2dea}',
    '\u{2deb}',
    '\u{2dec}',
    '\u{2ded}',
    '\u{2dee}',
    '\u{2def}',
    '\u{2df0}',
    '\u{2df1}',
    '\u{2df2}',
    '\u{2df3}',
    '\u{2df4}',
    '\u{2df5}',
    '\u{2df6}',
    '\u{2df7}',
    '\u{2df8}',
    '\u{2df9}',
    '\u{2dfa}',
    '\u{2dfb}',
    '\u{2dfc}',
    '\u{2dfd}',
    '\u{2dfe}',
    '\u{2dff}',
    '\u{a66f}',
    '\u{a67c}',
    '\u{a67d}',
    '\u{a6f0}',
    '\u{a6f1}',
    '\u{a8e0}',
    '\u{a8e1}',
    '\u{a8e2}',
    '\u{a8e3}',
    '\u{a8e4}',
    '\u{a8e5}',
    '\u{a8e6}',
    '\u{a8e7}',
    '\u{a8e8}',
    '\u{a8e9}',
    '\u{a8ea}',
    '\u{a8eb}',
    '\u{a8ec}',
    '\u{a8ed}',
    '\u{a8ee}',
    '\u{a8ef}',
    '\u{a8f0}',
    '\u{a8f1}',
    '\u{aab0}',
    '\u{aab2}',
    '\u{aab3}',
    '\u{aab7}',
    '\u{aab8}',
    '\u{aabe}',
    '\u{aabf}',
    '\u{aac1}',
    '\u{fe20}',
    '\u{fe21}',
    '\u{fe22}',
    '\u{fe23}',
    '\u{fe24}',
    '\u{fe25}',
    '\u{fe26}',
    '\u{10a0f}',
    '\u{10a38}',
    '\u{1d185}',
    '\u{1d186}',
    '\u{1d187}',
    '\u{1d188}',
    '\u{1d189}',
    '\u{1d1aa}',
    '\u{1d1ab}',
    '\u{1d1ac}',
    '\u{1d1ad}',
    '\u{1d242}',
    '\u{1d243}',
    '\u{1d244}',
];
