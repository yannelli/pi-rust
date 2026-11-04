//! Integration tests for pi-js, built as one test binary. Binaries with their own main
//! (node_fork, node_cluster, node_ipc_*) stay separate; see Cargo.toml.

mod common;
mod node_common;

mod extension_fixture;
mod fetch;
mod fetch_body;
mod host_bridge;
mod modules;
mod node_apis;
mod node_apis_sys;
mod node_async_hooks;
mod node_backend;
mod node_backend_env;
mod node_backend_modules;
mod node_backend_transport;
mod node_buffer;
mod node_child;
mod node_child_kill;
mod node_core;
mod node_crypto;
mod node_crypto_keys;
mod node_crypto_modes;
mod node_dgram_dns;
mod node_fs;
mod node_http;
mod node_inspector;
mod node_ipc_sender_cleanup;
mod node_ipc_sender_unit;
mod node_json_parse;
mod node_legacy_mods;
mod node_listener_inheritance;
mod node_net;
mod node_readline;
mod node_repl;
mod node_sqlite;
mod node_stream_web_compression;
mod node_test_runner;
mod node_tls;
mod node_v8;
mod node_vm;
mod node_wasm;
mod node_web;
mod node_web_bytes;
mod node_worker;
mod node_zlib;
