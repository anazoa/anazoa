use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const DEFAULT_SOCKET: &str = "/run/anazoa.sock";

fn usage() -> ! {
    eprintln!("usage: ctl [-s socket] <call [peer_id] | hangup | status | answer <secs>|always>");
    std::process::exit(1);
}

fn version() -> ! {
    println!("anazoa-ctl {}", env!("CARGO_PKG_VERSION"));
    std::process::exit(0);
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let mut socket_path = DEFAULT_SOCKET.to_string();

    if args.peek().map(|s| s.as_str()) == Some("--version") {
        version();
    }
    if args.peek().map(|s| s.as_str()) == Some("-s") {
        args.next();
        socket_path = args
            .next()
            .ok_or_else(|| anyhow!("-s requires an argument"))?;
    }

    let method = args.next().unwrap_or_else(|| {
        usage();
    });
    let params: Value = match method.as_str() {
        "call" => {
            if let Some(id_str) = args.next() {
                let peer_id: i64 = id_str
                    .parse()
                    .map_err(|_| anyhow!("peer_id must be an integer"))?;
                json!({"peer_id": peer_id})
            } else {
                json!(null)
            }
        }
        "hangup" | "status" => json!(null),
        "answer" => {
            let arg = args.next().unwrap_or_else(|| usage());
            if arg == "always" {
                json!({})
            } else {
                let secs: u64 = arg
                    .parse()
                    .map_err(|_| anyhow!("answer: expected a number of seconds or 'always'"))?;
                json!({"secs": secs})
            }
        }
        _ => usage(),
    };

    let stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| anyhow!("connect {socket_path}: {e}"))?;

    let request = if params.is_null() {
        json!({"jsonrpc":"2.0","id":1,"method":method})
    } else {
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
    };

    let (read_half, mut write_half) = stream.into_split();
    let mut line = request.to_string();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;

    let mut reader = BufReader::new(read_half).lines();
    if let Some(response_line) = reader.next_line().await? {
        let response: Value = serde_json::from_str(&response_line)
            .map_err(|e| anyhow!("invalid response JSON: {e}"))?;

        if let Some(error) = response.get("error") {
            let msg = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            eprintln!("error: {msg}");
            std::process::exit(1);
        }

        if let Some(result) = response.get("result") {
            println!("{}", serde_json::to_string_pretty(result)?);
        }
    }

    Ok(())
}
