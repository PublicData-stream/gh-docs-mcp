//! Local JSON fixture driver for the private exploration API.

use std::{
    fs::File,
    io::{self, Read},
    sync::Arc,
};

use docs_mcp_core::{Result, required};
use docs_mcp_search::exploration::{CursorKey, Error, Explorer, Request};
use serde_json::{Value, json, value::RawValue};
use tokio::task::JoinSet;

const REQUEST_BYTES: u64 = 128 * 1024;
const BATCH_LIMIT: usize = 16;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--schema"] {
        println!(
            "{}",
            serde_json::to_string_pretty(&schemars::schema_for!(Request))?
        );
        return Ok(());
    }
    let fail = match args.as_slice() {
        [] => false,
        [arg] if arg == "--fail-setup" => true,
        _ => return Err("unknown Phase 4 fixture option".into()),
    };
    let file = File::open(required("EXPLORATION_CURSOR_KEY_FILE")?)?;
    let mut bytes = Vec::new();
    file.take(49).read_to_end(&mut bytes)?;
    let key = CursorKey::new(required("EXPLORATION_CURSOR_KEY_ID")?, &bytes)?;
    let explorer = Explorer::new(key);
    let explorer = Arc::new(if fail {
        explorer.force_worker_failure_for_fixture()
    } else {
        explorer
    });

    bytes.clear();
    io::stdin()
        .lock()
        .take(REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let output = if bytes.len() as u64 <= REQUEST_BYTES {
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'[') {
            match serde_json::from_slice::<Vec<Box<RawValue>>>(&bytes) {
                Ok(requests) if (1..=BATCH_LIMIT).contains(&requests.len()) => {
                    batch(explorer, requests).await?
                }
                _ => failure(Error::InvalidArgument),
            }
        } else {
            execute(&explorer, serde_json::from_slice(&bytes)).await
        }
    } else {
        failure(Error::InvalidArgument)
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

async fn batch(explorer: Arc<Explorer>, input: Vec<Box<RawValue>>) -> Result<Value> {
    let mut tasks = JoinSet::new();
    let mut ordered = vec![Value::Null; input.len()];
    for (index, value) in input.into_iter().enumerate() {
        let explorer = Arc::clone(&explorer);
        tasks.spawn(async move {
            (
                index,
                execute(&explorer, serde_json::from_str(value.get())).await,
            )
        });
    }
    while let Some(result) = tasks.join_next().await {
        let (index, output) = result?;
        ordered[index] = output;
    }
    Ok(json!(ordered))
}

async fn execute(explorer: &Explorer, request: serde_json::Result<Request>) -> Value {
    match request {
        Ok(request) => match explorer.execute(request).await {
            Ok(result) => json!({"result":result}),
            Err(error) => failure(error),
        },
        Err(_) => failure(Error::InvalidArgument),
    }
}

fn failure(error: Error) -> Value {
    json!({"error":error})
}
