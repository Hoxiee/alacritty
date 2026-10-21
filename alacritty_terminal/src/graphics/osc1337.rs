//! iTerm2 inline images, including multipart transfers.

use std::collections::HashMap;
use std::str;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;

use super::image::{self, MAX_ENCODED_BYTES};
use super::{GraphicData, Graphics};

#[derive(Debug)]
pub(crate) struct Transfer {
    options: HashMap<String, String>,
    encoded: Vec<u8>,
}

impl Graphics {
    pub(crate) fn osc1337(
        &mut self,
        params: &[&[u8]],
        columns: usize,
        lines: usize,
    ) -> Option<GraphicData> {
        if params.first().copied() != Some(b"1337") {
            return None;
        }
        let command = params.get(1)?;
        if let Some(part) = command.strip_prefix(b"FilePart=") {
            let transfer = self.iterm_transfer.as_mut()?;
            if transfer.encoded.len().saturating_add(part.len()) > MAX_ENCODED_BYTES {
                self.iterm_transfer = None;
            } else {
                transfer.encoded.extend_from_slice(part);
            }
            return None;
        }
        let transfer = if *command == b"FileEnd" {
            self.iterm_transfer.take()?
        } else {
            self.iterm_transfer = None;
            let multipart = command.starts_with(b"MultipartFile=");
            let prefix = if multipart { b"MultipartFile=".as_slice() } else { b"File=".as_slice() };
            let first = command.strip_prefix(prefix)?;
            let mut options = HashMap::new();
            let mut encoded = Vec::new();
            for (index, param) in
                std::iter::once(first).chain(params.iter().skip(2).copied()).enumerate()
            {
                let (option, data) = match param.iter().position(|&b| b == b':') {
                    Some(i) => (&param[..i], Some(&param[i + 1..])),
                    None => (param, None),
                };
                if let Some(data) = data {
                    if index + 2 != params.len() || data.len() > MAX_ENCODED_BYTES {
                        return None;
                    }
                    encoded.extend_from_slice(data);
                }
                let option = str::from_utf8(option).ok()?;
                let (key, value) = option.split_once('=')?;
                options.insert(key.to_owned(), value.to_owned());
            }
            if options.get("inline").map(String::as_str) != Some("1") {
                return None;
            }
            let transfer = Transfer { options, encoded };
            if multipart {
                self.iterm_transfer = Some(transfer);
                return None;
            }
            transfer
        };
        let data = Base64.decode(&transfer.encoded).ok()?;
        let graphic = image::decode(&data).ok()?;
        let dimension = |key: &str, cell: f32, count: usize| -> Option<Option<usize>> {
            let value = transfer.options.get(key).map(String::as_str).unwrap_or("auto");
            if value == "auto" {
                return Some(None);
            }
            let pixels = if let Some(n) = value.strip_suffix("px") {
                n.parse::<usize>().ok()?
            } else if let Some(n) = value.strip_suffix('%') {
                n.parse::<usize>().ok()?.checked_mul(count)?.checked_mul(cell.max(1.) as usize)?
                    / 100
            } else {
                value.parse::<usize>().ok()?.checked_mul(cell.max(1.) as usize)?
            };
            Some(Some(pixels))
        };
        let width = dimension("width", self.cell_width, columns)?;
        let height = dimension("height", self.cell_height, lines)?;
        image::resize(
            graphic,
            width,
            height,
            transfer.options.get("preserveAspectRatio").map(String::as_str) != Some("0"),
        )
        .ok()
    }
}
