//! Proves smb2 runs inside a Cloudflare Worker: every request opens an SMB
//! connection over a Workers TCP socket, logs in with NTLM, lists the share
//! recursively, reads the first file it finds, and answers with
//! `{"files": <count>, "first": {"name": <path>, "bytes": <len>}}`.
//! `?path=<path>` reads that file instead of the first one.
//!
//! The only glue smb2 needs is [`WorkerSockets`]: a `TransportFactory` that
//! opens a `worker::Socket` and frames SMB2 messages over it.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use smb2::transport::{TransportFactory, TransportReceive, TransportSend};
use smb2::{ClientConfig, SmbClient};
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::Mutex;
use worker::send::{SendFuture, SendWrapper};
use worker::{event, Context, Env, Request, Response, Socket};

/// Opens SMB connections over `connect()`, the Workers TCP socket API. A
/// Workers VPC binding's socket would be wrapped with `Socket::from` the same
/// way.
#[derive(Debug)]
struct WorkerSockets;

#[async_trait]
impl TransportFactory for WorkerSockets {
    async fn connect(
        &self,
        addr: &str,
    ) -> smb2::Result<(Box<dyn TransportSend>, Box<dyn TransportReceive>)> {
        let (host, port) = addr
            .rsplit_once(':')
            .and_then(|(host, port)| Some((host, port.parse::<u16>().ok()?)))
            .ok_or_else(|| smb2::Error::invalid_data(format!("not a host:port: {addr}")))?;
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        // A JS socket isn't `Send`; a Worker has one thread, so wrapping is
        // sound, and smb2's trait objects need it.
        let socket = SendFuture::new(async move {
            let socket = Socket::builder().connect(host, port)?;
            // Surface a refused connect here rather than on the first write.
            socket.opened().await?;
            Ok::<_, worker::Error>(socket)
        })
        .await
        .map_err(|e| smb2::Error::Io(std::io::Error::other(e.to_string())))?;
        let (reader, writer) = tokio::io::split(socket);
        Ok((
            Box::new(SocketSend(Mutex::new(SendWrapper::new(writer)))),
            Box::new(SocketReceive(Mutex::new(SendWrapper::new(reader)))),
        ))
    }
}

/// The largest message the 3-byte length in the direct-TCP header can carry
/// (MS-SMB2 § 2.1).
const MAX_FRAME: usize = (1 << 24) - 1;

struct SocketSend(Mutex<SendWrapper<WriteHalf<Socket>>>);
struct SocketReceive(Mutex<SendWrapper<ReadHalf<Socket>>>);

#[async_trait]
impl TransportSend for SocketSend {
    async fn send(&self, data: &[u8]) -> smb2::Result<()> {
        if data.len() > MAX_FRAME {
            return Err(smb2::Error::invalid_data(format!(
                "message of {} bytes doesn't fit a frame",
                data.len()
            )));
        }
        // One write per frame, header included: a zero byte, then the length
        // in 3 bytes big-endian.
        let mut frame = Vec::with_capacity(4 + data.len());
        frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
        frame.extend_from_slice(data);
        SendFuture::new(async {
            let mut writer = self.0.lock().await;
            writer.write_all(&frame).await?;
            writer.flush().await
        })
        .await
        .map_err(smb2::Error::Io)
    }
}

#[async_trait]
impl TransportReceive for SocketReceive {
    async fn receive(&self) -> smb2::Result<Vec<u8>> {
        SendFuture::new(async {
            let mut reader = self.0.lock().await;
            let mut header = [0u8; 4];
            reader.read_exact(&mut header).await.map_err(read_error)?;
            if header[0] != 0 {
                return Err(smb2::Error::invalid_data(format!(
                    "bad direct-TCP header {header:02x?}"
                )));
            }
            let mut message = vec![0u8; u32::from_be_bytes(header) as usize];
            reader.read_exact(&mut message).await.map_err(read_error)?;
            Ok(message)
        })
        .await
    }
}

/// The peer closing the socket is a disconnect, as smb2's own TCP reports it.
fn read_error(e: std::io::Error) -> smb2::Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        smb2::Error::Disconnected
    } else {
        smb2::Error::Io(e)
    }
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    let path = req
        .url()?
        .query_pairs()
        .find_map(|(key, value)| (key == "path").then(|| value.into_owned()));
    match probe(&env, path).await {
        Ok(body) => Response::from_json(&body),
        Err(e) => Response::error(format!("probe failed: {e}"), 502),
    }
}

async fn probe(
    env: &Env,
    path: Option<String>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let var = |name: &str| env.var(name).map(|v| v.to_string());
    let config = ClientConfig {
        addr: format!("{}:{}", var("SMB_HOST")?, var("SMB_PORT")?),
        username: var("SMB_USER")?,
        password: var("SMB_PASS")?,
        transport_factory: Some(Arc::new(WorkerSockets)),
        ..Default::default()
    };
    let mut client = SmbClient::connect(config).await?;
    let mut tree = client.connect_share(&var("SMB_SHARE")?).await?;

    // Depth-first, so the "first" file is stable for a given share.
    let mut files = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        let mut entries = client.list_directory(&mut tree, &dir).await?;
        entries.sort_by(|a, b| b.name.cmp(&a.name));
        for entry in entries {
            if entry.name == "." || entry.name == ".." {
                continue;
            }
            let path = if dir.is_empty() {
                entry.name
            } else {
                format!("{dir}/{}", entry.name)
            };
            if entry.is_directory {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }

    let first = match path.as_ref().or(files.first()) {
        Some(name) => {
            let bytes = client.read_file(&mut tree, name).await?.len();
            json!({ "name": name, "bytes": bytes })
        }
        None => serde_json::Value::Null,
    };
    client.disconnect_share(&tree).await?;
    Ok(json!({ "files": files.len(), "first": first }))
}
