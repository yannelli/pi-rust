//! Natives for `node:dgram`: UDP sockets on tokio. Received datagrams and errors reach a
//! JavaScript callback; socket options go through socket2.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object, Persistent, TypedArray, Value};
use serde::Deserialize;
use socket2::{Domain, Protocol, SockRef, Socket, Type};
use tokio::net::UdpSocket;

use super::net::{address_json, net_error, parse_ip, parse_options};
use super::set_fn;
use crate::error::JsError;
use crate::runtime::convert::{bytes_of, bytes_to_js, error_value, from_json, throw};
use crate::runtime::scope::spawn_op;
use crate::runtime::state::{Message, Task, state};
use crate::runtime::{UncaughtKind, report_error};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BindOptions {
    address: String,
    port: u16,
    #[serde(default)]
    reuse_addr: bool,
    #[serde(default)]
    ipv6_only: bool,
    recv_buffer_size: Option<usize>,
    send_buffer_size: Option<usize>,
}

struct Udp {
    socket: Arc<UdpSocket>,
    reader: tokio::task::AbortHandle,
    referenced: bool,
}

enum Event {
    Message(Vec<u8>, SocketAddr),
    Error(JsError),
}

fn post(sender: &std::sync::mpsc::Sender<Message>, id: u64, event: Event) {
    let task: Task = Box::new(move |ctx| deliver(ctx, id, event));
    let _ = sender.send(Message::Task(task));
}

fn deliver(ctx: &Ctx<'_>, id: u64, event: Event) {
    let callback = state().callbacks.borrow().get(&id).cloned();
    let Some(function) = callback.and_then(|callback| callback.restore(ctx).ok()) else { return };
    let result = match event {
        Event::Message(bytes, from) => {
            let mut info = address_json(Some(from));
            info["size"] = bytes.len().into();
            bytes_to_js(ctx, bytes).and_then(|bytes| {
                from_json(ctx, &info).and_then(|info| function.call::<_, ()>(("message", bytes, info)))
            })
        }
        Event::Error(error) => error_value(ctx, &error).and_then(|error| function.call::<_, ()>(("error", error))),
    };
    if let Err(error) = result {
        report_error(ctx, UncaughtKind::Exception, error);
    }
}

fn open(address: SocketAddr, options: &BindOptions) -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::for_address(address), Type::DGRAM, Some(Protocol::UDP))?;
    if options.reuse_addr {
        socket.set_reuse_address(true)?;
    }
    if address.is_ipv6() {
        socket.set_only_v6(options.ipv6_only)?;
    }
    if let Some(size) = options.recv_buffer_size {
        socket.set_recv_buffer_size(size)?;
    }
    if let Some(size) = options.send_buffer_size {
        socket.set_send_buffer_size(size)?;
    }
    socket.bind(&address.into())?;
    socket.set_nonblocking(true)?;
    UdpSocket::from_std(socket.into())
}

fn bind<'js>(ctx: Ctx<'js>, options: Value<'js>, callback: Function<'js>) -> rquickjs::Result<Value<'js>> {
    let options: BindOptions = parse_options(&ctx, options)?;
    let address = parse_ip(&ctx, &options.address, options.port)?;
    let state = state();
    let socket = {
        let _guard = state.shared.tokio.enter();
        open(address, &options)
    };
    let socket =
        socket.map_err(|error| throw(&ctx, &net_error(&error, "bind", Some(&options.address), Some(options.port))))?;
    let local = address_json(socket.local_addr().ok());
    let socket = Arc::new(socket);
    let id = state.next_id();
    let sender = state.sender.clone();
    let receiver = socket.clone();
    let reader = state.shared.tokio.spawn(async move {
        let mut buffer = vec![0; 65536];
        loop {
            match receiver.recv_from(&mut buffer).await {
                Ok((count, from)) => post(&sender, id, Event::Message(buffer[..count].to_vec(), from)),
                Err(error) => post(&sender, id, Event::Error(net_error(&error, "recvmsg", None, None))),
            }
        }
    });
    state.add_busy();
    let udp = Udp { socket, reader: reader.abort_handle(), referenced: true };
    state.resources.borrow_mut().insert(id, Box::new(udp));
    state.callbacks.borrow_mut().insert(id, Persistent::save(&ctx, callback));
    from_json(&ctx, &serde_json::json!({ "id": id, "address": local }))
}

