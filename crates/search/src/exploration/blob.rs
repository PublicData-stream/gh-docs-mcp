use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use super::{
    Error, RESPONSE_BYTES,
    cursor::{Pagination, Position, Scope},
    page::{coverage, json_size},
    request::{ByteRange, Encoding},
};

const CHUNK: usize = 32 * 1024;

pub(super) fn page(
    mut base: Value,
    raw: &[u8],
    range: Option<ByteRange>,
    encoding: Encoding,
    pagination: &Pagination,
    scope: Scope,
) -> Result<Value, Error> {
    let size = raw.len();
    let ByteRange { start, end } = range.unwrap_or(ByteRange {
        start: 0,
        end: size as u64,
    });
    let range =
        gh_docs_core::git::checked_range(start, end, size).map_err(|_| Error::InvalidArgument)?;
    let first =
        usize::try_from(pagination.offset()?.unwrap_or(start)).map_err(|_| Error::CursorInvalid)?;
    if first < range.start || first > range.end {
        return Err(Error::CursorInvalid);
    }
    let text = if encoding == Encoding::Utf8 {
        if inside_code_point(raw, range.start) || inside_code_point(raw, range.end) {
            return Err(Error::InvalidBoundary);
        }
        Some(std::str::from_utf8(&raw[range.clone()]).map_err(|_| Error::UnsupportedEncoding)?)
    } else {
        None
    };
    // Six encoded JSON bytes per raw byte is the worst case for text.
    // Use the actual envelope size since paths and notices are variable.
    let remaining = RESPONSE_BYTES.saturating_sub(json_size(&base)? + 2048);
    let max_raw = match encoding {
        Encoding::Utf8 => remaining / 6,
        Encoding::Base64 => remaining / 4 * 3,
    }
    .min(CHUNK);
    let mut last = range.end.min(first + max_raw);
    if let Some(text) = text {
        if !text.is_char_boundary(first - range.start) {
            return Err(Error::CursorInvalid);
        }
        last = range.start + text.floor_char_boundary(last - range.start);
    }
    if first == last && first != range.end {
        return Err(Error::Budget);
    }
    let data = match text {
        Some(text) => text[first - range.start..last - range.start].to_owned(),
        None => STANDARD.encode(&raw[first..last]),
    };
    let complete = last == range.end;
    let next = if complete {
        None
    } else {
        Some(pagination.encode(Some(scope), Position::Offset(last as u64))?)
    };
    base["encoding"] = json!(encoding);
    base["data"] = json!(data);
    base["total_bytes"] = json!(size);
    base["requested_range"] = json!({"start":range.start, "end":range.end});
    base["returned_range"] = json!({"start":first, "end":last});
    base["coverage"] = coverage(complete, false, 1, Some(last - first));
    base["next_cursor"] = json!(next);
    Ok(base)
}

fn inside_code_point(bytes: &[u8], offset: usize) -> bool {
    if !bytes.get(offset).is_some_and(|byte| byte & 0xc0 == 0x80) {
        return false;
    }
    for distance in 1..=offset.min(3) {
        let start = offset - distance;
        let length = match bytes[start] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            0x80..=0xbf => continue,
            _ => return false,
        };
        return distance < length
            && bytes
                .get(start..start + length)
                .is_some_and(|slice| std::str::from_utf8(slice).is_ok());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_split_characters_without_misclassifying_invalid_bytes() {
        let bytes = "a€😀b".as_bytes();
        for offset in [2, 3, 5, 6, 7] {
            assert!(inside_code_point(bytes, offset));
        }
        for offset in [0, 1, 4, 8, 9] {
            assert!(!inside_code_point(bytes, offset));
        }
        assert!(!inside_code_point(b"x\x80x", 1));
        assert!(!inside_code_point(b"x\xe0\x80x", 2));
    }
}
