//! Bounded acquisition of one exact hosted docs.rs Rustdoc JSON artifact.

use std::{io::Read, time::Duration};

use docs_mcp_core::{
    Result,
    markdown::{Input, JSON_BYTES, digest},
};
use flate2::bufread::GzDecoder;
use reqwest::{StatusCode, header};
use tokio_postgres::Client;

use crate::upstream::{TransferError, fixed_client};

use super::{
    convert,
    jobs::{self, Job, Outcome},
};

const COMPRESSED_BYTES: usize = 32 * 1024 * 1024;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(90);

enum Response {
    Unchanged,
    Gzip(Vec<u8>, Option<String>),
}

pub async fn process(
    client: &mut Client,
    job: &Job,
    fixture: Option<&crate::upstream::Fixture>,
) -> Result<()> {
    let Input::DocsrsRustdoc {
        package,
        version,
        target,
        format,
    } = &job.approval.input
    else {
        return Err("hosted worker received a Git job".into());
    };
    let url = format!("https://docs.rs/crate/{package}/{version}/{target}/json/{format}.gz");
    let response = tokio::time::timeout(
        TRANSFER_TIMEOUT,
        download(&url, job.etag.as_deref(), fixture),
    )
    .await
    .unwrap_or_else(|_| {
        Err(TransferError::transient(
            "docsrs_timeout",
            Duration::from_secs(60),
        ))
    });
    let response = match response {
        Ok(Response::Gzip(compressed, etag)) => {
            let target = target.clone();
            let grants = job.approval.validate()?;
            match tokio::task::spawn_blocking(move || -> Result<_> {
                let raw = decompress(&compressed)?;
                let artifact = digest(&raw);
                let docs = convert::project(&raw, &target, &grants, |span| Some(span.to_owned()))?;
                Ok((artifact, docs))
            })
            .await?
            {
                Ok((artifact, docs)) => {
                    jobs::finish(
                        client,
                        job,
                        Outcome::Publish {
                            artifact: &artifact,
                            docs: &docs,
                            etag: etag.as_deref(),
                        },
                    )
                    .await?;
                    return Ok(());
                }
                Err(_) => Outcome::Failure {
                    reason: "invalid_docsrs_json",
                    retry: None,
                },
            }
        }
        Ok(Response::Unchanged) => Outcome::Unchanged,
        Err(error) => Outcome::Failure {
            reason: error.reason,
            retry: error.retry,
        },
    };
    jobs::finish(client, job, response).await?;
    Ok(())
}

async fn download(
    url: &str,
    etag: Option<&str>,
    fixture: Option<&crate::upstream::Fixture>,
) -> std::result::Result<Response, TransferError> {
    let (client, url) = fixed_client("docs.rs", url, fixture).await?;
    let mut request = client.get(url).header(header::ACCEPT_ENCODING, "identity");
    if let Some(etag) = etag {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    let transient =
        || TransferError::transient("docsrs_transfer_unavailable", Duration::from_secs(60));
    let mut response = request.send().await.map_err(|_| transient())?;
    let status = response.status();
    if status == StatusCode::NOT_MODIFIED && etag.is_some() {
        return Ok(Response::Unchanged);
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60);
        return Err(TransferError::transient(
            "docsrs_upstream_unavailable",
            Duration::from_secs(retry),
        ));
    }
    if status == StatusCode::NOT_FOUND {
        return Err(TransferError::permanent("docsrs_json_unavailable"));
    }
    if status != StatusCode::OK || response.headers().contains_key(header::CONTENT_ENCODING) {
        return Err(TransferError::permanent("docsrs_response_denied"));
    }
    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .filter(|tag| {
            tag.len() <= 256
                && (tag.starts_with('"') || tag.starts_with("W/\""))
                && tag.ends_with('"')
        })
        .map(str::to_owned);
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| transient())? {
        if body.len() + chunk.len() > COMPRESSED_BYTES {
            return Err(TransferError::permanent("docsrs_transfer_budget"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response::Gzip(body, etag))
}

fn decompress(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut stream = GzDecoder::new(compressed);
    let mut body = Vec::new();
    stream
        .by_ref()
        .take(JSON_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > JSON_BYTES || !stream.get_ref().is_empty() {
        return Err("invalid or oversized docs.rs gzip body".into());
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::decompress;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;

    #[test]
    fn decodes_one_complete_gzip_member() {
        let mut fixture = GzEncoder::new(Vec::new(), Compression::fast());
        fixture.write_all(b"synthetic rustdoc").unwrap();
        let mut bytes = fixture.finish().unwrap();
        assert_eq!(decompress(&bytes).unwrap(), b"synthetic rustdoc");
        assert!(decompress(&bytes[..bytes.len() - 1]).is_err());
        bytes.push(0);
        assert!(decompress(&bytes).is_err());
    }
}
