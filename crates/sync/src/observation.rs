//! Bounded observation-only Git Smart HTTP discovery. No local Git storage.

use std::{collections::BTreeMap, time::Duration};

use docs_mcp_core::{
    Result,
    git::{ObjectFormat, Oid, RefName},
};
use reqwest::{Client, RequestBuilder, header};
use sha2::{Digest, Sha256};

use crate::{Fixture, approval::MAX_REFS, upstream, worker::packet_size};

pub use crate::approval::Approval;

const MAX_RESPONSE: usize = 4 * 1024 * 1024;

async fn receive(
    request: RequestBuilder,
    content_type: &'static str,
    remaining: &mut usize,
) -> Result<Vec<u8>> {
    let mut response = request
        .header(header::ACCEPT, content_type)
        .header(header::ACCEPT_ENCODING, "identity")
        .header("Git-Protocol", "version=2")
        .send()
        .await
        .map_err(|_| "Git observation transport failed")?;
    if response.status() != reqwest::StatusCode::OK
        || response.headers().contains_key(header::CONTENT_ENCODING)
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some(content_type)
    {
        return Err("Git observation response denied".into());
    }
    let mut output = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Git observation body unavailable")?
    {
        *remaining = remaining
            .checked_sub(chunk.len())
            .ok_or("Git observation exceeds budget")?;
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn packets(mut bytes: &[u8], max: usize) -> Result<Vec<&[u8]>> {
    let mut packets = Vec::new();
    while let Some(header) = bytes.get(..4) {
        bytes = &bytes[4..];
        match packet_size(header)? {
            0 if bytes.is_empty() => return Ok(packets),
            size @ 5..=65520 if packets.len() < max && size - 4 <= bytes.len() => {
                let (packet, tail) = bytes.split_at(size - 4);
                packets.push(packet.strip_suffix(b"\n").unwrap_or(packet));
                bytes = tail;
            }
            _ => break,
        }
    }
    Err("invalid Git observation framing or packet count".into())
}

fn request(format: ObjectFormat) -> Vec<u8> {
    let mut body = Vec::new();
    let object_format = format!("object-format={}\n", format.name());
    for line in [
        "command=ls-refs\n",
        &object_format,
        "0001",
        "symrefs\n",
        "ref-prefix HEAD\n",
        "ref-prefix refs/heads/\n",
        "ref-prefix refs/tags/\n",
        "0000",
    ] {
        if line.starts_with("000") {
            body.extend_from_slice(line.as_bytes());
        } else {
            body.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
        }
    }
    body
}

fn digest(bytes: &[u8], approval: &Approval, format: ObjectFormat) -> Result<String> {
    let mut head = None;
    let mut refs = BTreeMap::new();
    for line in packets(bytes, MAX_REFS * 2)? {
        let mut fields = line.split(|byte| *byte == b' ');
        let oid = Oid::parse(format, fields.next().ok_or("missing observed object ID")?)?;
        let name = fields.next().ok_or("missing observed ref")?;
        if name == b"HEAD" {
            let target = fields
                .next()
                .and_then(|field| field.strip_prefix(b"symref-target:"))
                .ok_or("invalid observed HEAD")?;
            if head
                .replace((RefName::new(target.to_vec())?, oid))
                .is_some()
            {
                return Err("duplicate observed HEAD".into());
            }
        } else {
            let name = RefName::new(name.to_vec())?;
            if approval.allows(&name) && refs.insert(name, oid).is_some() {
                return Err("duplicate approved observation".into());
            }
        }
    }
    let (default, oid) = head.ok_or("missing observed HEAD")?;
    if !default.bytes().starts_with(b"refs/heads/")
        || refs.get(&default) != Some(&oid)
        || refs.len() > MAX_REFS
    {
        return Err("unapproved observed HEAD or too many refs".into());
    }
    for required in &approval.required_refs {
        if !refs.contains_key(&RefName::new(required.bytes().to_vec())?) {
            return Err("required observed ref is absent".into());
        }
    }
    let mut digest = Sha256::new();
    digest.update(b"gh-docs-feed-approved-refs-v1\0");
    digest.update(approval.version.to_be_bytes());
    digest.update(default.bytes());
    digest.update(b"\0");
    for (name, oid) in refs {
        digest.update(name.bytes());
        digest.update(b"\0");
        digest.update(oid.to_string().as_bytes());
        digest.update(b"\0");
    }
    Ok(crate::hex(&digest.finalize()))
}

async fn discover(
    client: &Client,
    base: &str,
    approval: &Approval,
    format: ObjectFormat,
) -> Result<String> {
    let mut remaining = MAX_RESPONSE;
    let capabilities = receive(
        client.get(format!("{base}/info/refs?service=git-upload-pack")),
        "application/x-git-upload-pack-advertisement",
        &mut remaining,
    )
    .await?;
    let capabilities = packets(&capabilities, 64)?;
    let mut formats = capabilities
        .iter()
        .filter_map(|line| line.strip_prefix(b"object-format="));
    if capabilities.first() != Some(&b"version 2".as_slice())
        || !capabilities
            .iter()
            .any(|line| *line == b"ls-refs" || line.starts_with(b"ls-refs="))
        || formats.next().unwrap_or(b"sha1") != format.name().as_bytes()
        || formats.next().is_some()
    {
        return Err("unsupported Git observation capabilities".into());
    }
    // Retain the one-second separation between Git requests made by this feed.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let refs = receive(
        client
            .post(format!("{base}/git-upload-pack"))
            .header(
                header::CONTENT_TYPE,
                "application/x-git-upload-pack-request",
            )
            .body(request(format)),
        "application/x-git-upload-pack-result",
        &mut remaining,
    )
    .await?;
    digest(&refs, approval, format)
}

pub async fn poll(approval: &Approval, fixture: Option<&Fixture>) -> Result<String> {
    let (_, _, format) = approval.validate()?;
    tokio::time::timeout(Duration::from_secs(20), async {
        let (client, base) = upstream::client(approval, fixture)
            .await
            .map_err(|error| error.reason)?;
        discover(&client, &base, approval, format).await
    })
    .await?
}