fn socket(ctx: &Ctx<'_>, id: u64, syscall: &str) -> rquickjs::Result<Arc<UdpSocket>> {
    state().with_resource(id, |udp: &mut Udp| udp.socket.clone()).map_err(|_| {
        throw(ctx, &net_error(&std::io::Error::from_raw_os_error(EBADF), syscall, None, None).with_code("EBADF"))
    })
}

#[cfg(unix)]
const EBADF: i32 = libc::EBADF;
#[cfg(windows)]
const EBADF: i32 = 9;

fn option<'js>(
    ctx: &Ctx<'js>,
    id: u64,
    syscall: &str,
    f: impl FnOnce(SockRef<'_>) -> std::io::Result<()>,
) -> rquickjs::Result<()> {
    let socket = socket(ctx, id, syscall)?;
    f(SockRef::from(socket.as_ref())).map_err(|error| throw(ctx, &net_error(&error, syscall, None, None)))
}

fn set_ref(id: u64, referenced: bool) {
    let state = state();
    let changed = state.with_resource(id, |udp: &mut Udp| std::mem::replace(&mut udp.referenced, referenced));
    match changed {
        Ok(previous) if previous != referenced && referenced => state.add_busy(),
        Ok(previous) if previous != referenced => state.remove_busy(),
        _ => {}
    }
}

fn close(id: u64) {
    set_ref(id, false);
    let state = state();
    state.callbacks.borrow_mut().remove(&id);
    if let Some(Ok(udp)) = state.resources.borrow_mut().remove(&id).map(|resource| resource.downcast::<Udp>()) {
        udp.reader.abort();
    }
}

fn membership(ctx: &Ctx<'_>, id: u64, group: String, interface: Option<String>, join: bool) -> rquickjs::Result<()> {
    let syscall = if join { "addMembership" } else { "dropMembership" };
    let group: IpAddr =
        group.parse().map_err(|_| throw(ctx, &JsError::new(format!("{syscall} EINVAL")).with_code("EINVAL")))?;
    option(ctx, id, syscall, |socket| match group {
        IpAddr::V4(group) => {
            let interface = interface.and_then(|text| text.parse().ok()).unwrap_or(Ipv4Addr::UNSPECIFIED);
            if join {
                socket.join_multicast_v4(&group, &interface)
            } else {
                socket.leave_multicast_v4(&group, &interface)
            }
        }
        IpAddr::V6(group) => {
            if join {
                socket.join_multicast_v6(&group, 0)
            } else {
                socket.leave_multicast_v6(&group, 0)
            }
        }
    })
}

