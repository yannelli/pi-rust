//! HTTP/2 stream tasks for `node:http2`: sending headers, data, and trailers with h2's flow
//! control, and forwarding received frames to JavaScript.

use std::future::poll_fn;

use bytes::Bytes;
use h2::{Reason, RecvStream, SendStream};
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use tokio::sync::{mpsc, oneshot};

use super::http2::{Event, Poster};
use crate::error::JsError;

pub(super) type Headers = Vec<(String, String)>;

pub(super) type Done = oneshot::Sender<Result<(), JsError>>;
type PendingData = (Bytes, bool, Done);

pub(super) enum StreamCommand {
    Respond(Headers, bool),
    Informational(Headers),
    Data(Bytes, bool, Done),
    Trailers(Headers),
    Reset(u32),
}

pub(super) enum Sender {
    Respond(h2::server::SendResponse<Bytes>),
    Send(SendStream<Bytes>),
    Done,
}

pub(super) fn h2_error(error: &h2::Error) -> JsError {
    let code = error.reason().map_or(2, u32::from);
    let mut result = JsError::new(error.to_string()).with_code("ERR_HTTP2_ERROR").with_data("errno", code);
    if error.is_io() {
        result = JsError::new(error.to_string()).with_code("ECONNRESET");
    }
    result
}

fn header_map(headers: &Headers) -> Result<HeaderMap, JsError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers.iter().filter(|(name, _)| !name.starts_with(':')) {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            JsError::type_error(format!("Invalid header name \"{name}\"")).with_code("ERR_INVALID_HTTP_TOKEN")
        })?;
        let value = HeaderValue::from_bytes(&crate::runtime::convert::latin1_to_bytes(value)).map_err(|_| {
            JsError::type_error(format!("Invalid character in header content [\"{name}\"]"))
                .with_code("ERR_INVALID_CHAR")
        })?;
        map.append(name, value);
    }
    Ok(map)
}

fn pseudo<'a>(headers: &'a Headers, name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

fn response(headers: &Headers) -> Result<Response<()>, JsError> {
    let status = pseudo(headers, ":status").unwrap_or("200").parse::<u16>().unwrap_or(0);
    let mut response = Response::builder().status(status).body(()).map_err(|_| {
        JsError::range_error(format!("Invalid status code: {status}")).with_code("ERR_HTTP2_STATUS_INVALID")
    })?;
    *response.headers_mut() = header_map(headers)?;
    Ok(response)
}

pub(super) fn request(headers: &Headers) -> Result<Request<()>, JsError> {
    let method = pseudo(headers, ":method").unwrap_or("GET");
    let authority = pseudo(headers, ":authority").unwrap_or("localhost");
    let uri = if method == "CONNECT" && pseudo(headers, ":protocol").is_none() {
        authority.to_string()
    } else {
        format!(
            "{}://{authority}{}",
            pseudo(headers, ":scheme").unwrap_or("https"),
            pseudo(headers, ":path").unwrap_or("/")
        )
    };
    let mut builder = Request::builder().method(method).uri(&uri);
    if let Some(protocol) = pseudo(headers, ":protocol") {
        builder = builder.extension(h2::ext::Protocol::from(protocol));
    }
    let mut request = builder
        .body(())
        .map_err(|error| JsError::type_error(error.to_string()).with_code("ERR_HTTP2_INVALID_PSEUDOHEADER"))?;
    *request.headers_mut() = header_map(headers)?;
    Ok(request)
}

fn push_headers(list: &mut Headers, headers: &HeaderMap) {
    for (name, value) in headers {
        list.push((name.to_string(), value.as_bytes().iter().map(|&byte| byte as char).collect()));
    }
}

pub(super) fn request_headers<B>(request: &Request<B>) -> Headers {
    let uri = request.uri();
    let mut list = vec![(":method".to_string(), request.method().to_string())];
    if let Some(path) =
        uri.path_and_query().filter(|_| request.method() != http::Method::CONNECT || uri.scheme().is_some())
    {
        list.push((":path".into(), path.to_string()));
    }
    if let Some(authority) = uri.authority() {
        list.push((":authority".into(), authority.to_string()));
    }
    if let Some(scheme) = uri.scheme_str() {
        list.push((":scheme".into(), scheme.to_string()));
    }
    if let Some(protocol) = request.extensions().get::<h2::ext::Protocol>() {
        list.push((":protocol".into(), protocol.as_str().to_string()));
    }
    push_headers(&mut list, request.headers());
    list
}

pub(super) fn response_headers<B>(response: &Response<B>) -> Headers {
    let mut list = vec![(":status".to_string(), response.status().as_u16().to_string())];
    push_headers(&mut list, response.headers());
    list
}

