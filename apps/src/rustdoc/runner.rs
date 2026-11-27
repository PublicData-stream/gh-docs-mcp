//! Private mutual-TLS transport for the isolated Git documentation runner.

use std::{
    future::pending,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use docs_mcp_core::{Result, markdown::JSON_BYTES};
use docs_mcp_storage::connect;
use docs_mcp_sync::markdown::{
    git,
    jobs::{self, Outcome},
};
use rustls::{
    RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::WebPkiClientVerifier,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tokio_rustls::{TlsAcceptor, server::TlsStream};
use uuid::Uuid;

const HEADER_BYTES: usize = 16 * 1024;
const SECRET_DIR: &str = "/run/secrets";

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Lease,
    Complete {
        job_id: Uuid,
        attempt: i32,
        body_bytes: usize,
    },
    Fail {
        job_id: Uuid,
        attempt: i32,
    },
}

pub async fn serve() -> Result<()> {
    let bind: SocketAddr = match std::env::var("DOCS_MCP_RUSTDOC_RUNNER_BIND") {
        Ok(value) if !value.is_empty() => value.parse()?,
        Ok(_) | Err(std::env::VarError::NotPresent) => return pending().await,
        Err(error) => return Err(error.into()),
    };
    if bind.ip().is_unspecified() || bind.port() == 0 {
        return Err("Rustdoc runner link requires an explicit private bind address".into());
    }
    match bind.ip() {
        IpAddr::V4(ip)
            if ip.is_private()
                || ip.is_loopback()
                || (cfg!(feature = "phase6-fixtures")
                    && matches!(ip.octets(), [198, 18 | 19, ..])) => {}
        IpAddr::V6(ip) if ip.is_unique_local() || ip.is_loopback() => (),
        _ => return Err("Rustdoc runner link requires a private bind address".into()),
    }

    let tls = acceptor()?;
    let listener = TcpListener::bind(bind).await?;
    let slots = Arc::new(Semaphore::new(2));
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            result = listener.accept() => {
                let (socket, _) = result?;
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                let tls = tls.clone();
                handlers.spawn(async move {
                    let _permit = permit;
                    if let Err(_error) = handle(tls, socket).await {
                        #[cfg(feature = "phase6-fixtures")]
                        eprintln!("synthetic Rustdoc runner request failed: {_error:?}");
                    }
                });
            }
            _ = handlers.join_next(), if !handlers.is_empty() => (),
        }
    }
}

fn acceptor() -> Result<TlsAcceptor> {
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_file_iter(format!("{SECRET_DIR}/rustdoc-client-ca.crt"))?
    {
        roots.add(certificate?)?;
    }
    let certs = CertificateDer::pem_file_iter(format!("{SECRET_DIR}/rustdoc-server.crt"))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(format!("{SECRET_DIR}/rustdoc-server.key"))?;
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(
                WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots),
                    Arc::new(rustls::crypto::ring::default_provider()),
                )
                .build()?,
            )
            .with_single_cert(certs, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn handle(tls: TlsAcceptor, socket: TcpStream) -> Result<()> {
    let mut stream = tokio::time::timeout(Duration::from_secs(8), tls.accept(socket)).await??;
    let request = tokio::time::timeout(Duration::from_secs(8), read_header(&mut stream)).await??;
    let (mut client, mut driver) = connect("gh_docs_sync").await?;
    let result = tokio::select! {
        result = handle_request(&mut client, &mut stream, request) => result,
        _ = &mut driver => Err("Rustdoc runner database connection closed".into()),
    };
    driver.abort();
    result
}

async fn read_header(stream: &mut TlsStream<TcpStream>) -> Result<Request> {
    let count = stream.read_u32().await? as usize;
    if count == 0 || count > HEADER_BYTES {
        return Err("invalid Rustdoc runner request size".into());
    }
    let mut header = vec![0; count];
    stream.read_exact(&mut header).await?;
    Ok(serde_json::from_slice(&header)?)
}

async fn respond(stream: &mut TlsStream<TcpStream>, header: Value, body: &[u8]) -> Result<()> {
    let header = serde_json::to_vec(&header)?;
    tokio::time::timeout(Duration::from_secs(90), async {
        stream.write_u32(header.len() as u32).await?;
        stream.write_all(&header).await?;
        stream.write_all(body).await?;
        stream.shutdown().await
    })
    .await??;
    Ok(())
}

async fn handle_request(
    client: &mut tokio_postgres::Client,
    stream: &mut TlsStream<TcpStream>,
    request: Request,
) -> Result<()> {
    let status =
        match request {
            Request::Lease => {
                let Some(job) = jobs::claim(client, true).await? else {
                    return respond(stream, json!({"status":"idle"}), &[]).await;
                };
                match git::bundle(client, &job, crate::control::fixture()?.as_ref()).await {
                    Ok(bundle)
                        if jobs::active_job(client, job.id, job.attempt)
                            .await?
                            .is_some() =>
                    {
                        let (package, target) = job.approval.input.package_target();
                        return respond(stream, json!({
                        "status":"job", "job_id":job.id, "attempt":job.attempt,
                        "package":package, "package_path":bundle.package_path, "target":target,
                        "body_bytes":bundle.archive.len(),
                    }), &bundle.archive).await;
                    }
                    Ok(_) => "rejected",
                    Err(_) => {
                        jobs::finish(
                            client,
                            &job,
                            Outcome::Failure {
                                reason: "git_input_unavailable",
                                retry: Some(Duration::from_secs(60)),
                            },
                        )
                        .await?;
                        "input_unavailable"
                    }
                }
            }
            Request::Complete {
                job_id,
                attempt,
                body_bytes,
            } if (1..=JSON_BYTES).contains(&body_bytes) => {
                if let Some(job) = jobs::active_job(client, job_id, attempt).await? {
                    let mut body = vec![0; body_bytes];
                    tokio::time::timeout(Duration::from_secs(90), stream.read_exact(&mut body))
                        .await??;
                    match git::complete(client, &job, body).await {
                        Ok(true) => "accepted",
                        Ok(false) => "rejected",
                        Err(_) => {
                            jobs::finish(
                                client,
                                &job,
                                Outcome::Failure {
                                    reason: "invalid_git_rustdoc_result",
                                    retry: None,
                                },
                            )
                            .await?;
                            "rejected"
                        }
                    }
                } else {
                    "rejected"
                }
            }
            Request::Fail { job_id, attempt } => {
                if let Some(job) = jobs::active_job(client, job_id, attempt).await? {
                    if jobs::finish(
                        client,
                        &job,
                        Outcome::Failure {
                            reason: "git_rustdoc_build_failed",
                            retry: Some(Duration::from_secs(60)),
                        },
                    )
                    .await?
                    {
                        "accepted"
                    } else {
                        "rejected"
                    }
                } else {
                    "rejected"
                }
            }
            _ => "rejected",
        };
    respond(stream, json!({"status":status}), &[]).await
}
