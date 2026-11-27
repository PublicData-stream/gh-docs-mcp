use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use docs_mcp_core::Result;
use reqwest::{Certificate, Client, StatusCode, header};
use tokio::net::UnixStream;
use tokio_postgres::Client as Database;
use uuid::Uuid;

use crate::{approval::Approval, worker};

const REF_RESPONSE: u64 = 4 * 1024 * 1024;
pub const SAMPLE_LIMIT: usize = 64 * 1024;

pub struct TransferError {
    pub reason: &'static str,
    pub retry: Option<Duration>,
}

impl TransferError {
    pub fn permanent(reason: &'static str) -> Self {
        Self {
            reason,
            retry: None,
        }
    }

    pub fn transient(reason: &'static str, after: Duration) -> Self {
        Self {
            reason,
            retry: Some(after),
        }
    }
}

pub struct Upstream<'a> {
    database: &'a Database,
    job: Uuid,
    approval: &'a Approval,
    remaining: u64,
    requests: u32,
    fixture: Option<Fixture>,
    sample: PathBuf,
}

#[derive(Clone)]
pub struct Fixture {
    #[cfg(feature = "fixtures")]
    pub address: SocketAddr,
    #[cfg(feature = "fixtures")]
    pub ca: Option<std::path::PathBuf>,
    #[cfg(feature = "fixtures")]
    pub fault: Option<String>,
}

impl Fixture {
    pub fn terminate_at(&self, point: &str) {
        #[cfg(feature = "fixtures")]
        if self.fault.as_deref() == Some(point) {
            std::process::exit(86);
        }
        #[cfg(not(feature = "fixtures"))]
        let _ = point;
    }
}

impl<'a> Upstream<'a> {
    pub fn new(
        database: &'a Database,
        job: Uuid,
        approval: &'a Approval,
        fixture: Option<Fixture>,
        sample: PathBuf,
    ) -> Self {
        Self {
            database,
            job,
            approval,
            remaining: approval.limits.response_bytes,
            requests: 0,
            fixture,
            sample,
        }
    }

    pub fn received(&self) -> u64 {
        self.approval.limits.response_bytes - self.remaining
    }

    pub async fn git(
        &mut self,
        scratch: &Path,
        odb: &Path,
        args: &[std::ffi::OsString],
        max: usize,
        wants: &BTreeSet<String>,
    ) -> std::result::Result<Vec<u8>, TransferError> {
        let (parent, child) = std::os::unix::net::UnixStream::pair()
            .map_err(|_| TransferError::permanent("broker_unavailable"))?;
        parent
            .set_nonblocking(true)
            .map_err(|_| TransferError::permanent("broker_unavailable"))?;
        let parent = UnixStream::from_std(parent)
            .map_err(|_| TransferError::permanent("broker_unavailable"))?;
        let mut process = worker::GitProcess::spawn(scratch, odb, args, Some(&child))
            .map_err(|_| TransferError::permanent("sandbox_unavailable"))?;
        drop(child);
        let result = tokio::try_join!(self.serve(parent, wants), async {
            process
                .output(max)
                .await
                .map_err(|_| TransferError::permanent("git_worker_failed"))
        },);
        process.stop();
        result.map(|((), output)| output)
    }

    async fn serve(
        &mut self,
        mut channel: UnixStream,
        wants: &BTreeSet<String>,
    ) -> std::result::Result<(), TransferError> {
        let invalid = || TransferError::permanent("invalid_git_protocol");
        let mut discovered = false;
        while let Some(request) = worker::broker_request(&mut channel)
            .await
            .map_err(|_| invalid())?
        {
            if request.is_empty() == discovered {
                return Err(invalid());
            }
            let fetch = if discovered {
                worker::validate_post(&request, wants).map_err(|_| invalid())?
            } else {
                false
            };
            self.request(&mut channel, discovered, request, fetch)
                .await?;
            discovered = true;
        }
        Ok(())
    }

