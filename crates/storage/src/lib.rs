//! PostgreSQL connection policy for the foundation roles.

use std::{fs, time::Duration};

use gh_docs_core::{Result, required};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, config::SslMode};
use tokio_postgres_rustls::MakeRustlsConnect;

pub async fn connect(role: &str) -> Result<(Client, JoinHandle<()>)> {
    let secret = fs::read(required("DB_PASSWORD_FILE")?)?;
    let password = secret.strip_suffix(b"\n").unwrap_or(&secret);
    if password.is_empty() || password.len() > 4096 {
        return Err("invalid database password file length".into());
    }

    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(required("DB_CA_FILE")?)? {
        roots.add(cert?)?;
    }
    if roots.is_empty() {
        return Err("database CA file contains no certificates".into());
    }
    let tls = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let (client, connection) = tokio_postgres::Config::new()
        .host(&required("DB_HOST")?)
        .user(role)
        .password(password)
        .dbname("gh_docs")
        .ssl_mode(SslMode::Require)
        .connect_timeout(Duration::from_secs(4))
        .options("-csearch_path=pg_catalog -cstatement_timeout=4000 -clock_timeout=2000")
        .connect(MakeRustlsConnect::new(tls))
        .await?;
    let driver = tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("database connection ended: {error}");
        }
    });
    Ok((client, driver))
}

pub async fn verify_fixture(client: &Client) -> Result<()> {
    let row = client
        .query_one("SELECT value FROM foundation.probe WHERE id = 1", &[])
        .await?;
    if row.get::<_, &str>(0) != "phase-zero fixture" {
        return Err("unexpected foundation fixture".into());
    }
    Ok(())
}

/// Fixed operator-only qualification commands. All attempted mutations roll back.
pub async fn probe(mut client: Client, name: &str) -> Result<()> {
    if name == "--db-identity" {
        let row = client.query_one(
            "SELECT current_user::text, current_setting('server_encoding'), d.datlocprovider::text,
                    coalesce(d.datlocale, ''), coalesce(d.datcollversion, ''),
                    coalesce(pg_database_collation_actual_version(d.oid), ''), s.ssl,
                    current_setting('default_transaction_read_only')::boolean
             FROM pg_database d, pg_stat_ssl s
             WHERE d.datname = current_database() AND s.pid = pg_backend_pid()", &[],
        ).await?;
        let identity = (0..6)
            .map(|index| row.get::<_, &str>(index))
            .collect::<Vec<_>>();
        println!(
            "{} tls={} default_read_only={}",
            identity.join(" "),
            row.get::<_, bool>(6),
            row.get::<_, bool>(7)
        );
        return Ok(());
    }
    let sql = match name {
        "--db-select" => "SELECT value FROM foundation.probe WHERE id = 1",
        "--db-dml" => {
            "INSERT INTO foundation.probe VALUES (2, 'test'); UPDATE foundation.probe SET value = 'updated' WHERE id = 2; DELETE FROM foundation.probe WHERE id = 2"
        }
        "--db-ddl" => "CREATE TABLE foundation.denied_ddl (id integer)",
        "--db-temp" => "CREATE TEMP TABLE denied_temp (id integer)",
        "--db-private" => "SELECT id FROM foundation.operator_only",
        "--db-lock" => {
            "SET TRANSACTION READ ONLY; SELECT pg_advisory_xact_lock_shared(2); SELECT value FROM foundation.probe"
        }
        _ => return Err("unknown database probe".into()),
    };
    let transaction = client.transaction().await?;
    if matches!(name, "--db-dml" | "--db-ddl" | "--db-temp") {
        // Verify actual privilege denial, even with an explicitly writable
        // transaction overriding the serving role's safe default.
        transaction
            .batch_execute("SET TRANSACTION READ WRITE")
            .await?;
    }
    let result = transaction.batch_execute(sql).await;
    transaction.rollback().await?;
    if let Err(error) = result {
        if let Some(db) = error.as_db_error() {
            eprintln!("database probe SQLSTATE {}", db.code().code());
        }
        return Err(error.into());
    }
    println!("database probe passed");
    Ok(())
}
