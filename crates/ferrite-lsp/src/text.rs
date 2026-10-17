//! Conversions between byte offsets and LSP positions.

/// How the server counts columns, negotiated at initialisation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

/// Byte offset of the start of each line.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut v = vec![0];
    v.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    v
}

/// Byte offset -> (line, column) in the given encoding.
pub fn offset_to_position(text: &str, offset: usize, enc: Encoding) -> (u32, u32) {
    let offset = clamp_to_char(text, offset);
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line = text[..line_start].matches('\n').count() as u32;
    let segment = &text[line_start..offset];
    let col = match enc {
        Encoding::Utf8 => segment.len(),
        Encoding::Utf16 => segment.encode_utf16().count(),
    };
    (line, col as u32)
}

/// (line, column) in the given encoding -> byte offset, clamped to the text.
pub fn position_to_offset(text: &str, line: u32, col: u32, enc: Encoding) -> usize {
    let starts = line_starts(text);
    let Some(&start) = starts.get(line as usize) else { return text.len() };
    let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
    let line_text = &text[start..end];
    match enc {
        Encoding::Utf8 => start + clamp_to_char(line_text, (col as usize).min(line_text.len())),
        Encoding::Utf16 => {
            let mut units = 0u32;
            for (i, ch) in line_text.char_indices() {
                if units >= col {
                    return start + i;
                }
                units += ch.len_utf16() as u32;
            }
            end
        }
    }
}

fn clamp_to_char(text: &str, mut offset: usize) -> usize {
    offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_in_both_encodings() {
        let text = "fn main() {\n    let s = \"é😀x\";\n}\n";
        let x = text.find('x').unwrap();
        assert_eq!(offset_to_position(text, x, Encoding::Utf8), (1, 19));
        assert_eq!(offset_to_position(text, x, Encoding::Utf16), (1, 16));
        assert_eq!(position_to_offset(text, 1, 19, Encoding::Utf8), x);
        assert_eq!(position_to_offset(text, 1, 16, Encoding::Utf16), x);
        assert_eq!(position_to_offset(text, 9, 0, Encoding::Utf8), text.len());
    }
}