    async fn request(
        &mut self,
        channel: &mut UnixStream,
        post: bool,
        body: Vec<u8>,
        fetch: bool,
    ) -> std::result::Result<(), TransferError> {
        let transient = || TransferError::transient("upstream_unavailable", Duration::from_secs(2));
        let invalid = || TransferError::permanent("invalid_git_protocol");
        self.requests += 1;
        if self.requests > 48 || self.remaining == 0 {
            return Err(TransferError::permanent("transfer_budget"));
        }
        let (client, base) = client(self.approval, self.fixture.as_ref()).await?;
        loop {
            let row = self
                .database
                .query_one(
                    "SELECT git_catalog.acquire_upstream_start($1,$2)",
                    &[&self.job, &self.approval.host()],
                )
                .await
                .map_err(|_| TransferError::permanent("approval_unavailable"))?;
            if row.get(0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let suffix = if post {
            "/git-upload-pack"
        } else {
            "/info/refs?service=git-upload-pack"
        };
        let mut request = client
            .request(
                if post {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                },
                format!("{base}{suffix}"),
            )
            .header("Git-Protocol", "version=2")
            .header(header::ACCEPT_ENCODING, "identity");
        if post {
            request = request
                .header(
                    header::CONTENT_TYPE,
                    "application/x-git-upload-pack-request",
                )
                .header(header::ACCEPT, "application/x-git-upload-pack-result")
                .body(body);
        }
        let mut response = request.send().await.map_err(|error| {
            if error.is_timeout() || error.is_body() {
                transient()
            } else {
                TransferError::permanent("upstream_connection_or_tls")
            }
        })?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let value = response.headers().get(header::RETRY_AFTER);
            let after = match value {
                None => Duration::from_secs(2),
                Some(value) => {
                    let Some(seconds) = value.to_str().ok().and_then(|value| value.parse().ok())
                    else {
                        return Err(TransferError::permanent("retry_after_unavailable"));
                    };
                    Duration::from_secs(seconds)
                }
            };
            return Err(TransferError::transient("upstream_unavailable", after));
        }
        if status != StatusCode::OK || response.headers().contains_key(header::CONTENT_ENCODING) {
            return Err(TransferError::permanent("upstream_response_denied"));
        }
        let content_type = if post {
            "application/x-git-upload-pack-result"
        } else {
            "application/x-git-upload-pack-advertisement"
        };
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some(content_type)
        {
            return Err(invalid());
        }
        let mut remaining = if fetch {
            self.remaining
        } else {
            self.remaining.min(REF_RESPONSE)
        };
        let mut sample = if fetch {
            Some(
                fs::File::create(&self.sample)
                    .map_err(|_| TransferError::permanent("diagnostics_unavailable"))?,
            )
        } else {
            None
        };
        let mut sample_left = SAMPLE_LIMIT;
        let mut framing = ResponseFrames::default();
        while let Some(chunk) = response.chunk().await.map_err(|_| transient())? {
            let length = chunk.len() as u64;
            if length > remaining {
                self.remaining = self.remaining.saturating_sub(length);
                return Err(TransferError::permanent("transfer_budget"));
            }
            remaining -= length;
            self.remaining -= length;
            if let Some(sample) = &mut sample
                && sample_left > 0
            {
                let length = sample_left.min(chunk.len());
                sample
                    .write_all(&chunk[..length])
                    .map_err(|_| TransferError::permanent("diagnostics_unavailable"))?;
                sample_left -= length;
            }
            framing.push(&chunk).map_err(|_| invalid())?;
            if !post
                && !framing.version_prefix.starts_with(b"000eversion 2\n")
                && !b"000eversion 2\n".starts_with(&framing.version_prefix)
            {
                return Err(invalid());
            }
            worker::broker_chunk(channel, &chunk)
                .await
                .map_err(|_| invalid())?;
        }
        if !framing.complete() {
            return Err(invalid());
        }
        if post {
            worker::broker_chunk(channel, b"0002")
                .await
                .map_err(|_| invalid())?;
        }
        worker::broker_end(channel).await.map_err(|_| invalid())?;
        Ok(())
    }
}

pub(crate) async fn client(
    approval: &Approval,
    fixture: Option<&Fixture>,
) -> std::result::Result<(Client, String), TransferError> {
    let host = approval.host();
    let (base, addresses, fixture_certificate) = if let Some(_fixture) = fixture {
        #[cfg(feature = "fixtures")]
        {
            let fixture_certificate = _fixture.ca.as_deref();
            let scheme = if fixture_certificate.is_some() {
                "https"
            } else {
                "http"
            };
            let path = approval.url[8 + host.len()..].to_owned();
            (
                format!("{scheme}://{host}:{}{path}", _fixture.address.port()),
                vec![_fixture.address],
                fixture_certificate,
            )
        }
        #[cfg(not(feature = "fixtures"))]
        {
            let _ = fixture;
            return Err(TransferError::permanent("fixture_transport_disabled"));
        }
    } else {
        let resolver = hickory_resolver::TokioResolver::builder_tokio()
            .map_err(|_| TransferError::permanent("resolver_unavailable"))?
            .build()
            .map_err(|_| TransferError::permanent("resolver_unavailable"))?;
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            resolver.lookup_ip(format!("{host}.")),
        )
        .await
        .ok()
        .and_then(std::result::Result::ok)
        .ok_or_else(|| TransferError::transient("dns_unavailable", Duration::from_secs(2)))?;
        let addresses = answer
            .iter()
            .map(|ip| SocketAddr::new(ip, 443))
            .collect::<Vec<_>>();
        if addresses.is_empty()
            || addresses.len() > 32
            || addresses.iter().any(|value| !public(value.ip()))
        {
            return Err(TransferError::permanent("destination_denied"));
        }
        (approval.url.clone(), addresses, None)
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = Client::builder()
        .resolve_to_addrs(host, &addresses)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .http1_only()
        .connect_timeout(Duration::from_secs(5))
        .read_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(0);
    let ca_file = fixture_certificate.unwrap_or(Path::new("/etc/ssl/certs/ca-certificates.crt"));
    let roots = std::fs::read(ca_file)
        .ok()
        .and_then(|pem| Certificate::from_pem_bundle(&pem).ok())
        .filter(|roots| !roots.is_empty())
        .ok_or_else(|| TransferError::permanent("tls_roots_unavailable"))?;
    builder = builder.tls_certs_only(roots);
    let client = builder
        .build()
        .map_err(|_| TransferError::permanent("transport_unavailable"))?;
    Ok((client, base))
}
#[derive(Default)]
struct ResponseFrames {
    header: Vec<u8>,
    payload: usize,
    flush: bool,
    version_prefix: Vec<u8>,
}

impl ResponseFrames {
    fn push(&mut self, mut bytes: &[u8]) -> Result<()> {
        let prefix = bytes.len().min(14 - self.version_prefix.len());
        self.version_prefix.extend_from_slice(&bytes[..prefix]);
        while !bytes.is_empty() {
            if self.payload > 0 {
                let n = bytes.len().min(self.payload);
                bytes = &bytes[n..];
                self.payload -= n;
            } else {
                let n = bytes.len().min(4 - self.header.len());
                self.header.extend_from_slice(&bytes[..n]);
                bytes = &bytes[n..];
                if self.header.len() == 4 {
                    let size = usize::from_str_radix(std::str::from_utf8(&self.header)?, 16)?;
                    if !(size <= 1 || (4..=65520).contains(&size)) {
                        return Err("invalid Git response framing".into());
                    }
                    self.flush = size == 0;
                    self.payload = size.saturating_sub(4);
                    self.header.clear();
                }
            }
        }
        Ok(())
    }

    fn complete(&self) -> bool {
        self.flush && self.payload == 0 && self.header.is_empty()
    }
}

fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    !matches!(
        ip.octets(),
        [0 | 10 | 127 | 224..=255, ..]
            | [100, 64..=127, ..]
            | [169, 254, ..]
            | [172, 16..=31, ..]
            | [192, 0, 0 | 2, _]
            | [192, 88, 99, _]
            | [192, 168, ..]
            | [198, 18 | 19, ..]
            | [198, 51, 100, _]
            | [203, 0, 113, _]
    )
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let [a, b, ..] = ip.segments();
    a & 0xe000 == 0x2000
        && !(a == 0x2001 && (b <= 0x1ff || b == 0xdb8))
        && a != 0x2002
        && a & 0xfff0 != 0x3ff0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_local_and_transition_addresses() {
        for ip in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "192.0.2.1",
            "192.168.1.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2002:7f00:1::1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }
}
