use crate::ipc::command::{MuxControlCommand, MuxControlResponse};
use crate::ipc::event::IpcEvent;
use crate::ipc::handlers::handle_command;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

// Limit to 32 concurrent connections.
const MAX_CONCURRENT: usize = 32;

pub struct MuxControlContext {
    pub state: Arc<tokio::sync::Mutex<crate::state::MuxState>>,
    pub event_tx: Option<broadcast::Sender<IpcEvent>>,
}

impl MuxControlContext {
    pub fn new(
        state: Arc<tokio::sync::Mutex<crate::state::MuxState>>,
        event_tx: Option<broadcast::Sender<IpcEvent>>,
    ) -> Self {
        Self { state, event_tx }
    }
}

pub fn socket_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".rust-mux")
        .join("ipc")
        .join("control.sock")
}

fn vibecrafted_home() -> PathBuf {
    std::env::var_os("VIBECRAFTED_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".vibecrafted")))
        .unwrap_or_else(|| PathBuf::from(".vibecrafted"))
}

pub async fn run_server(ctx: Arc<MuxControlContext>) -> Result<(), String> {
    let path = socket_path();

    if let Some(tx) = ctx.event_tx.clone() {
        let vibecrafted_home = vibecrafted_home();
        tokio::spawn(async move {
            if let Err(error) = crate::jsonl_bridge::run_jsonl_bridge(vibecrafted_home, tx).await {
                eprintln!("events.jsonl bridge stopped: {error:#}");
            }
        });
    }

    // Create parent directory if it doesn't exist
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

    if path.exists() {
        let _ = tokio::fs::remove_file(&path).await;
    }

    let listener = UnixListener::bind(&path).map_err(|e| {
        format!(
            "Failed to bind to IPC control socket at {}: {}",
            path.display(),
            e
        )
    })?;

    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT));

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(res) => res,
            Err(e) => {
                eprintln!("Failed to accept IPC connection: {}", e);
                continue;
            }
        };

        // Acquire permit before handling
        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("IPC connection limit reached, rejecting");
                continue; // Drop connection immediately
            }
        };

        let ctx_clone = ctx.clone();
        tokio::spawn(async move {
            let _permit = permit; // Hold permit until task drops
            if let Err(e) = handle_connection(stream, ctx_clone).await {
                eprintln!("IPC connection error: {}", e);
            }
        });
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    ctx: Arc<MuxControlContext>,
) -> Result<(), String> {
    // 1. Verify peer UID matches our UID to prevent unauthorized access
    let peer_cred = stream
        .peer_cred()
        .map_err(|e| format!("Failed to get peer credentials: {}", e))?;
    let my_uid = unsafe { libc::geteuid() };
    if peer_cred.uid() != my_uid {
        return Err(format!(
            "Unauthorized IPC connection: peer UID {} != my UID {}",
            peer_cred.uid(),
            my_uid
        ));
    }

    let mut buf = vec![0u8; 32 * 1024]; // 32KB max per message

    // Assume one JSON line per connection, or newline delimited for sub/unsub streams.
    // For simplicity of this port, we read up to EOF or newline.
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break, // EOF
            Ok(n) => n,
            Err(e) => return Err(format!("Read error: {}", e)),
        };

        // Extract lines.
        let data = buf[..n].to_vec();
        let lines = data.split(|&b| b == b'\n').filter(|l| !l.is_empty());

        for line in lines {
            let cmd: MuxControlCommand = match serde_json::from_slice(line) {
                Ok(c) => c,
                Err(e) => {
                    let err_resp = MuxControlResponse::Error(format!("Parse error: {}", e));
                    let _ = send_response(&mut stream, &err_resp).await;
                    continue;
                }
            };

            let is_subscribe = cmd == MuxControlCommand::Subscribe;

            let response = handle_command(ctx.clone(), cmd).await?;
            let _ = send_response(&mut stream, &response).await;

            if is_subscribe {
                // Enter subscription loop
                if let Some(ref tx) = ctx.event_tx {
                    let mut rx = tx.subscribe();
                    loop {
                        tokio::select! {
                            Ok(event) = rx.recv() => {
                                let ev_resp = MuxControlResponse::Event(event);
                                if send_response(&mut stream, &ev_resp).await.is_err() {
                                    break; // Client disconnected
                                }
                            }
                            // Also need to read from stream to detect unsubscribe/disconnect
                            res = stream.read(&mut buf) => {
                                match res {
                                    Ok(0) => break, // EOF
                                    Ok(n) => {
                                        let data = buf[..n].to_vec();
                                        if let Ok(MuxControlCommand::Unsubscribe) = serde_json::from_slice::<MuxControlCommand>(&data) {
                                            let _ = send_response(&mut stream, &MuxControlResponse::Ok).await;
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                    }
                } else {
                    let _ = send_response(
                        &mut stream,
                        &MuxControlResponse::Error("No event sender configured".into()),
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}

async fn send_response(
    stream: &mut UnixStream,
    resp: &MuxControlResponse,
) -> Result<(), std::io::Error> {
    let mut data = serde_json::to_vec(resp)?;
    data.push(b'\n');
    stream.write_all(&data).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn test_unixstream_pair() {
        let (mut client, server) = UnixStream::pair().unwrap();

        let state = Arc::new(tokio::sync::Mutex::new(crate::state::MuxState::new(
            crate::state::MuxStateConfig {
                max_active_clients: 32,
                service_name: "test".into(),
                max_request_bytes: 1024 * 1024,
                request_timeout: std::time::Duration::from_secs(30),
                restart_backoff: std::time::Duration::from_secs(1),
                restart_backoff_max: std::time::Duration::from_secs(10),
                max_restarts: 3,
                queue_depth: 100,
                child_pid: None,
                event_tx: None,
            },
        )));
        let ctx = Arc::new(MuxControlContext::new(state, None));

        tokio::spawn(async move {
            handle_connection(server, ctx).await.unwrap();
        });

        // Test GetStatus
        let cmd = MuxControlCommand::GetStatus;
        let mut msg = serde_json::to_vec(&cmd).unwrap();
        msg.push(b'\n');
        client.write_all(&msg).await.unwrap();

        let mut buf = [0u8; 1024];
        let n = client.read(&mut buf).await.unwrap();
        let resp: MuxControlResponse = serde_json::from_slice(&buf[..n]).unwrap();

        match resp {
            MuxControlResponse::Status(_) => {} // Expected
            _ => panic!("Expected Status response"),
        }
    }
}

#[cfg(test)]
mod additional_tests {
    use super::*;
    use crate::ipc::command::{ClientKind, VerifyResult};

    #[test]
    fn test_serde_roundtrip_commands() {
        let cmds = vec![
            MuxControlCommand::Subscribe,
            MuxControlCommand::Unsubscribe,
            MuxControlCommand::GetStatus,
            MuxControlCommand::Verify {
                client_kind: ClientKind::Claude,
            },
            MuxControlCommand::RouteSnapshot,
            MuxControlCommand::RestartService {
                name: "test".into(),
            },
            MuxControlCommand::ReloadConfig,
            MuxControlCommand::Shutdown { graceful: true },
        ];

        for cmd in cmds {
            let s = serde_json::to_string(&cmd).unwrap();
            let parsed: MuxControlCommand = serde_json::from_str(&s).unwrap();
            assert_eq!(cmd, parsed);
        }
    }

    #[test]
    fn test_serde_roundtrip_responses() {
        let resps = vec![
            MuxControlResponse::Ok,
            MuxControlResponse::Error("test error".into()),
            MuxControlResponse::Unimplemented,
            MuxControlResponse::Routes(vec![]),
            MuxControlResponse::VerifyResult(VerifyResult {
                ok: true,
                non_mux_servers: vec![],
            }),
            MuxControlResponse::Event(IpcEvent::ServerHealth {
                name: "test".into(),
                rss_mb: 100,
                restarts: 0,
                last_error: None,
            }),
        ];

        for resp in resps {
            let s = serde_json::to_string(&resp).unwrap();
            let parsed: MuxControlResponse = serde_json::from_str(&s).unwrap();
            assert_eq!(resp, parsed);
        }
    }

    // Since UnixStream::pair peer_cred() always returns the same UID as the test runner,
    // we cannot easily test the "reject foreign UID" branch without mocking or running as root.
    // We will assume the condition is correct based on source code.
}