async fn send_data(stream: &mut SendStream<Bytes>, mut bytes: Bytes, end: bool) -> Result<(), h2::Error> {
    loop {
        if bytes.is_empty() {
            return if end { stream.send_data(bytes, true) } else { Ok(()) };
        }
        stream.reserve_capacity(bytes.len());
        let capacity = match poll_fn(|cx| stream.poll_capacity(cx)).await {
            Some(capacity) => capacity?,
            None => return Err(h2::Error::from(Reason::STREAM_CLOSED)),
        };
        let chunk = bytes.split_to(capacity.min(bytes.len()));
        if !chunk.is_empty() {
            let last = end && bytes.is_empty();
            stream.send_data(chunk, last)?;
            if last {
                return Ok(());
            }
        }
    }
}

async fn reset_of(sender: &mut Sender) -> Option<Reason> {
    let result = match sender {
        Sender::Respond(respond) => poll_fn(|cx| respond.poll_reset(cx)).await,
        Sender::Send(stream) => poll_fn(|cx| stream.poll_reset(cx)).await,
        Sender::Done => return std::future::pending().await,
    };
    result.ok()
}

fn apply(sender: &mut Sender, command: StreamCommand) -> Result<Option<PendingData>, JsError> {
    match (std::mem::replace(sender, Sender::Done), command) {
        (Sender::Respond(mut respond), StreamCommand::Respond(headers, end)) => {
            let stream = respond.send_response(response(&headers)?, end).map_err(|error| h2_error(&error))?;
            *sender = if end { Sender::Done } else { Sender::Send(stream) };
        }
        (Sender::Respond(mut respond), StreamCommand::Informational(headers)) => {
            respond.send_informational(response(&headers)?).map_err(|error| h2_error(&error))?;
            *sender = Sender::Respond(respond);
        }
        (Sender::Send(stream), StreamCommand::Data(bytes, end, done)) => {
            *sender = Sender::Send(stream);
            return Ok(Some((bytes, end, done)));
        }
        (Sender::Send(mut stream), StreamCommand::Trailers(headers)) => {
            stream.send_trailers(header_map(&headers)?).map_err(|error| h2_error(&error))?;
        }
        (Sender::Respond(mut respond), StreamCommand::Reset(code)) => respond.send_reset(Reason::from(code)),
        (Sender::Send(mut stream), StreamCommand::Reset(code)) => stream.send_reset(Reason::from(code)),
        (_, StreamCommand::Data(_, _, done)) => {
            let _ = done.send(Err(JsError::new("The stream has been destroyed").with_code("ERR_STREAM_DESTROYED")));
        }
        (previous, _) => *sender = previous,
    }
    Ok(None)
}

/// Own the sending half of one stream: run commands from JavaScript in order.
pub(super) async fn sender_task(
    key: u64,
    mut sender: Sender,
    mut commands: mpsc::UnboundedReceiver<StreamCommand>,
    post: Poster,
) {
    loop {
        let command = tokio::select! {
            command = commands.recv() => command,
            reason = reset_of(&mut sender) => {
                if let Some(reason) = reason {
                    post.send(Event::Reset(key, reason.into()));
                }
                sender = Sender::Done;
                continue;
            }
        };
        let Some(command) = command else { return };
        match apply(&mut sender, command) {
            Ok(Some((bytes, end, done))) => {
                let Sender::Send(stream) = &mut sender else { continue };
                let result = send_data(stream, bytes, end).await.map_err(|error| h2_error(&error));
                if end {
                    sender = Sender::Done;
                }
                let _ = done.send(result);
            }
            Ok(None) => {}
            Err(error) => post.send(Event::StreamError(key, error)),
        }
    }
}

/// Forward a stream's body and trailers to JavaScript.
pub(super) async fn receive(key: u64, mut body: RecvStream, post: &Poster) {
    while let Some(data) = body.data().await {
        match data {
            Ok(bytes) => {
                let _ = body.flow_control().release_capacity(bytes.len());
                post.send(Event::Data(key, bytes.to_vec()));
            }
            Err(error) => return post.send(stream_failure(key, &error)),
        }
    }
    match body.trailers().await {
        Ok(Some(trailers)) => {
            let mut list = Vec::new();
            push_headers(&mut list, &trailers);
            post.send(Event::Trailers(key, list));
        }
        Ok(None) => {}
        Err(error) => return post.send(stream_failure(key, &error)),
    }
    post.send(Event::End(key));
}

pub(super) fn stream_failure(key: u64, error: &h2::Error) -> Event {
    match error.reason() {
        Some(reason) if error.is_reset() || error.is_go_away() => Event::Reset(key, reason.into()),
        _ => Event::StreamError(key, h2_error(error)),
    }
}

/// A client stream: informational headers, the response, then its body.
pub(super) async fn client_response(key: u64, mut future: h2::client::ResponseFuture, post: Poster) {
    while let Some(Ok(info)) = poll_fn(|cx| future.poll_informational(cx)).await {
        post.send(Event::Headers(key, response_headers(&info)));
    }
    match future.await {
        Ok(response) => {
            let end = response.body().is_end_stream();
            post.send(Event::Response(key, response_headers(&response), end));
            receive(key, response.into_body(), &post).await;
        }
        Err(error) => post.send(stream_failure(key, &error)),
    }
}
