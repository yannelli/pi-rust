//! How a pipe reader ends once stopped: what it already read or can read at once still arrives, and nothing waits
//! for the pipe's end. The paused clock makes a reader that never ends fail its timeout at once.

use tokio::time::timeout;

use super::*;

const NEVER: Duration = Duration::from_secs(3600);

async fn collect(mut receiver: mpsc::Receiver<(OutputStream, Vec<u8>)>) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some((_, chunk)) = receiver.recv().await {
        bytes.extend(chunk);
    }
    bytes
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn delivers_what_the_pipe_already_holds_when_stopped_before_its_first_read() {
    let (mut writer, pipe) = tokio::io::duplex(READ_CHUNK);
    writer.write_all(b"payload").await.unwrap();
    let (sender, receiver) = mpsc::channel(8);
    // Stopped before the reader first runs, as when its thread was starved past the exit grace.
    let (stop, stopped) = watch::channel(true);
    let reader = spawn_reader(pipe, OutputStream::Stdout, sender, stopped);
    let bytes = timeout(NEVER, collect(receiver)).await.expect("the reader ends without the pipe's end");
    assert_eq!(bytes, b"payload");
    reader.await.unwrap();
    drop((writer, stop));
}

#[tokio::test(start_paused = true)]
async fn ends_a_quiet_open_pipe_once_stopped_without_waiting_for_its_end() {
    let (writer, pipe) = tokio::io::duplex(64);
    let (sender, receiver) = mpsc::channel(8);
    let (stop, stopped) = watch::channel(false);
    let reader = spawn_reader(pipe, OutputStream::Stdout, sender, stopped);
    tokio::task::yield_now().await;
    stop.send(true).unwrap();
    let bytes = timeout(NEVER, collect(receiver)).await.expect("the reader ends without the pipe's end");
    assert!(bytes.is_empty());
    reader.await.unwrap();
    drop(writer);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn keeps_a_queued_chunk_and_the_next_one_read_or_still_buffered_when_stopped() {
    let (mut writer, pipe) = tokio::io::duplex(READ_CHUNK);
    let (sender, receiver) = mpsc::channel(1);
    let queued = sender.clone();
    let (stop, stopped) = watch::channel(false);
    let reader = spawn_reader(pipe, OutputStream::Stdout, sender, stopped);
    writer.write_all(b"first").await.unwrap();
    let full = async {
        while queued.capacity() > 0 {
            tokio::task::yield_now().await;
        }
    };
    timeout(NEVER, full).await.expect("the first chunk is queued");
    // With the channel full, the reader sends this one as soon as it reads it; either way it must arrive.
    writer.write_all(b"second").await.unwrap();
    tokio::task::yield_now().await;
    stop.send(true).unwrap();
    drop(queued);
    let bytes = timeout(NEVER, collect(receiver)).await.expect("the reader ends without the pipe's end");
    assert_eq!(bytes, b"firstsecond");
    reader.await.unwrap();
    drop(writer);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn reads_at_most_one_chunk_after_stop_while_a_writer_keeps_writing() {
    let (mut writer, pipe) = tokio::io::duplex(4 * READ_CHUNK);
    writer.write_all(&vec![b'x'; 3 * READ_CHUNK]).await.unwrap();
    let feeder = tokio::spawn(async move { while writer.write_all(&[b'y'; 1024]).await.is_ok() {} });
    let (sender, receiver) = mpsc::channel(8);
    let (stop, stopped) = watch::channel(true);
    let reader = spawn_reader(pipe, OutputStream::Stdout, sender, stopped);
    let bytes = timeout(NEVER, collect(receiver)).await.expect("the reader ends while the writer goes on");
    assert_eq!(bytes.len(), READ_CHUNK);
    reader.await.unwrap();
    feeder.abort();
    drop(stop);
}
