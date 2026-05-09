use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

pub enum DaemonCmd {
    Call {
        peer_id: Option<i64>,
        resp: oneshot::Sender<Result<Value>>,
    },
    Hangup {
        resp: oneshot::Sender<Result<Value>>,
    },
    Status {
        resp: oneshot::Sender<Result<Value>>,
    },
}

pub fn bind_socket(socket_path: &str) -> Result<UnixListener> {
    let _ = std::fs::remove_file(socket_path);
    UnixListener::bind(socket_path).map_err(|e| anyhow!("bind {socket_path}: {e}"))
}

pub async fn run_jsonrpc_server(
    listener: UnixListener,
    cmd_tx: mpsc::Sender<DaemonCmd>,
) -> Result<()> {
    debug!("JSON-RPC server accepting connections");

    loop {
        let (stream, _) = listener.accept().await?;
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, cmd_tx).await {
                debug!("JSON-RPC connection closed: {e:#}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    cmd_tx: mpsc::Sender<DaemonCmd>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("parse error: {e}")}});
                write_line(&mut write_half, &resp).await?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        let result = dispatch(&method, &params, &cmd_tx).await;

        let response = match result {
            Ok(val) => json!({"jsonrpc":"2.0","id":id,"result":val}),
            Err(msg) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":msg}}),
        };
        write_line(&mut write_half, &response).await?;
    }
    Ok(())
}

async fn dispatch(
    method: &str,
    params: &Value,
    cmd_tx: &mpsc::Sender<DaemonCmd>,
) -> Result<Value, String> {
    let (resp_tx, resp_rx) = oneshot::channel();

    let cmd = match method {
        "call" => {
            let peer_id = params.get("peer_id").and_then(Value::as_i64);
            DaemonCmd::Call {
                peer_id,
                resp: resp_tx,
            }
        }
        "hangup" => DaemonCmd::Hangup { resp: resp_tx },
        "status" => DaemonCmd::Status { resp: resp_tx },
        other => return Err(format!("method not found: {other}")),
    };

    cmd_tx
        .send(cmd)
        .await
        .map_err(|_| "daemon shutting down".to_string())?;

    resp_rx
        .await
        .map_err(|_| "daemon did not respond".to_string())?
        .map_err(|e| e.to_string())
}

async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, value: &Value) -> Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    Ok(())
}
