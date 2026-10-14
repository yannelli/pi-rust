//! The daemon process of one session: its stdin writer, stdout frame reader, stderr log, exit watch, and pings.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::connection::{ConnectionOptions, Inner, PING_INTERVAL, RemoteError, Session, lock};
use crate::frame::FrameReader;

pub(crate) fn write_frame(session: &Session, bytes: Vec<u8>) {
    if session.live.load(Ordering::SeqCst)
        && let Some(writer) = lock(&session.writer).as_ref()
    {
        let _ = writer.send(bytes);
    }
}

pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let _ = SystemRandom::new().fill(&mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn teardown(inner: &Weak<Inner>, session: &Arc<Session>, error: RemoteError) {
    match inner.upgrade() {
        Some(inner) => inner.teardown(session, error),
        None => {
            session.teardown(error);
        }
    }
}

/// Spawn `command serve --token <hex>` and the tasks that serve its pipes.
pub(crate) fn start_session(
    inner: Weak<Inner>,
    options: &ConnectionOptions,
    command: &[String],
    id: u64,
) -> Result<Arc<Session>, RemoteError> {
    let Some((program, args)) = command.split_first() else {
        return Err(RemoteError::with("spawn_error", "No daemon command", false));
    };
    let token = random_hex(16);
    let mut child = Command::new(program)
        .args(args)
        .args(["serve", "--token", &token])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| RemoteError::with("spawn_error", &error.to_string(), true))?;
    let (writer, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
    let (kill, killed) = oneshot::channel();
    let session = Arc::new(Session {
        id,
        pending: Mutex::new(HashMap::new()),
        live: AtomicBool::new(true),
        writer: Mutex::new(Some(writer)),
        kill: Mutex::new(Some(kill)),
        last_seen: Mutex::new(Instant::now()),
    });
    let (mut stdin, mut stdout, mut stderr) =
        (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
    tokio::spawn(async move {
        while let Some(bytes) = outgoing.recv().await {
            if stdin.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });
    if let Some(on_log) = options.on_log.clone() {
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 16 * 1024];
            while let Ok(read) = stderr.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                on_log(&String::from_utf8_lossy(&buffer[..read]));
            }
        });
    } else {
        tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await });
    }
    let (reader_inner, reader_session, on_log) = (inner.clone(), session.clone(), options.on_log.clone());
    tokio::spawn(async move {
        let mut frames = FrameReader::new(&token);
        let mut buffer = vec![0u8; 256 * 1024];
        let mut synced = false;
        loop {
            let read = match stdout.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if !reader_session.live.load(Ordering::SeqCst) {
                return;
            }
            *lock(&reader_session.last_seen) = Instant::now();
            frames.push(&buffer[..read]);
            if !synced {
                let (found, noise) = frames.sync();
                if let (Some(noise), Some(on_log)) = (noise, &on_log) {
                    on_log(&noise);
                }
                if !found {
                    continue;
                }
                synced = true;
            }
            loop {
                match frames.next() {
                    Ok(Some(incoming)) => {
                        reader_session.dispatch(incoming.kind, incoming.id, incoming.json, incoming.payload)
                    }
                    Ok(None) => break,
                    Err(_) => {
                        teardown(&reader_inner, &reader_session, RemoteError::lost("Corrupt frame from pi-env"));
                        return;
                    }
                }
            }
        }
        teardown(&reader_inner, &reader_session, RemoteError::lost("pi-env connection lost"));
    });
    let (exit_inner, exit_session) = (inner.clone(), session.clone());
    tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status.ok(),
            _ = killed => {
                let _ = child.start_kill();
                child.wait().await.ok()
            }
        };
        let code = status.and_then(|status| status.code()).map_or("null".to_string(), |code| code.to_string());
        teardown(&exit_inner, &exit_session, RemoteError::lost(&format!("pi-env exited with code {code}")));
    });
    let ping_session = session.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
        loop {
            interval.tick().await;
            let Some(inner) = inner.upgrade() else { return };
            if !inner.ping(&ping_session) {
                return;
            }
        }
    });
    Ok(session)
}
