//! TLS over a stream that JavaScript owns, for `new tls.TLSSocket(socket)` and
//! `tls.connect({ socket })`. Ciphertext from the stream is fed in here, and ciphertext to send
//! is posted back as `wire` events, so any duplex works as the transport, as in Node.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use rquickjs::{Ctx, Function, Object, TypedArray, Value};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::net::{Entry, Event, Sender, insert, plain_conn, post, secure, stream_kind};
use super::set_fn;
use super::tls::Tls;
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, from_json, throw};
use crate::runtime::state::state;

/// The sending end of a wrapped stream's incoming ciphertext. Dropping it is end of stream.
struct Feed(UnboundedSender<Vec<u8>>);

struct WireIo {
    incoming: UnboundedReceiver<Vec<u8>>,
    pending: Vec<u8>,
    offset: usize,
    sender: Sender,
    id: u64,
    ended: bool,
}

impl AsyncRead for WireIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            if self.offset < self.pending.len() {
                let count = buf.remaining().min(self.pending.len() - self.offset);
                let start = self.offset;
                buf.put_slice(&self.pending[start..start + count]);
                self.offset += count;
                return Poll::Ready(Ok(()));
            }
            match self.incoming.poll_recv(cx) {
                Poll::Ready(Some(bytes)) => (self.pending, self.offset) = (bytes, 0),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for WireIo {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.ended {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        post(&self.sender, self.id, Event::Wire(buf.to_vec()));
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.ended {
            self.ended = true;
            post(&self.sender, self.id, Event::WireEnd);
        }
        Poll::Ready(Ok(()))
    }
}

/// Start a TLS handshake over a JavaScript stream. Returns `{ id, feed }`: the socket handle,
/// and the handle that `feed` and `feedEnd` take.
fn wrap<'js>(
    ctx: Ctx<'js>,
    options: Value<'js>,
    is_server: bool,
    callback: Function<'js>,
) -> rquickjs::Result<Value<'js>> {
    let tls = Tls::from_js(&ctx, options, is_server)?;
    let tls = tls.ok_or_else(|| throw(&ctx, &JsError::type_error("TLS options are required")))?;
    let id = insert(&ctx, stream_kind(None).0, Some(callback));
    let state = state();
    let (feed, incoming) = unbounded_channel();
    let feed = state.insert_resource(Box::new(Feed(feed)));
    let sender = state.sender.clone();
    let wire = WireIo { incoming, pending: Vec::new(), offset: 0, sender: sender.clone(), id, ended: false };
    let task = state.shared.tokio.spawn(async move {
        let event = match secure(Ok(plain_conn(Box::new(wire))), Some(tls), &sender, id).await {
            Ok(conn) => Event::Connect(conn),
            Err(error) => Event::Error(error),
        };
        post(&sender, id, event);
    });
    let _ = state.with_resource(id, |entry: &mut Entry| entry.task = Some(task.abort_handle()));
    from_json(&ctx, &json!({ "id": id, "feed": feed }))
}

pub(super) fn natives<'js>(object: &Object<'js>) -> rquickjs::Result<()> {
    set_fn(object, "wrap", wrap)?;
    set_fn(object, "feed", |feed: u64, bytes: TypedArray<'js, u8>| {
        let bytes = bytes_of(&bytes);
        let _ = state().with_resource(feed, |feed: &mut Feed| feed.0.send(bytes));
    })?;
    set_fn(object, "feedEnd", |feed: u64| state().resources.borrow_mut().remove(&feed).is_some())?;
    Ok(())
}
