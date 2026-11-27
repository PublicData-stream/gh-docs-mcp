//! Fixed binary protocol for the confined regex process.

use std::io::{self, Read, Write};

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;

pub(crate) const BLOB_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const PATTERN_BYTES: usize = 4096;
pub(crate) const MATCH_LIMIT: usize = 101;
pub(crate) const RECORD_FIELDS: usize = 5;

pub fn run(force_failure: bool) -> io::Result<()> {
    let mut output = io::stdout().lock();
    if docs_mcp_git::sandbox::confine_framed_search(force_failure).is_err() {
        return output.write_all(b"-").and_then(|()| output.flush());
    }
    output.write_all(b"+")?;
    output.flush()?;

    let mut input = io::stdin().lock();
    let mut mode = [0];
    input.read_exact(&mut mode)?;
    let pattern = bytes(&mut input, PATTERN_BYTES)?;
    let pattern = std::str::from_utf8(&pattern).map_err(io::Error::other)?;
    let compiled = RegexMatcherBuilder::new()
        .line_terminator(Some(b'\n'))
        .fixed_strings(mode[0] & 1 != 0)
        .case_insensitive(mode[0] & 2 != 0)
        .size_limit(1024 * 1024)
        .dfa_size_limit(1024 * 1024)
        .nest_limit(64)
        .build(pattern);
    let Ok(regex) = compiled else {
        return output.write_all(b"-").and_then(|()| output.flush());
    };
    output.write_all(b"+")?;
    output.flush()?;

    loop {
        let mut opcode = [0];
        match input.read(&mut opcode)? {
            0 => return Ok(()),
            1 if opcode == [1] => (),
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
        let after = word(&mut input)?;
        let blob = bytes(&mut input, BLOB_BYTES)?;
        if after > blob.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut matches = Vec::new();
        let mut start = 0;
        for (line_number, line) in blob.split_inclusive(|&b| b == b'\n').enumerate() {
            let end = start + line.len();
            let text = line.strip_suffix(b"\n").unwrap_or(line);
            if start + text.len() >= after {
                regex
                    .find_iter(text, |hit| {
                        if start + hit.start() >= after {
                            matches.push([
                                line_number + 1,
                                start,
                                end,
                                start + hit.start(),
                                start + hit.end(),
                            ]);
                        }
                        matches.len() < MATCH_LIMIT
                    })
                    .map_err(io::Error::other)?;
            }
            if matches.len() == MATCH_LIMIT {
                break;
            }
            start = end;
        }
        output.write_all(&(matches.len() as u32).to_be_bytes())?;
        for item in matches {
            for field in item {
                output.write_all(&(field as u32).to_be_bytes())?;
            }
        }
        output.flush()?;
    }
}

fn word(input: &mut impl Read) -> io::Result<usize> {
    let mut bytes = [0; 4];
    input.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes) as usize)
}

fn bytes(input: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let len = word(input)?;
    if len > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut value = vec![0; len];
    input.read_exact(&mut value)?;
    Ok(value)
}