fn source_membership(
    ctx: &Ctx<'_>,
    id: u64,
    addresses: [String; 2],
    interface: Option<String>,
    join: bool,
) -> rquickjs::Result<()> {
    let syscall = if join { "addSourceSpecificMembership" } else { "dropSourceSpecificMembership" };
    let [source, group] = addresses;
    let Some(membership) = super::dgram_ssm::parse(&source, &group, interface.as_deref()) else {
        let error = JsError::new(format!("{syscall} EINVAL")).with_code("EINVAL");
        return Err(throw(ctx, &error.with_data("errno", -22).with_data("syscall", syscall)));
    };
    let socket = socket(ctx, id, syscall)?;
    super::dgram_ssm::apply(&SockRef::from(socket.as_ref()), &membership, join).map_err(|error| {
        if error.kind() == std::io::ErrorKind::Unsupported {
            throw(ctx, &JsError::unsupported("dgram IPv6 source-specific membership on this platform"))
        } else {
            throw(ctx, &net_error(&error, syscall, None, None))
        }
    })
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let object = Object::new(ctx.clone())?;
    set_fn(&object, "bind", bind)?;
    set_fn(
        &object,
        "send",
        |ctx: Ctx<'js>, id: u64, data: TypedArray<'js, u8>, address: Opt<String>, port: Opt<u16>| {
            let socket = socket(&ctx, id, "send")?;
            let bytes = bytes_of(&data);
            let target = match (address.0, port.0) {
                (Some(address), Some(port)) => Some(parse_ip(&ctx, &address, port)?),
                _ => None,
            };
            let work = async move {
                let sent = match target {
                    Some(target) => socket.send_to(&bytes, target).await,
                    None => socket.send(&bytes).await,
                };
                let shown = target.map(|target| target.ip().to_string());
                sent.map_err(|error| net_error(&error, "send", shown.as_deref(), target.map(|target| target.port())))
            };
            spawn_op(&ctx, work, |ctx, count: usize| Ok(Value::new_number(ctx.clone(), count as f64)))
        },
    )?;
    set_fn(&object, "connect", |ctx: Ctx<'js>, id: u64, address: String, port: u16| {
        let socket = socket(&ctx, id, "connect")?;
        let target = parse_ip(&ctx, &address, port)?;
        let work = async move {
            socket.connect(target).await.map_err(|error| net_error(&error, "connect", Some(&address), Some(port)))
        };
        spawn_op(&ctx, work, |ctx, ()| Ok(Value::new_undefined(ctx.clone())))
    })?;
    set_fn(&object, "disconnect", |ctx: Ctx<'js>, id: u64| option(&ctx, id, "disconnect", super::sys::udp_disconnect))?;
    set_fn(&object, "remoteAddress", |ctx: Ctx<'js>, id: u64| {
        let peer = socket(&ctx, id, "getpeername")?.peer_addr().ok();
        from_json(&ctx, &address_json(peer))
    })?;
    set_fn(&object, "setBroadcast", |ctx: Ctx<'js>, id: u64, on: bool| {
        option(&ctx, id, "setBroadcast", |s| s.set_broadcast(on))
    })?;
    set_fn(&object, "setTTL", |ctx: Ctx<'js>, id: u64, ttl: u32, v6: bool| {
        option(&ctx, id, "setTTL", |s| if v6 { s.set_unicast_hops_v6(ttl) } else { s.set_ttl_v4(ttl) })
    })?;
    set_fn(&object, "setMulticastTTL", |ctx: Ctx<'js>, id: u64, ttl: u32, v6: bool| {
        option(
            &ctx,
            id,
            "setMulticastTTL",
            |s| if v6 { s.set_multicast_hops_v6(ttl) } else { s.set_multicast_ttl_v4(ttl) },
        )
    })?;
    set_fn(&object, "setMulticastLoopback", |ctx: Ctx<'js>, id: u64, on: bool, v6: bool| {
        option(&ctx, id, "setMulticastLoopback", |s| {
            if v6 { s.set_multicast_loop_v6(on) } else { s.set_multicast_loop_v4(on) }
        })
    })?;
    set_fn(&object, "setMulticastInterface", |ctx: Ctx<'js>, id: u64, interface: String| {
        option(&ctx, id, "setMulticastInterface", |s| match interface.parse::<Ipv4Addr>() {
            Ok(address) => s.set_multicast_if_v4(&address),
            Err(_) => {
                s.set_multicast_if_v6(interface.rsplit('%').next().and_then(|index| index.parse().ok()).unwrap_or(0))
            }
        })
    })?;
    set_fn(&object, "addMembership", |ctx: Ctx<'js>, id: u64, group: String, interface: Option<String>| {
        membership(&ctx, id, group, interface, true)
    })?;
    set_fn(&object, "dropMembership", |ctx: Ctx<'js>, id: u64, group: String, interface: Option<String>| {
        membership(&ctx, id, group, interface, false)
    })?;
    set_fn(
        &object,
        "sourceMembership",
        |ctx: Ctx<'js>, id: u64, source: String, group: String, interface: Option<String>, join: bool| {
            source_membership(&ctx, id, [source, group], interface, join)
        },
    )?;
    set_fn(&object, "bufferSize", |ctx: Ctx<'js>, id: u64, recv: bool, size: Opt<usize>| -> rquickjs::Result<usize> {
        let socket = socket(&ctx, id, "uv_send_buffer_size")?;
        let sock = SockRef::from(socket.as_ref());
        let result = match (recv, size.0) {
            (true, Some(size)) => sock.set_recv_buffer_size(size).and_then(|()| sock.recv_buffer_size()),
            (false, Some(size)) => sock.set_send_buffer_size(size).and_then(|()| sock.send_buffer_size()),
            (true, None) => sock.recv_buffer_size(),
            (false, None) => sock.send_buffer_size(),
        };
        result.map_err(|error| {
            throw(
                &ctx,
                &super::os::system_error(&error, if recv { "uv_recv_buffer_size" } else { "uv_send_buffer_size" }),
            )
        })
    })?;
    set_fn(&object, "close", close)?;
    set_fn(&object, "ref", set_ref)?;
    Ok(object)
}
