// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
// Usage: provide a port as argument to run hyper_hello benchmark server
// otherwise this starts multiple servers on many ports for test endpoints.
use anyhow::anyhow;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use bytes::Bytes;
use denokv_proto::datapath::AtomicWrite;
use denokv_proto::datapath::AtomicWriteOutput;
use denokv_proto::datapath::AtomicWriteStatus;
use denokv_proto::datapath::ReadRangeOutput;
use denokv_proto::datapath::SnapshotRead;
use denokv_proto::datapath::SnapshotReadOutput;
use fastwebsockets::FragmentCollector;
use fastwebsockets::Frame;
use fastwebsockets::OpCode;
use fastwebsockets::Role;
use fastwebsockets::WebSocket;
use futures::future::join3;
use futures::future::poll_fn;
use futures::Future;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use h2::server::Handshake;
use h2::server::SendResponse;
use h2::Reason;
use h2::RecvStream;
use https::get_tls_listener_stream;
use https::SupportedHttpVersions;
use hyper::header::HeaderValue;
use hyper::http;
use hyper::server::Server;
use hyper::service::make_service_fn;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::Body;
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use npm::CUSTOM_NPM_PACKAGE_CACHE;
use once_cell::sync::Lazy;
use pretty_assertions::assert_eq;
use prost::Message;
use pty::Pty;
use regex::Regex;
use rustls_tokio_stream::TlsStream;
use serde::Serialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::env;
use std::io;
use std::io::Write;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV6;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Child;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::result::Result;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::task::LocalSet;
use url::Url;

pub mod assertions;
mod builders;
pub mod factory;
mod fs;
mod https;
pub mod lsp;
mod npm;
pub mod pty;

pub use builders::DenoChild;
pub use builders::TestCommandBuilder;
pub use builders::TestCommandOutput;
pub use builders::TestContext;
pub use builders::TestContextBuilder;
pub use fs::PathRef;
pub use fs::TempDir;

const PORT: u16 = 4545;
const TEST_AUTH_TOKEN: &str = "abcdef123456789";
const TEST_BASIC_AUTH_USERNAME: &str = "testuser123";
const TEST_BASIC_AUTH_PASSWORD: &str = "testpassabc";
const KV_DATABASE_ID: &str = "11111111-1111-1111-1111-111111111111";
const KV_ACCESS_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KV_DATABASE_TOKEN: &str = "MOCKMOCKMOCKMOCKMOCKMOCKMOCK";
const REDIRECT_PORT: u16 = 4546;
const ANOTHER_REDIRECT_PORT: u16 = 4547;
const DOUBLE_REDIRECTS_PORT: u16 = 4548;
const INF_REDIRECTS_PORT: u16 = 4549;
const REDIRECT_ABSOLUTE_PORT: u16 = 4550;
const AUTH_REDIRECT_PORT: u16 = 4551;
const TLS_CLIENT_AUTH_PORT: u16 = 4552;
const BASIC_AUTH_REDIRECT_PORT: u16 = 4554;
const TLS_PORT: u16 = 4557;
const HTTPS_PORT: u16 = 5545;
const H1_ONLY_TLS_PORT: u16 = 5546;
const H2_ONLY_TLS_PORT: u16 = 5547;
const H1_ONLY_PORT: u16 = 5548;
const H2_ONLY_PORT: u16 = 5549;
const HTTPS_CLIENT_AUTH_PORT: u16 = 5552;
const WS_PORT: u16 = 4242;
const WSS_PORT: u16 = 4243;
const WSS2_PORT: u16 = 4249;
const WS_CLOSE_PORT: u16 = 4244;
const WS_PING_PORT: u16 = 4245;
const H2_GRPC_PORT: u16 = 4246;
const H2S_GRPC_PORT: u16 = 4247;

pub const PERMISSION_VARIANTS: [&str; 5] =
  ["read", "write", "env", "net", "run"];
pub const PERMISSION_DENIED_PATTERN: &str = "PermissionDenied";

static GUARD: Lazy<Mutex<HttpServerCount>> =
  Lazy::new(|| Mutex::new(HttpServerCount::default()));

pub fn env_vars_for_npm_tests() -> Vec<(String, String)> {
  vec![
    ("NPM_CONFIG_REGISTRY".to_string(), npm_registry_url()),
    ("NO_COLOR".to_string(), "1".to_string()),
  ]
}

pub fn env_vars_for_jsr_tests() -> Vec<(String, String)> {
  vec![
    ("DENO_REGISTRY_URL".to_string(), jsr_registry_url()),
    ("NO_COLOR".to_string(), "1".to_string()),
  ]
}

pub fn root_path() -> PathRef {
  PathRef::new(
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR")))
      .parent()
      .unwrap(),
  )
}

pub fn prebuilt_path() -> PathRef {
  third_party_path().join("prebuilt")
}

pub fn tests_path() -> PathRef {
  root_path().join("cli").join("tests")
}

pub fn testdata_path() -> PathRef {
  tests_path().join("testdata")
}

pub fn third_party_path() -> PathRef {
  root_path().join("third_party")
}

pub fn napi_tests_path() -> PathRef {
  root_path().join("test_napi")
}

/// Test server registry url.
pub fn npm_registry_url() -> String {
  "http://localhost:4545/npm/registry/".to_string()
}

pub fn npm_registry_unset_url() -> String {
  "http://NPM_CONFIG_REGISTRY.is.unset".to_string()
}

pub fn jsr_registry_url() -> String {
  "http://localhost:4545/jsr/registry/".to_string()
}

pub fn std_path() -> PathRef {
  root_path().join("test_util").join("std")
}

pub fn std_file_url() -> String {
  Url::from_directory_path(std_path()).unwrap().to_string()
}

pub fn target_dir() -> PathRef {
  let current_exe = std::env::current_exe().unwrap();
  let target_dir = current_exe.parent().unwrap().parent().unwrap();
  PathRef::new(target_dir)
}

pub fn deno_exe_path() -> PathRef {
  // Something like /Users/rld/src/deno/target/debug/deps/deno
  let mut p = target_dir().join("deno").to_path_buf();
  if cfg!(windows) {
    p.set_extension("exe");
  }
  PathRef::new(p)
}

pub fn prebuilt_tool_path(tool: &str) -> PathRef {
  let mut exe = tool.to_string();
  exe.push_str(if cfg!(windows) { ".exe" } else { "" });
  prebuilt_path().join(platform_dir_name()).join(exe)
}

pub fn platform_dir_name() -> &'static str {
  if cfg!(target_os = "linux") {
    "linux64"
  } else if cfg!(target_os = "macos") {
    "mac"
  } else if cfg!(target_os = "windows") {
    "win"
  } else {
    unreachable!()
  }
}

pub fn test_server_path() -> PathBuf {
  let mut p = target_dir().join("test_server").to_path_buf();
  if cfg!(windows) {
    p.set_extension("exe");
  }
  p
}

fn ensure_test_server_built() {
  // if the test server doesn't exist then remind the developer to build first
  if !test_server_path().exists() {
    panic!(
      "Test server not found. Please cargo build before running the tests."
    );
  }
}

/// Benchmark server that just serves "hello world" responses.
async fn hyper_hello(port: u16) {
  println!("hyper hello");
  let addr = SocketAddr::from(([127, 0, 0, 1], port));
  let hello_svc = make_service_fn(|_| async move {
    Ok::<_, Infallible>(service_fn(move |_: Request<Body>| async move {
      Ok::<_, Infallible>(Response::new(Body::from("Hello World!")))
    }))
  });

  let server = Server::bind(&addr).serve(hello_svc);
  if let Err(e) = server.await {
    eprintln!("server error: {e}");
  }
}

fn redirect_resp(url: String) -> Response<Body> {
  let mut redirect_resp = Response::new(Body::empty());
  *redirect_resp.status_mut() = StatusCode::MOVED_PERMANENTLY;
  redirect_resp.headers_mut().insert(
    hyper::header::LOCATION,
    HeaderValue::from_str(&url[..]).unwrap(),
  );

  redirect_resp
}

async fn redirect(req: Request<Body>) -> hyper::Result<Response<Body>> {
  let p = req.uri().path();
  assert_eq!(&p[0..1], "/");
  let url = format!("http://localhost:{PORT}{p}");

  Ok(redirect_resp(url))
}

async fn double_redirects(req: Request<Body>) -> hyper::Result<Response<Body>> {
  let p = req.uri().path();
  assert_eq!(&p[0..1], "/");
  let url = format!("http://localhost:{REDIRECT_PORT}{p}");

  Ok(redirect_resp(url))
}

async fn inf_redirects(req: Request<Body>) -> hyper::Result<Response<Body>> {
  let p = req.uri().path();
  assert_eq!(&p[0..1], "/");
  let url = format!("http://localhost:{INF_REDIRECTS_PORT}{p}");

  Ok(redirect_resp(url))
}

async fn another_redirect(req: Request<Body>) -> hyper::Result<Response<Body>> {
  let p = req.uri().path();
  assert_eq!(&p[0..1], "/");
  let url = format!("http://localhost:{PORT}/subdir{p}");

  Ok(redirect_resp(url))
}

async fn auth_redirect(req: Request<Body>) -> hyper::Result<Response<Body>> {
  if let Some(auth) = req
    .headers()
    .get("authorization")
    .map(|v| v.to_str().unwrap())
  {
    if auth.to_lowercase() == format!("bearer {TEST_AUTH_TOKEN}") {
      let p = req.uri().path();
      assert_eq!(&p[0..1], "/");
      let url = format!("http://localhost:{PORT}{p}");
      return Ok(redirect_resp(url));
    }
  }

  let mut resp = Response::new(Body::empty());
  *resp.status_mut() = StatusCode::NOT_FOUND;
  Ok(resp)
}

async fn basic_auth_redirect(
  req: Request<Body>,
) -> hyper::Result<Response<Body>> {
  if let Some(auth) = req
    .headers()
    .get("authorization")
    .map(|v| v.to_str().unwrap())
  {
    let credentials =
      format!("{TEST_BASIC_AUTH_USERNAME}:{TEST_BASIC_AUTH_PASSWORD}");
    if auth == format!("Basic {}", BASE64_STANDARD.encode(credentials)) {
      let p = req.uri().path();
      assert_eq!(&p[0..1], "/");
      let url = format!("http://localhost:{PORT}{p}");
      return Ok(redirect_resp(url));
    }
  }

  let mut resp = Response::new(Body::empty());
  *resp.status_mut() = StatusCode::NOT_FOUND;
  Ok(resp)
}

async fn echo_websocket_handler(
  ws: fastwebsockets::WebSocket<Upgraded>,
) -> Result<(), anyhow::Error> {
  let mut ws = fastwebsockets::FragmentCollector::new(ws);

  loop {
    let frame = ws.read_frame().await.unwrap();
    match frame.opcode {
      fastwebsockets::OpCode::Close => break,
      fastwebsockets::OpCode::Text | fastwebsockets::OpCode::Binary => {
        ws.write_frame(frame).await.unwrap();
      }
      _ => {}
    }
  }

  Ok(())
}

type WsHandler =
  fn(
    fastwebsockets::WebSocket<Upgraded>,
  ) -> Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + Send>>;

fn spawn_ws_server<S>(stream: S, handler: WsHandler)
where
  S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
  let srv_fn = service_fn(move |mut req: Request<Body>| async move {
    let (response, upgrade_fut) = fastwebsockets::upgrade::upgrade(&mut req)
      .map_err(|e| anyhow!("Error upgrading websocket connection: {}", e))?;

    tokio::spawn(async move {
      let ws = upgrade_fut
        .await
        .map_err(|e| anyhow!("Error upgrading websocket connection: {}", e))
        .unwrap();

      if let Err(e) = handler(ws).await {
        eprintln!("Error in websocket connection: {}", e);
      }
    });

    Ok::<_, anyhow::Error>(response)
  });

  tokio::spawn(async move {
    let conn_fut = hyper::server::conn::Http::new()
      .serve_connection(stream, srv_fn)
      .with_upgrades();

    if let Err(e) = conn_fut.await {
      eprintln!("websocket server error: {e:?}");
    }
  });
}

async fn run_ws_server(port: u16) {
  let mut tcp = get_tcp_listener_stream("ws", port).await;
  while let Some(Ok(stream)) = tcp.next().await {
    spawn_ws_server(stream, |ws| Box::pin(echo_websocket_handler(ws)));
  }
}

async fn ping_websocket_handler(
  ws: fastwebsockets::WebSocket<Upgraded>,
) -> Result<(), anyhow::Error> {
  let mut ws = fastwebsockets::FragmentCollector::new(ws);

  for i in 0..9 {
    ws.write_frame(Frame::new(true, OpCode::Ping, None, vec![].into()))
      .await
      .unwrap();

    let frame = ws.read_frame().await.unwrap();
    assert_eq!(frame.opcode, OpCode::Pong);
    assert!(frame.payload.is_empty());

    ws.write_frame(Frame::text(
      format!("hello {}", i).as_bytes().to_vec().into(),
    ))
    .await
    .unwrap();

    let frame = ws.read_frame().await.unwrap();
    assert_eq!(frame.opcode, OpCode::Text);
    assert_eq!(frame.payload, format!("hello {}", i).as_bytes());
  }

  ws.write_frame(fastwebsockets::Frame::close(1000, b""))
    .await
    .unwrap();

  Ok(())
}

async fn run_ws_ping_server(port: u16) {
  let mut tcp = get_tcp_listener_stream("ws (ping)", port).await;
  while let Some(Ok(stream)) = tcp.next().await {
    spawn_ws_server(stream, |ws| Box::pin(ping_websocket_handler(ws)));
  }
}

async fn close_websocket_handler(
  ws: fastwebsockets::WebSocket<Upgraded>,
) -> Result<(), anyhow::Error> {
  let mut ws = fastwebsockets::FragmentCollector::new(ws);

  ws.write_frame(fastwebsockets::Frame::close_raw(vec![].into()))
    .await
    .unwrap();

  Ok(())
}

async fn run_ws_close_server(port: u16) {
  let mut tcp = get_tcp_listener_stream("ws (close)", port).await;
  while let Some(Ok(stream)) = tcp.next().await {
    spawn_ws_server(stream, |ws| Box::pin(close_websocket_handler(ws)));
  }
}

async fn handle_wss_stream(
  recv: Request<RecvStream>,
  mut send: SendResponse<Bytes>,
) -> Result<(), h2::Error> {
  if recv.method() != Method::CONNECT {
    eprintln!("wss2: refusing non-CONNECT stream");
    send.send_reset(Reason::REFUSED_STREAM);
    return Ok(());
  }
  let Some(protocol) = recv.extensions().get::<h2::ext::Protocol>() else {
    eprintln!("wss2: refusing no-:protocol stream");
    send.send_reset(Reason::REFUSED_STREAM);
    return Ok(());
  };
  if protocol.as_str() != "websocket" && protocol.as_str() != "WebSocket" {
    eprintln!("wss2: refusing non-websocket stream");
    send.send_reset(Reason::REFUSED_STREAM);
    return Ok(());
  }
  let mut body = recv.into_body();
  let mut response = Response::new(());
  *response.status_mut() = StatusCode::OK;
  let mut resp = send.send_response(response, false)?;
  // Use a duplex stream to talk to fastwebsockets because it's just faster to implement
  let (a, b) = tokio::io::duplex(65536);
  let f1 = tokio::spawn(tokio::task::unconstrained(async move {
    let ws = WebSocket::after_handshake(a, Role::Server);
    let mut ws = FragmentCollector::new(ws);
    loop {
      let frame = ws.read_frame().await.unwrap();
      if frame.opcode == OpCode::Close {
        break;
      }
      ws.write_frame(frame).await.unwrap();
    }
  }));
  let (mut br, mut bw) = tokio::io::split(b);
  let f2 = tokio::spawn(tokio::task::unconstrained(async move {
    loop {
      let Some(Ok(data)) = poll_fn(|cx| body.poll_data(cx)).await else {
        return;
      };
      body.flow_control().release_capacity(data.len()).unwrap();
      let Ok(_) = bw.write_all(&data).await else {
        break;
      };
    }
  }));
  let f3 = tokio::spawn(tokio::task::unconstrained(async move {
    loop {
      let mut buf = [0; 65536];
      let n = br.read(&mut buf).await.unwrap();
      if n == 0 {
        break;
      }
      resp.reserve_capacity(n);
      poll_fn(|cx| resp.poll_capacity(cx)).await;
      resp
        .send_data(Bytes::copy_from_slice(&buf[0..n]), false)
        .unwrap();
    }
    resp.send_data(Bytes::new(), true).unwrap();
  }));
  _ = join3(f1, f2, f3).await;
  Ok(())
}

async fn run_wss2_server(port: u16) {
  let mut tls = get_tls_listener_stream(
    "wss2 (tls)",
    port,
    SupportedHttpVersions::Http2Only,
  )
  .await;
  while let Some(Ok(tls)) = tls.next().await {
    tokio::spawn(async move {
      let mut h2 = h2::server::Builder::new();
      h2.enable_connect_protocol();
      // Using Bytes is pretty alloc-heavy but this is a test server
      let server: Handshake<_, Bytes> = h2.handshake(tls);
      let mut server = match server.await {
        Ok(server) => server,
        Err(e) => {
          println!("Failed to handshake h2: {e:?}");
          return;
        }
      };
      loop {
        let Some(conn) = server.accept().await else {
          break;
        };
        let (recv, send) = match conn {
          Ok(conn) => conn,
          Err(e) => {
            println!("Failed to accept a connection: {e:?}");
            break;
          }
        };
        tokio::spawn(handle_wss_stream(recv, send));
      }
    });
  }
}

async fn run_wss_server(port: u16) {
  let mut tls = get_tls_listener_stream("wss", port, Default::default()).await;
  while let Some(Ok(tls_stream)) = tls.next().await {
    tokio::spawn(async move {
      spawn_ws_server(tls_stream, |ws| Box::pin(echo_websocket_handler(ws)));
    });
  }
}

/// Returns a [`Stream`] of [`TcpStream`]s accepted from the given port.
async fn get_tcp_listener_stream(
  name: &'static str,
  port: u16,
) -> impl Stream<Item = Result<TcpStream, std::io::Error>> + Unpin + Send {
  let host_and_port = &format!("localhost:{port}");

  // Listen on ALL addresses that localhost can resolves to.
  let accept = |listener: tokio::net::TcpListener| {
    async {
      let result = listener.accept().await;
      Some((result.map(|r| r.0), listener))
    }
    .boxed()
  };

  let mut addresses = vec![];
  let listeners = tokio::net::lookup_host(host_and_port)
    .await
    .expect(host_and_port)
    .inspect(|address| addresses.push(*address))
    .map(tokio::net::TcpListener::bind)
    .collect::<futures::stream::FuturesUnordered<_>>()
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .map(|s| s.unwrap())
    .map(|listener| futures::stream::unfold(listener, accept))
    .collect::<Vec<_>>();

  // Eye catcher for HttpServerCount
  println!("ready: {name} on {:?}", addresses);

  futures::stream::select_all(listeners)
}

/// This server responds with 'PASS' if client authentication was successful. Try it by running
/// test_server and
///   curl --key cli/tests/testdata/tls/localhost.key \
///        --cert cli/tests/testsdata/tls/localhost.crt \
///        --cacert cli/tests/testdata/tls/RootCA.crt https://localhost:4552/
async fn run_tls_client_auth_server() {
  let mut tls = get_tls_listener_stream(
    "tls client auth",
    TLS_CLIENT_AUTH_PORT,
    Default::default(),
  )
  .await;
  while let Some(Ok(mut tls_stream)) = tls.next().await {
    tokio::spawn(async move {
      let Ok(handshake) = tls_stream.handshake().await else {
        eprintln!("Failed to handshake");
        return;
      };
      // We only need to check for the presence of client certificates
      // here. Rusttls ensures that they are valid and signed by the CA.
      let response = match handshake.has_peer_certificates {
        true => b"PASS",
        false => b"FAIL",
      };
      tls_stream.write_all(response).await.unwrap();
    });
  }
}

/// This server responds with 'PASS' if client authentication was successful. Try it by running
/// test_server and
///   curl --cacert cli/tests/testdata/tls/RootCA.crt https://localhost:4553/
async fn run_tls_server() {
  let mut tls =
    get_tls_listener_stream("tls", TLS_PORT, Default::default()).await;
  while let Some(Ok(mut tls_stream)) = tls.next().await {
    tokio::spawn(async move {
      tls_stream.write_all(b"PASS").await.unwrap();
    });
  }
}

async fn absolute_redirect(
  req: Request<Body>,
) -> hyper::Result<Response<Body>> {
  let path = req.uri().path();

  if path == "/" {
    // We have to manually extract query params here,
    // as `req.uri()` returns `PathAndQuery` only,
    // and we cannot use `Url::parse(req.uri()).query_pairs()`,
    // as it requires url to have a proper base.
    let query_params: HashMap<_, _> = req
      .uri()
      .query()
      .unwrap_or_default()
      .split('&')
      .filter_map(|s| {
        s.split_once('=').map(|t| (t.0.to_owned(), t.1.to_owned()))
      })
      .collect();

    if let Some(url) = query_params.get("redirect_to") {
      println!("URL: {url:?}");
      let redirect = redirect_resp(url.to_owned());
      return Ok(redirect);
    }
  }

  if path.starts_with("/REDIRECT") {
    let url = &req.uri().path()[9..];
    println!("URL: {url:?}");
    let redirect = redirect_resp(url.to_string());
    return Ok(redirect);
  }

  if path.starts_with("/a/b/c") {
    if let Some(x_loc) = req.headers().get("x-location") {
      let loc = x_loc.to_str().unwrap();
      return Ok(redirect_resp(loc.to_string()));
    }
  }

  let file_path = testdata_path().join(&req.uri().path()[1..]);
  if file_path.is_dir() || !file_path.exists() {
    let mut not_found_resp = Response::new(Body::empty());
    *not_found_resp.status_mut() = StatusCode::NOT_FOUND;
    return Ok(not_found_resp);
  }

  let file = tokio::fs::read(file_path).await.unwrap();
  let file_resp = custom_headers(req.uri().path(), file);
  Ok(file_resp)
}

async fn main_server(
  req: Request<Body>,
) -> Result<Response<Body>, hyper::http::Error> {
  return match (req.method(), req.uri().path()) {
    (_, "/echo_server") => {
      let (parts, body) = req.into_parts();
      let mut response = Response::new(body);

      if let Some(status) = parts.headers.get("x-status") {
        *response.status_mut() =
          StatusCode::from_bytes(status.as_bytes()).unwrap();
      }
      response.headers_mut().extend(parts.headers);
      Ok(response)
    }
    (&hyper::Method::POST, "/echo_multipart_file") => {
      let body = req.into_body();
      let bytes = &hyper::body::to_bytes(body).await.unwrap()[0..];
      let start = b"--boundary\t \r\n\
                    Content-Disposition: form-data; name=\"field_1\"\r\n\
                    \r\n\
                    value_1 \r\n\
                    \r\n--boundary\r\n\
                    Content-Disposition: form-data; name=\"file\"; \
                    filename=\"file.bin\"\r\n\
                    Content-Type: application/octet-stream\r\n\
                    \r\n";
      let end = b"\r\n--boundary--\r\n";
      let b = [start as &[u8], bytes, end].concat();

      let mut response = Response::new(Body::from(b));
      response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("multipart/form-data;boundary=boundary"),
      );
      Ok(response)
    }
    (_, "/multipart_form_data.txt") => {
      let b = "Preamble\r\n\
             --boundary\t \r\n\
             Content-Disposition: form-data; name=\"field_1\"\r\n\
             \r\n\
             value_1 \r\n\
             \r\n--boundary\r\n\
             Content-Disposition: form-data; name=\"field_2\";\
             filename=\"file.js\"\r\n\
             Content-Type: text/javascript\r\n\
             \r\n\
             console.log(\"Hi\")\
             \r\n--boundary--\r\n\
             Epilogue";
      let mut res = Response::new(Body::from(b));
      res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("multipart/form-data;boundary=boundary"),
      );
      Ok(res)
    }
    (_, "/multipart_form_bad_content_type") => {
      let b = "Preamble\r\n\
             --boundary\t \r\n\
             Content-Disposition: form-data; name=\"field_1\"\r\n\
             \r\n\
             value_1 \r\n\
             \r\n--boundary\r\n\
             Content-Disposition: form-data; name=\"field_2\";\
             filename=\"file.js\"\r\n\
             Content-Type: text/javascript\r\n\
             \r\n\
             console.log(\"Hi\")\
             \r\n--boundary--\r\n\
             Epilogue";
      let mut res = Response::new(Body::from(b));
      res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("multipart/form-datatststs;boundary=boundary"),
      );
      Ok(res)
    }
    (_, "/bad_redirect") => {
      let mut res = Response::new(Body::empty());
      *res.status_mut() = StatusCode::FOUND;
      Ok(res)
    }
    (_, "/server_error") => {
      let mut res = Response::new(Body::empty());
      *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
      Ok(res)
    }
    (_, "/x_deno_warning.js") => {
      let mut res = Response::new(Body::empty());
      *res.status_mut() = StatusCode::MOVED_PERMANENTLY;
      res
        .headers_mut()
        .insert("X-Deno-Warning", HeaderValue::from_static("foobar"));
      res.headers_mut().insert(
        "location",
        HeaderValue::from_bytes(b"/lsp/x_deno_warning_redirect.js").unwrap(),
      );
      Ok(res)
    }
    (_, "/non_ascii_redirect") => {
      let mut res = Response::new(Body::empty());
      *res.status_mut() = StatusCode::MOVED_PERMANENTLY;
      res.headers_mut().insert(
        "location",
        HeaderValue::from_bytes(b"/redirect\xae").unwrap(),
      );
      Ok(res)
    }
    (_, "/etag_script.ts") => {
      let if_none_match = req.headers().get("if-none-match");
      if if_none_match == Some(&HeaderValue::from_static("33a64df551425fcc55e"))
      {
        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::NOT_MODIFIED;
        resp.headers_mut().insert(
          "Content-type",
          HeaderValue::from_static("application/typescript"),
        );
        resp
          .headers_mut()
          .insert("ETag", HeaderValue::from_static("33a64df551425fcc55e"));

        Ok(resp)
      } else {
        let mut resp = Response::new(Body::from("console.log('etag')"));
        resp.headers_mut().insert(
          "Content-type",
          HeaderValue::from_static("application/typescript"),
        );
        resp
          .headers_mut()
          .insert("ETag", HeaderValue::from_static("33a64df551425fcc55e"));
        Ok(resp)
      }
    }
    (_, "/xTypeScriptTypes.js") => {
      let mut res = Response::new(Body::from("export const foo = 'foo';"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/javascript"),
      );
      res.headers_mut().insert(
        "X-TypeScript-Types",
        HeaderValue::from_static("./xTypeScriptTypes.d.ts"),
      );
      Ok(res)
    }
    (_, "/xTypeScriptTypes.jsx") => {
      let mut res = Response::new(Body::from("export const foo = 'foo';"));
      res
        .headers_mut()
        .insert("Content-type", HeaderValue::from_static("text/jsx"));
      res.headers_mut().insert(
        "X-TypeScript-Types",
        HeaderValue::from_static("./xTypeScriptTypes.d.ts"),
      );
      Ok(res)
    }
    (_, "/xTypeScriptTypes.ts") => {
      let mut res =
        Response::new(Body::from("export const foo: string = 'foo';"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      res.headers_mut().insert(
        "X-TypeScript-Types",
        HeaderValue::from_static("./xTypeScriptTypes.d.ts"),
      );
      Ok(res)
    }
    (_, "/xTypeScriptTypes.d.ts") => {
      let mut res = Response::new(Body::from("export const foo: 'foo';"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/run/type_directives_redirect.js") => {
      let mut res = Response::new(Body::from("export const foo = 'foo';"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/javascript"),
      );
      res.headers_mut().insert(
        "X-TypeScript-Types",
        HeaderValue::from_static(
          "http://localhost:4547/xTypeScriptTypesRedirect.d.ts",
        ),
      );
      Ok(res)
    }
    (_, "/run/type_headers_deno_types.foo.js") => {
      let mut res = Response::new(Body::from(
        "export function foo(text) { console.log(text); }",
      ));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/javascript"),
      );
      res.headers_mut().insert(
        "X-TypeScript-Types",
        HeaderValue::from_static(
          "http://localhost:4545/run/type_headers_deno_types.d.ts",
        ),
      );
      Ok(res)
    }
    (_, "/run/type_headers_deno_types.d.ts") => {
      let mut res =
        Response::new(Body::from("export function foo(text: number): void;"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/run/type_headers_deno_types.foo.d.ts") => {
      let mut res =
        Response::new(Body::from("export function foo(text: string): void;"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/subdir/xTypeScriptTypesRedirect.d.ts") => {
      let mut res = Response::new(Body::from(
        "import './xTypeScriptTypesRedirected.d.ts';",
      ));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/subdir/xTypeScriptTypesRedirected.d.ts") => {
      let mut res = Response::new(Body::from("export const foo: 'foo';"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/referenceTypes.js") => {
      let mut res = Response::new(Body::from("/// <reference types=\"./xTypeScriptTypes.d.ts\" />\r\nexport const foo = \"foo\";\r\n"));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/javascript"),
      );
      Ok(res)
    }
    (_, "/subdir/file_with_:_in_name.ts") => {
      let mut res = Response::new(Body::from(
        "console.log('Hello from file_with_:_in_name.ts');",
      ));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/v1/extensionless") => {
      let mut res =
        Response::new(Body::from(r#"export * from "/subdir/mod1.ts";"#));
      res.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/subdir/no_js_ext@1.0.0") => {
      let mut res = Response::new(Body::from(
        r#"import { printHello } from "./mod2.ts";
        printHello();
        "#,
      ));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/javascript"),
      );
      Ok(res)
    }
    (_, "/.well-known/deno-import-intellisense.json") => {
      let file_path =
        testdata_path().join("lsp/registries/deno-import-intellisense.json");
      if let Ok(body) = tokio::fs::read(file_path).await {
        Ok(custom_headers(
          "/.well-known/deno-import-intellisense.json",
          body,
        ))
      } else {
        Ok(Response::new(Body::empty()))
      }
    }
    (_, "/http_version") => {
      let version = format!("{:?}", req.version());
      Ok(Response::new(version.into()))
    }
    (_, "/content_length") => {
      let content_length = format!("{:?}", req.headers().get("content-length"));
      Ok(Response::new(content_length.into()))
    }
    (_, "/jsx/jsx-runtime") | (_, "/jsx/jsx-dev-runtime") => {
      let mut res = Response::new(Body::from(
        r#"export function jsx(
          _type,
          _props,
          _key,
          _source,
          _self,
        ) {}
        export const jsxs = jsx;
        export const jsxDEV = jsx;
        export const Fragment = Symbol("Fragment");
        console.log("imported", import.meta.url);
        "#,
      ));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/javascript"),
      );
      Ok(res)
    }
    (_, "/dynamic") => {
      let mut res = Response::new(Body::from(
        serde_json::to_string_pretty(&std::time::SystemTime::now()).unwrap(),
      ));
      res
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-cache"));
      Ok(res)
    }
    (_, "/dynamic_cache") => {
      let mut res = Response::new(Body::from(
        serde_json::to_string_pretty(&std::time::SystemTime::now()).unwrap(),
      ));
      res.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("public, max-age=604800, immutable"),
      );
      Ok(res)
    }
    (_, "/dynamic_module.ts") => {
      let mut res = Response::new(Body::from(format!(
        r#"export const time = {};"#,
        std::time::SystemTime::now().elapsed().unwrap().as_nanos()
      )));
      res.headers_mut().insert(
        "Content-type",
        HeaderValue::from_static("application/typescript"),
      );
      Ok(res)
    }
    (_, "/echo_accept") => {
      let accept = req.headers().get("accept").map(|v| v.to_str().unwrap());
      let res = Response::new(Body::from(
        serde_json::json!({ "accept": accept }).to_string(),
      ));
      Ok(res)
    }
    (_, "/search_params") => {
      let query = req.uri().query().map(|s| s.to_string());
      let res = Response::new(Body::from(query.unwrap_or_default()));
      Ok(res)
    }
    (&hyper::Method::POST, "/kv_remote_authorize") => {
      if req
        .headers()
        .get("authorization")
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        != format!("Bearer {}", KV_ACCESS_TOKEN)
      {
        return Ok(
          Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .unwrap(),
        );
      }

      Ok(
        Response::builder()
          .header("content-type", "application/json")
          .body(Body::from(
            serde_json::json!({
              "version": 1,
              "databaseId": KV_DATABASE_ID,
              "endpoints": [
                {
                  "url": format!("http://localhost:{}/kv_blackhole", PORT),
                  "consistency": "strong",
                }
              ],
              "token": KV_DATABASE_TOKEN,
              "expiresAt": "2099-01-01T00:00:00Z",
            })
            .to_string(),
          ))
          .unwrap(),
      )
    }
    (&hyper::Method::POST, "/kv_remote_authorize_invalid_format") => {
      if req
        .headers()
        .get("authorization")
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        != format!("Bearer {}", KV_ACCESS_TOKEN)
      {
        return Ok(
          Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .unwrap(),
        );
      }

      Ok(
        Response::builder()
          .header("content-type", "application/json")
          .body(Body::from(
            serde_json::json!({
              "version": 1,
              "databaseId": KV_DATABASE_ID,
            })
            .to_string(),
          ))
          .unwrap(),
      )
    }
    (&hyper::Method::POST, "/kv_remote_authorize_invalid_version") => {
      if req
        .headers()
        .get("authorization")
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        != format!("Bearer {}", KV_ACCESS_TOKEN)
      {
        return Ok(
          Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .unwrap(),
        );
      }

      Ok(
        Response::builder()
          .header("content-type", "application/json")
          .body(Body::from(
            serde_json::json!({
              "version": 1000,
              "databaseId": KV_DATABASE_ID,
              "endpoints": [
                {
                  "url": format!("http://localhost:{}/kv_blackhole", PORT),
                  "consistency": "strong",
                }
              ],
              "token": KV_DATABASE_TOKEN,
              "expiresAt": "2099-01-01T00:00:00Z",
            })
            .to_string(),
          ))
          .unwrap(),
      )
    }
    (&hyper::Method::POST, "/kv_blackhole/snapshot_read") => {
      if req
        .headers()
        .get("authorization")
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        != format!("Bearer {}", KV_DATABASE_TOKEN)
      {
        return Ok(
          Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .unwrap(),
        );
      }

      let body = hyper::body::to_bytes(req.into_body())
        .await
        .unwrap_or_default();
      let Ok(body): Result<SnapshotRead, _> = prost::Message::decode(&body[..])
      else {
        return Ok(
          Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap(),
        );
      };
      if body.ranges.is_empty() {
        return Ok(
          Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap(),
        );
      }
      Ok(
        Response::builder()
          .body(Body::from(
            SnapshotReadOutput {
              ranges: body
                .ranges
                .iter()
                .map(|_| ReadRangeOutput { values: vec![] })
                .collect(),
              read_disabled: false,
              read_is_strongly_consistent: true,
            }
            .encode_to_vec(),
          ))
          .unwrap(),
      )
    }
    (&hyper::Method::POST, "/kv_blackhole/atomic_write") => {
      if req
        .headers()
        .get("authorization")
        .and_then(|x| x.to_str().ok())
        .unwrap_or_default()
        != format!("Bearer {}", KV_DATABASE_TOKEN)
      {
        return Ok(
          Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .unwrap(),
        );
      }

      let body = hyper::body::to_bytes(req.into_body())
        .await
        .unwrap_or_default();
      let Ok(_body): Result<AtomicWrite, _> = prost::Message::decode(&body[..])
      else {
        return Ok(
          Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap(),
        );
      };
      Ok(
        Response::builder()
          .body(Body::from(
            AtomicWriteOutput {
              status: AtomicWriteStatus::AwSuccess.into(),
              versionstamp: vec![0u8; 10],
              failed_checks: vec![],
            }
            .encode_to_vec(),
          ))
          .unwrap(),
      )
    }
    (&hyper::Method::GET, "/upgrade/sleep/release-latest.txt") => {
      tokio::time::sleep(Duration::from_secs(45)).await;
      return Ok(
        Response::builder()
          .status(StatusCode::OK)
          .body(Body::from("99999.99.99"))
          .unwrap(),
      );
    }
    (&hyper::Method::GET, "/upgrade/sleep/canary-latest.txt") => {
      tokio::time::sleep(Duration::from_secs(45)).await;
      return Ok(
        Response::builder()
          .status(StatusCode::OK)
          .body(Body::from("bda3850f84f24b71e02512c1ba2d6bf2e3daa2fd"))
          .unwrap(),
      );
    }
    (&hyper::Method::GET, "/release-latest.txt") => {
      return Ok(
        Response::builder()
          .status(StatusCode::OK)
          // use a deno version that will never happen
          .body(Body::from("99999.99.99"))
          .unwrap(),
      );
    }
    (&hyper::Method::GET, "/canary-latest.txt") => {
      return Ok(
        Response::builder()
          .status(StatusCode::OK)
          .body(Body::from("bda3850f84f24b71e02512c1ba2d6bf2e3daa2fd"))
          .unwrap(),
      );
    }
    _ => {
      let mut file_path = testdata_path().to_path_buf();
      file_path.push(&req.uri().path()[1..].replace("%2f", "/"));
      if let Ok(file) = tokio::fs::read(&file_path).await {
        let file_resp = custom_headers(req.uri().path(), file);
        return Ok(file_resp);
      }

      // serve npm registry files
      if let Some(suffix) = req
        .uri()
        .path()
        .strip_prefix("/npm/registry/@denotest/")
        .or_else(|| req.uri().path().strip_prefix("/npm/registry/@denotest%2f"))
      {
        // serve all requests to /npm/registry/@deno using the file system
        // at that path
        match handle_custom_npm_registry_path(suffix) {
          Ok(Some(response)) => return Ok(response),
          Ok(None) => {} // ignore, not found
          Err(err) => {
            return Response::builder()
              .status(StatusCode::INTERNAL_SERVER_ERROR)
              .body(format!("{err:#}").into());
          }
        }
      } else if req.uri().path().starts_with("/npm/registry/") {
        // otherwise, serve based on registry.json and tgz files
        let is_tarball = req.uri().path().ends_with(".tgz");
        if !is_tarball {
          file_path.push("registry.json");
        }
        if let Ok(file) = tokio::fs::read(&file_path).await {
          let file_resp = custom_headers(req.uri().path(), file);
          return Ok(file_resp);
        } else if should_download_npm_packages() {
          if let Err(err) =
            download_npm_registry_file(req.uri(), &file_path, is_tarball).await
          {
            return Response::builder()
              .status(StatusCode::INTERNAL_SERVER_ERROR)
              .body(format!("{err:#}").into());
          };

          // serve the file
          if let Ok(file) = tokio::fs::read(&file_path).await {
            let file_resp = custom_headers(req.uri().path(), file);
            return Ok(file_resp);
          }
        }
      } else if let Some(suffix) = req.uri().path().strip_prefix("/deno_std/") {
        let file_path = std_path().join(suffix);
        if let Ok(file) = tokio::fs::read(&file_path).await {
          let file_resp = custom_headers(req.uri().path(), file);
          return Ok(file_resp);
        }
      } else if let Some(suffix) = req.uri().path().strip_prefix("/sleep/") {
        let duration = suffix.parse::<u64>().unwrap();
        tokio::time::sleep(Duration::from_millis(duration)).await;
        return Response::builder()
          .status(StatusCode::OK)
          .header("content-type", "application/typescript")
          .body(Body::empty());
      }

      Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
    }
  };
}

fn handle_custom_npm_registry_path(
  path: &str,
) -> Result<Option<Response<Body>>, anyhow::Error> {
  let parts = path
    .split('/')
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>();
  let cache = &CUSTOM_NPM_PACKAGE_CACHE;
  let package_name = format!("@denotest/{}", parts[0]);
  if parts.len() == 2 {
    if let Some(file_bytes) =
      cache.tarball_bytes(&package_name, parts[1].trim_end_matches(".tgz"))?
    {
      let file_resp = custom_headers("file.tgz", file_bytes);
      return Ok(Some(file_resp));
    }
  } else if parts.len() == 1 {
    if let Some(registry_file) = cache.registry_file(&package_name)? {
      let file_resp = custom_headers("registry.json", registry_file);
      return Ok(Some(file_resp));
    }
  }

  Ok(None)
}

fn should_download_npm_packages() -> bool {
  // when this env var is set, it will download and save npm packages
  // to the testdata/npm/registry directory
  std::env::var("DENO_TEST_UTIL_UPDATE_NPM") == Ok("1".to_string())
}

async fn download_npm_registry_file(
  uri: &hyper::Uri,
  file_path: &PathBuf,
  is_tarball: bool,
) -> Result<(), anyhow::Error> {
  let url_parts = uri
    .path()
    .strip_prefix("/npm/registry/")
    .unwrap()
    .split('/')
    .collect::<Vec<_>>();
  let package_name = if url_parts[0].starts_with('@') {
    url_parts.into_iter().take(2).collect::<Vec<_>>().join("/")
  } else {
    url_parts.into_iter().take(1).collect::<Vec<_>>().join("/")
  };
  let url = if is_tarball {
    let file_name = file_path.file_name().unwrap().to_string_lossy();
    format!("https://registry.npmjs.org/{package_name}/-/{file_name}")
  } else {
    format!("https://registry.npmjs.org/{package_name}")
  };
  let client = reqwest::Client::new();
  let response = client.get(url).send().await?;
  let bytes = response.bytes().await?;
  let bytes = if is_tarball {
    bytes.to_vec()
  } else {
    String::from_utf8(bytes.to_vec())
      .unwrap()
      .replace(
        &format!("https://registry.npmjs.org/{package_name}/-/"),
        &format!("http://localhost:4545/npm/registry/{package_name}/"),
      )
      .into_bytes()
  };
  std::fs::create_dir_all(file_path.parent().unwrap())?;
  std::fs::write(file_path, bytes)?;
  Ok(())
}

/// Taken from example in https://github.com/ctz/hyper-rustls/blob/a02ef72a227dcdf102f86e905baa7415c992e8b3/examples/server.rs
struct HyperAcceptor<'a> {
  acceptor: Pin<
    Box<dyn Stream<Item = io::Result<rustls_tokio_stream::TlsStream>> + 'a>,
  >,
}

impl hyper::server::accept::Accept for HyperAcceptor<'_> {
  type Conn = rustls_tokio_stream::TlsStream;
  type Error = io::Error;

  fn poll_accept(
    mut self: Pin<&mut Self>,
    cx: &mut Context,
  ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
    Pin::new(&mut self.acceptor).poll_next(cx)
  }
}

#[allow(clippy::non_send_fields_in_send_ty)]
// SAFETY: unsafe trait must have unsafe implementation
unsafe impl std::marker::Send for HyperAcceptor<'_> {}

async fn wrap_redirect_server() {
  let redirect_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(redirect)) });
  let redirect_addr = SocketAddr::from(([127, 0, 0, 1], REDIRECT_PORT));
  let redirect_server = Server::bind(&redirect_addr).serve(redirect_svc);
  if let Err(e) = redirect_server.await {
    eprintln!("Redirect error: {e:?}");
  }
}

async fn wrap_double_redirect_server() {
  let double_redirects_svc = make_service_fn(|_| async {
    Ok::<_, Infallible>(service_fn(double_redirects))
  });
  let double_redirects_addr =
    SocketAddr::from(([127, 0, 0, 1], DOUBLE_REDIRECTS_PORT));
  let double_redirects_server =
    Server::bind(&double_redirects_addr).serve(double_redirects_svc);
  if let Err(e) = double_redirects_server.await {
    eprintln!("Double redirect error: {e:?}");
  }
}

async fn wrap_inf_redirect_server() {
  let inf_redirects_svc = make_service_fn(|_| async {
    Ok::<_, Infallible>(service_fn(inf_redirects))
  });
  let inf_redirects_addr =
    SocketAddr::from(([127, 0, 0, 1], INF_REDIRECTS_PORT));
  let inf_redirects_server =
    Server::bind(&inf_redirects_addr).serve(inf_redirects_svc);
  if let Err(e) = inf_redirects_server.await {
    eprintln!("Inf redirect error: {e:?}");
  }
}

async fn wrap_another_redirect_server() {
  let another_redirect_svc = make_service_fn(|_| async {
    Ok::<_, Infallible>(service_fn(another_redirect))
  });
  let another_redirect_addr =
    SocketAddr::from(([127, 0, 0, 1], ANOTHER_REDIRECT_PORT));
  let another_redirect_server =
    Server::bind(&another_redirect_addr).serve(another_redirect_svc);
  if let Err(e) = another_redirect_server.await {
    eprintln!("Another redirect error: {e:?}");
  }
}

async fn wrap_auth_redirect_server() {
  let auth_redirect_svc = make_service_fn(|_| async {
    Ok::<_, Infallible>(service_fn(auth_redirect))
  });
  let auth_redirect_addr =
    SocketAddr::from(([127, 0, 0, 1], AUTH_REDIRECT_PORT));
  let auth_redirect_server =
    Server::bind(&auth_redirect_addr).serve(auth_redirect_svc);
  if let Err(e) = auth_redirect_server.await {
    eprintln!("Auth redirect error: {e:?}");
  }
}

async fn wrap_basic_auth_redirect_server() {
  let basic_auth_redirect_svc = make_service_fn(|_| async {
    Ok::<_, Infallible>(service_fn(basic_auth_redirect))
  });
  let basic_auth_redirect_addr =
    SocketAddr::from(([127, 0, 0, 1], BASIC_AUTH_REDIRECT_PORT));
  let basic_auth_redirect_server =
    Server::bind(&basic_auth_redirect_addr).serve(basic_auth_redirect_svc);
  if let Err(e) = basic_auth_redirect_server.await {
    eprintln!("Basic auth redirect error: {e:?}");
  }
}

async fn wrap_abs_redirect_server() {
  let abs_redirect_svc = make_service_fn(|_| async {
    Ok::<_, Infallible>(service_fn(absolute_redirect))
  });
  let abs_redirect_addr =
    SocketAddr::from(([127, 0, 0, 1], REDIRECT_ABSOLUTE_PORT));
  let abs_redirect_server =
    Server::bind(&abs_redirect_addr).serve(abs_redirect_svc);
  if let Err(e) = abs_redirect_server.await {
    eprintln!("Absolute redirect error: {e:?}");
  }
}

async fn wrap_main_server() {
  let main_server_addr = SocketAddr::from(([127, 0, 0, 1], PORT));
  wrap_main_server_for_addr(&main_server_addr).await
}

// necessary because on Windows the npm binary will resolve localhost to ::1
async fn wrap_main_ipv6_server() {
  let ipv6_loopback = Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1);
  let main_server_addr =
    SocketAddr::V6(SocketAddrV6::new(ipv6_loopback, PORT, 0, 0));
  wrap_main_server_for_addr(&main_server_addr).await
}

async fn wrap_main_server_for_addr(main_server_addr: &SocketAddr) {
  let main_server_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server = Server::bind(main_server_addr).serve(main_server_svc);
  if let Err(e) = main_server.await {
    eprintln!("HTTP server error: {e:?}");
  }
}

async fn wrap_main_https_server() {
  let tls =
    get_tls_listener_stream("https", HTTPS_PORT, Default::default()).await;
  let main_server_https_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server_https = Server::builder(HyperAcceptor {
    acceptor: tls.boxed_local(),
  })
  .serve(main_server_https_svc);
  let _ = main_server_https.await;
}

async fn wrap_https_h1_only_tls_server() {
  let tls = get_tls_listener_stream(
    "https (h1 only)",
    H1_ONLY_TLS_PORT,
    SupportedHttpVersions::Http1Only,
  )
  .await;

  let main_server_https_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server_https = Server::builder(HyperAcceptor {
    acceptor: tls.boxed_local(),
  })
  .http1_only(true)
  .serve(main_server_https_svc);

  let _ = main_server_https.await;
}

async fn wrap_https_h2_only_tls_server() {
  let tls = get_tls_listener_stream(
    "https (h2 only)",
    H2_ONLY_TLS_PORT,
    SupportedHttpVersions::Http2Only,
  )
  .await;

  let main_server_https_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server_https = Server::builder(HyperAcceptor {
    acceptor: tls.boxed_local(),
  })
  .http2_only(true)
  .serve(main_server_https_svc);

  let _ = main_server_https.await;
}

async fn wrap_http_h1_only_server() {
  let main_server_http_addr = SocketAddr::from(([127, 0, 0, 1], H1_ONLY_PORT));

  let main_server_http_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server_http = Server::bind(&main_server_http_addr)
    .http1_only(true)
    .serve(main_server_http_svc);
  let _ = main_server_http.await;
}

async fn wrap_http_h2_only_server() {
  let main_server_http_addr = SocketAddr::from(([127, 0, 0, 1], H2_ONLY_PORT));

  let main_server_http_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server_http = Server::bind(&main_server_http_addr)
    .http2_only(true)
    .serve(main_server_http_svc);
  let _ = main_server_http.await;
}

async fn h2_grpc_server() {
  let mut tcp = get_tcp_listener_stream("grpc", H2_GRPC_PORT).await;
  let mut tls = get_tls_listener_stream(
    "grpc (tls)",
    H2S_GRPC_PORT,
    SupportedHttpVersions::Http2Only,
  )
  .await;

  async fn serve(socket: TcpStream) -> Result<(), anyhow::Error> {
    let mut connection = h2::server::handshake(socket).await?;

    while let Some(result) = connection.accept().await {
      let (request, respond) = result?;
      tokio::spawn(async move {
        let _ = handle_request(request, respond).await;
      });
    }

    Ok(())
  }

  async fn serve_tls(socket: TlsStream) -> Result<(), anyhow::Error> {
    let mut connection = h2::server::handshake(socket).await?;

    while let Some(result) = connection.accept().await {
      let (request, respond) = result?;
      tokio::spawn(async move {
        let _ = handle_request(request, respond).await;
      });
    }

    Ok(())
  }

  async fn handle_request(
    mut request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<bytes::Bytes>,
  ) -> Result<(), anyhow::Error> {
    let body = request.body_mut();
    while let Some(data) = body.data().await {
      let data = data?;
      let _ = body.flow_control().release_capacity(data.len());
    }

    let maybe_recv_trailers = body.trailers().await?;

    let response = http::Response::new(());
    let mut send = respond.send_response(response, false)?;
    send.send_data(bytes::Bytes::from_static(b"hello "), false)?;
    send.send_data(bytes::Bytes::from_static(b"world\n"), false)?;
    let mut trailers = http::HeaderMap::new();
    trailers.insert(
      http::HeaderName::from_static("abc"),
      HeaderValue::from_static("def"),
    );
    trailers.insert(
      http::HeaderName::from_static("opr"),
      HeaderValue::from_static("stv"),
    );
    if let Some(recv_trailers) = maybe_recv_trailers {
      for (key, value) in recv_trailers {
        trailers.insert(key.unwrap(), value);
      }
    }
    send.send_trailers(trailers)?;

    Ok(())
  }

  let local_set = LocalSet::new();
  local_set.spawn_local(async move {
    while let Some(Ok(tcp)) = tcp.next().await {
      tokio::spawn(async move {
        let _ = serve(tcp).await;
      });
    }
  });

  local_set.spawn_local(async move {
    while let Some(Ok(tls)) = tls.next().await {
      tokio::spawn(async move {
        let _ = serve_tls(tls).await;
      });
    }
  });

  local_set.await;
}

async fn wrap_client_auth_https_server() {
  let mut tls = get_tls_listener_stream(
    "https_client_auth",
    HTTPS_CLIENT_AUTH_PORT,
    Default::default(),
  )
  .await;

  let tls = async_stream::stream! {
    while let Some(Ok(mut tls)) = tls.next().await {
      let handshake = tls.handshake().await?;
      // We only need to check for the presence of client certificates
      // here. Rusttls ensures that they are valid and signed by the CA.
      match handshake.has_peer_certificates {
        true => { yield Ok(tls); },
        false => { eprintln!("https_client_auth: no valid client certificate"); },
      };
    }
  };

  let main_server_https_svc =
    make_service_fn(|_| async { Ok::<_, Infallible>(service_fn(main_server)) });
  let main_server_https = Server::builder(HyperAcceptor {
    acceptor: tls.boxed_local(),
  })
  .serve(main_server_https_svc);

  let _ = main_server_https.await;
}

// Use the single-threaded scheduler. The hyper server is used as a point of
// comparison for the (single-threaded!) benchmarks in cli/bench. We're not
// comparing apples to apples if we use the default multi-threaded scheduler.
#[tokio::main(flavor = "current_thread")]
pub async fn run_all_servers() {
  if let Some(port) = env::args().nth(1) {
    return hyper_hello(port.parse::<u16>().unwrap()).await;
  }

  let redirect_server_fut = wrap_redirect_server();
  let double_redirects_server_fut = wrap_double_redirect_server();
  let inf_redirects_server_fut = wrap_inf_redirect_server();
  let another_redirect_server_fut = wrap_another_redirect_server();
  let auth_redirect_server_fut = wrap_auth_redirect_server();
  let basic_auth_redirect_server_fut = wrap_basic_auth_redirect_server();
  let abs_redirect_server_fut = wrap_abs_redirect_server();

  let ws_server_fut = run_ws_server(WS_PORT);
  let ws_ping_server_fut = run_ws_ping_server(WS_PING_PORT);
  let wss_server_fut = run_wss_server(WSS_PORT);
  let ws_close_server_fut = run_ws_close_server(WS_CLOSE_PORT);
  let wss2_server_fut = run_wss2_server(WSS2_PORT);

  let tls_server_fut = run_tls_server();
  let tls_client_auth_server_fut = run_tls_client_auth_server();
  let client_auth_server_https_fut = wrap_client_auth_https_server();
  let main_server_fut = wrap_main_server();
  let main_server_ipv6_fut = wrap_main_ipv6_server();
  let main_server_https_fut = wrap_main_https_server();
  let h1_only_server_tls_fut = wrap_https_h1_only_tls_server();
  let h2_only_server_tls_fut = wrap_https_h2_only_tls_server();
  let h1_only_server_fut = wrap_http_h1_only_server();
  let h2_only_server_fut = wrap_http_h2_only_server();
  let h2_grpc_server_fut = h2_grpc_server();

  let server_fut = async {
    futures::join!(
      redirect_server_fut,
      ws_server_fut,
      ws_ping_server_fut,
      wss_server_fut,
      wss2_server_fut,
      tls_server_fut,
      tls_client_auth_server_fut,
      ws_close_server_fut,
      another_redirect_server_fut,
      auth_redirect_server_fut,
      basic_auth_redirect_server_fut,
      inf_redirects_server_fut,
      double_redirects_server_fut,
      abs_redirect_server_fut,
      main_server_fut,
      main_server_ipv6_fut,
      main_server_https_fut,
      client_auth_server_https_fut,
      h1_only_server_tls_fut,
      h2_only_server_tls_fut,
      h1_only_server_fut,
      h2_only_server_fut,
      h2_grpc_server_fut,
    )
  }
  .boxed_local();

  server_fut.await;
}

fn custom_headers(p: &str, body: Vec<u8>) -> Response<Body> {
  let mut response = Response::new(Body::from(body));

  if p.ends_with("/run/import_compression/brotli") {
    response
      .headers_mut()
      .insert("Content-Encoding", HeaderValue::from_static("br"));
    response.headers_mut().insert(
      "Content-Type",
      HeaderValue::from_static("application/javascript"),
    );
    response
      .headers_mut()
      .insert("Content-Length", HeaderValue::from_static("26"));
    return response;
  }
  if p.ends_with("/run/import_compression/gziped") {
    response
      .headers_mut()
      .insert("Content-Encoding", HeaderValue::from_static("gzip"));
    response.headers_mut().insert(
      "Content-Type",
      HeaderValue::from_static("application/javascript"),
    );
    response
      .headers_mut()
      .insert("Content-Length", HeaderValue::from_static("39"));
    return response;
  }

  if p.contains("/encoding/") {
    let charset = p
      .split_terminator('/')
      .last()
      .unwrap()
      .trim_end_matches(".ts");

    response.headers_mut().insert(
      "Content-Type",
      HeaderValue::from_str(
        &format!("application/typescript;charset={charset}")[..],
      )
      .unwrap(),
    );
    return response;
  }

  let content_type = if p.contains(".t1.") {
    Some("text/typescript")
  } else if p.contains(".t2.") {
    Some("video/vnd.dlna.mpeg-tts")
  } else if p.contains(".t3.") {
    Some("video/mp2t")
  } else if p.contains(".t4.") {
    Some("application/x-typescript")
  } else if p.contains(".j1.") {
    Some("text/javascript")
  } else if p.contains(".j2.") {
    Some("application/ecmascript")
  } else if p.contains(".j3.") {
    Some("text/ecmascript")
  } else if p.contains(".j4.") {
    Some("application/x-javascript")
  } else if p.contains("form_urlencoded") {
    Some("application/x-www-form-urlencoded")
  } else if p.contains("unknown_ext") || p.contains("no_ext") {
    Some("text/typescript")
  } else if p.contains("mismatch_ext") || p.contains("no_js_ext") {
    Some("text/javascript")
  } else if p.ends_with(".ts") || p.ends_with(".tsx") {
    Some("application/typescript")
  } else if p.ends_with(".js") || p.ends_with(".jsx") {
    Some("application/javascript")
  } else if p.ends_with(".json") {
    Some("application/json")
  } else if p.ends_with(".wasm") {
    Some("application/wasm")
  } else if p.ends_with(".tgz") {
    Some("application/gzip")
  } else {
    None
  };

  if let Some(t) = content_type {
    response
      .headers_mut()
      .insert("Content-Type", HeaderValue::from_str(t).unwrap());
    return response;
  }

  response
}

#[derive(Default)]
struct HttpServerCount {
  count: usize,
  test_server: Option<Child>,
}

impl HttpServerCount {
  fn inc(&mut self) {
    self.count += 1;
    if self.test_server.is_none() {
      assert_eq!(self.count, 1);

      println!("test_server starting...");
      let mut test_server = Command::new(test_server_path())
        .current_dir(testdata_path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to execute test_server");
      let stdout = test_server.stdout.as_mut().unwrap();
      use std::io::BufRead;
      use std::io::BufReader;
      let lines = BufReader::new(stdout).lines();

      // Wait for all the servers to report being ready.
      let mut ready_count = 0;
      for maybe_line in lines {
        if let Ok(line) = maybe_line {
          if line.starts_with("ready:") {
            ready_count += 1;
          }
          if ready_count == 12 {
            break;
          }
        } else {
          panic!("{}", maybe_line.unwrap_err());
        }
      }
      self.test_server = Some(test_server);
    }
  }

  fn dec(&mut self) {
    assert!(self.count > 0);
    self.count -= 1;
    if self.count == 0 {
      let mut test_server = self.test_server.take().unwrap();
      match test_server.try_wait() {
        Ok(None) => {
          test_server.kill().expect("failed to kill test_server");
          let _ = test_server.wait();
        }
        Ok(Some(status)) => {
          panic!("test_server exited unexpectedly {status}")
        }
        Err(e) => panic!("test_server error: {e}"),
      }
    }
  }
}

impl Drop for HttpServerCount {
  fn drop(&mut self) {
    assert_eq!(self.count, 0);
    assert!(self.test_server.is_none());
  }
}

fn lock_http_server<'a>() -> MutexGuard<'a, HttpServerCount> {
  let r = GUARD.lock();
  if let Err(poison_err) = r {
    // If panics happened, ignore it. This is for tests.
    poison_err.into_inner()
  } else {
    r.unwrap()
  }
}

pub struct HttpServerGuard {}

impl Drop for HttpServerGuard {
  fn drop(&mut self) {
    let mut g = lock_http_server();
    g.dec();
  }
}

/// Adds a reference to a shared target/debug/test_server subprocess. When the
/// last instance of the HttpServerGuard is dropped, the subprocess will be
/// killed.
pub fn http_server() -> HttpServerGuard {
  ensure_test_server_built();
  let mut g = lock_http_server();
  g.inc();
  HttpServerGuard {}
}

/// Helper function to strip ansi codes.
pub fn strip_ansi_codes(s: &str) -> std::borrow::Cow<str> {
  console_static_text::ansi::strip_ansi_codes(s)
}

pub fn run(
  cmd: &[&str],
  input: Option<&[&str]>,
  envs: Option<Vec<(String, String)>>,
  current_dir: Option<&str>,
  expect_success: bool,
) {
  let mut process_builder = Command::new(cmd[0]);
  process_builder.args(&cmd[1..]).stdin(Stdio::piped());

  if let Some(dir) = current_dir {
    process_builder.current_dir(dir);
  }
  if let Some(envs) = envs {
    process_builder.envs(envs);
  }
  let mut prog = process_builder.spawn().expect("failed to spawn script");
  if let Some(lines) = input {
    let stdin = prog.stdin.as_mut().expect("failed to get stdin");
    stdin
      .write_all(lines.join("\n").as_bytes())
      .expect("failed to write to stdin");
  }
  let status = prog.wait().expect("failed to wait on child");
  if expect_success != status.success() {
    panic!("Unexpected exit code: {:?}", status.code());
  }
}

pub fn run_collect(
  cmd: &[&str],
  input: Option<&[&str]>,
  envs: Option<Vec<(String, String)>>,
  current_dir: Option<&str>,
  expect_success: bool,
) -> (String, String) {
  let mut process_builder = Command::new(cmd[0]);
  process_builder
    .args(&cmd[1..])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  if let Some(dir) = current_dir {
    process_builder.current_dir(dir);
  }
  if let Some(envs) = envs {
    process_builder.envs(envs);
  }
  let mut prog = process_builder.spawn().expect("failed to spawn script");
  if let Some(lines) = input {
    let stdin = prog.stdin.as_mut().expect("failed to get stdin");
    stdin
      .write_all(lines.join("\n").as_bytes())
      .expect("failed to write to stdin");
  }
  let Output {
    stdout,
    stderr,
    status,
  } = prog.wait_with_output().expect("failed to wait on child");
  let stdout = String::from_utf8(stdout).unwrap();
  let stderr = String::from_utf8(stderr).unwrap();
  if expect_success != status.success() {
    eprintln!("stdout: <<<{stdout}>>>");
    eprintln!("stderr: <<<{stderr}>>>");
    panic!("Unexpected exit code: {:?}", status.code());
  }
  (stdout, stderr)
}

pub fn run_and_collect_output(
  expect_success: bool,
  args: &str,
  input: Option<Vec<&str>>,
  envs: Option<Vec<(String, String)>>,
  need_http_server: bool,
) -> (String, String) {
  run_and_collect_output_with_args(
    expect_success,
    args.split_whitespace().collect(),
    input,
    envs,
    need_http_server,
  )
}

pub fn run_and_collect_output_with_args(
  expect_success: bool,
  args: Vec<&str>,
  input: Option<Vec<&str>>,
  envs: Option<Vec<(String, String)>>,
  need_http_server: bool,
) -> (String, String) {
  let mut deno_process_builder = deno_cmd()
    .args_vec(args)
    .current_dir(testdata_path())
    .stdin(Stdio::piped())
    .piped_output();
  if let Some(envs) = envs {
    deno_process_builder = deno_process_builder.envs(envs);
  }
  let _http_guard = if need_http_server {
    Some(http_server())
  } else {
    None
  };
  let mut deno = deno_process_builder
    .spawn()
    .expect("failed to spawn script");
  if let Some(lines) = input {
    let stdin = deno.stdin.as_mut().expect("failed to get stdin");
    stdin
      .write_all(lines.join("\n").as_bytes())
      .expect("failed to write to stdin");
  }
  let Output {
    stdout,
    stderr,
    status,
  } = deno.wait_with_output().expect("failed to wait on child");
  let stdout = String::from_utf8(stdout).unwrap();
  let stderr = String::from_utf8(stderr).unwrap();
  if expect_success != status.success() {
    eprintln!("stdout: <<<{stdout}>>>");
    eprintln!("stderr: <<<{stderr}>>>");
    panic!("Unexpected exit code: {:?}", status.code());
  }
  (stdout, stderr)
}

pub fn new_deno_dir() -> TempDir {
  TempDir::new()
}

pub fn deno_cmd() -> TestCommandBuilder {
  let deno_dir = new_deno_dir();
  deno_cmd_with_deno_dir(&deno_dir)
}

pub fn deno_cmd_with_deno_dir(deno_dir: &TempDir) -> TestCommandBuilder {
  TestCommandBuilder::new(deno_dir.clone())
    .env("DENO_DIR", deno_dir.path())
    .env("NPM_CONFIG_REGISTRY", npm_registry_unset_url())
}

pub fn run_powershell_script_file(
  script_file_path: &str,
  args: Vec<&str>,
) -> std::result::Result<(), i64> {
  let deno_dir = new_deno_dir();
  let mut command = Command::new("powershell.exe");

  command
    .env("DENO_DIR", deno_dir.path())
    .current_dir(testdata_path())
    .arg("-file")
    .arg(script_file_path);

  for arg in args {
    command.arg(arg);
  }

  let output = command.output().expect("failed to spawn script");
  let stdout = String::from_utf8(output.stdout).unwrap();
  let stderr = String::from_utf8(output.stderr).unwrap();
  println!("{stdout}");
  if !output.status.success() {
    panic!(
      "{script_file_path} executed with failing error code\n{stdout}{stderr}"
    );
  }

  Ok(())
}

#[derive(Debug, Default)]
pub struct CheckOutputIntegrationTest<'a> {
  pub args: &'a str,
  pub args_vec: Vec<&'a str>,
  pub output: &'a str,
  pub input: Option<&'a str>,
  pub output_str: Option<&'a str>,
  pub exit_code: i32,
  pub http_server: bool,
  pub envs: Vec<(String, String)>,
  pub env_clear: bool,
  pub temp_cwd: bool,
  /// Copies the files at the specified directory in the "testdata" directory
  /// to the temp folder and runs the test from there. This is useful when
  /// the test creates files in the testdata directory (ex. a node_modules folder)
  pub copy_temp_dir: Option<&'a str>,
  /// Relative to "testdata" directory
  pub cwd: Option<&'a str>,
}

impl<'a> CheckOutputIntegrationTest<'a> {
  pub fn output(&self) -> TestCommandOutput {
    let mut context_builder = TestContextBuilder::default();
    if self.temp_cwd {
      context_builder = context_builder.use_temp_cwd();
    }
    if let Some(dir) = &self.copy_temp_dir {
      context_builder = context_builder.use_copy_temp_dir(dir);
    }
    if self.http_server {
      context_builder = context_builder.use_http_server();
    }

    let context = context_builder.build();

    let mut command_builder = context.new_command();

    if !self.args.is_empty() {
      command_builder = command_builder.args(self.args);
    }
    if !self.args_vec.is_empty() {
      command_builder = command_builder.args_vec(self.args_vec.clone());
    }
    if let Some(input) = &self.input {
      command_builder = command_builder.stdin_text(input);
    }
    for (key, value) in &self.envs {
      command_builder = command_builder.env(key, value);
    }
    if self.env_clear {
      command_builder = command_builder.env_clear();
    }
    if let Some(cwd) = &self.cwd {
      command_builder = command_builder.current_dir(cwd);
    }

    command_builder.run()
  }
}

pub fn wildcard_match(pattern: &str, text: &str) -> bool {
  match wildcard_match_detailed(pattern, text) {
    WildcardMatchResult::Success => true,
    WildcardMatchResult::Fail(debug_output) => {
      eprintln!("{}", debug_output);
      false
    }
  }
}

pub enum WildcardMatchResult {
  Success,
  Fail(String),
}

pub fn wildcard_match_detailed(
  pattern: &str,
  text: &str,
) -> WildcardMatchResult {
  fn annotate_whitespace(text: &str) -> String {
    text.replace('\t', "\u{2192}").replace(' ', "\u{00B7}")
  }

  // Normalize line endings
  let original_text = text.replace("\r\n", "\n");
  let mut current_text = original_text.as_str();
  let pattern = pattern.replace("\r\n", "\n");
  let mut output_lines = Vec::new();

  let parts = parse_wildcard_pattern_text(&pattern).unwrap();

  let mut was_last_wildcard = false;
  for (i, part) in parts.iter().enumerate() {
    match part {
      WildcardPatternPart::Wildcard => {
        output_lines.push("<WILDCARD />".to_string());
      }
      WildcardPatternPart::Text(search_text) => {
        let is_last = i + 1 == parts.len();
        let search_index = if is_last && was_last_wildcard {
          // search from the end of the file
          current_text.rfind(search_text)
        } else {
          current_text.find(search_text)
        };
        match search_index {
          Some(found_index) if was_last_wildcard || found_index == 0 => {
            output_lines.push(format!(
              "<FOUND>{}</FOUND>",
              colors::gray(annotate_whitespace(search_text))
            ));
            current_text = &current_text[found_index + search_text.len()..];
          }
          Some(index) => {
            output_lines.push(
              "==== FOUND SEARCH TEXT IN WRONG POSITION ====".to_string(),
            );
            output_lines.push(colors::gray(annotate_whitespace(search_text)));
            output_lines
              .push("==== HAD UNKNOWN PRECEEDING TEXT ====".to_string());
            output_lines
              .push(colors::red(annotate_whitespace(&current_text[..index])));
            return WildcardMatchResult::Fail(output_lines.join("\n"));
          }
          None => {
            let mut max_found_index = 0;
            for (index, _) in search_text.char_indices() {
              let sub_string = &search_text[..index];
              if let Some(found_index) = current_text.find(sub_string) {
                if was_last_wildcard || found_index == 0 {
                  max_found_index = index;
                } else {
                  break;
                }
              } else {
                break;
              }
            }
            if !was_last_wildcard && max_found_index > 0 {
              output_lines.push(format!(
                "<FOUND>{}</FOUND>",
                colors::gray(annotate_whitespace(
                  &search_text[..max_found_index]
                ))
              ));
            }
            output_lines
              .push("==== COULD NOT FIND SEARCH TEXT ====".to_string());
            output_lines.push(colors::green(annotate_whitespace(
              if was_last_wildcard {
                search_text
              } else {
                &search_text[max_found_index..]
              },
            )));
            if was_last_wildcard && max_found_index > 0 {
              output_lines.push(format!(
                "==== MAX FOUND ====\n{}",
                colors::red(annotate_whitespace(
                  &search_text[..max_found_index]
                ))
              ));
            }
            let actual_next_text = &current_text[max_found_index..];
            let max_next_text_len = 40;
            let next_text_len =
              std::cmp::min(max_next_text_len, actual_next_text.len());
            output_lines.push(format!(
              "==== NEXT ACTUAL TEXT ====\n{}{}",
              colors::red(annotate_whitespace(
                &actual_next_text[..next_text_len]
              )),
              if actual_next_text.len() > max_next_text_len {
                "[TRUNCATED]"
              } else {
                ""
              },
            ));
            return WildcardMatchResult::Fail(output_lines.join("\n"));
          }
        }
      }
      WildcardPatternPart::UnorderedLines(expected_lines) => {
        assert!(!was_last_wildcard, "unsupported");
        let mut actual_lines = Vec::with_capacity(expected_lines.len());
        for _ in 0..expected_lines.len() {
          match current_text.find('\n') {
            Some(end_line_index) => {
              actual_lines.push(&current_text[..end_line_index]);
              current_text = &current_text[end_line_index + 1..];
            }
            None => {
              break;
            }
          }
        }
        actual_lines.sort_unstable();
        let mut expected_lines = expected_lines.clone();
        expected_lines.sort_unstable();

        if actual_lines.len() != expected_lines.len() {
          output_lines
            .push("==== HAD WRONG NUMBER OF UNORDERED LINES ====".to_string());
          output_lines.push("# ACTUAL".to_string());
          output_lines.extend(
            actual_lines
              .iter()
              .map(|l| colors::green(annotate_whitespace(l))),
          );
          output_lines.push("# EXPECTED".to_string());
          output_lines.extend(
            expected_lines
              .iter()
              .map(|l| colors::green(annotate_whitespace(l))),
          );
          return WildcardMatchResult::Fail(output_lines.join("\n"));
        }
        for (actual, expected) in actual_lines.iter().zip(expected_lines.iter())
        {
          if actual != expected {
            output_lines
              .push("==== UNORDERED LINE DID NOT MATCH ====".to_string());
            output_lines.push(format!(
              "  ACTUAL: {}",
              colors::red(annotate_whitespace(actual))
            ));
            output_lines.push(format!(
              "EXPECTED: {}",
              colors::green(annotate_whitespace(expected))
            ));
            return WildcardMatchResult::Fail(output_lines.join("\n"));
          } else {
            output_lines.push(format!(
              "<FOUND>{}</FOUND>",
              colors::gray(annotate_whitespace(expected))
            ));
          }
        }
      }
    }
    was_last_wildcard = matches!(part, WildcardPatternPart::Wildcard);
  }

  if was_last_wildcard || current_text.is_empty() {
    WildcardMatchResult::Success
  } else {
    output_lines.push("==== HAD TEXT AT END OF FILE ====".to_string());
    output_lines.push(colors::red(annotate_whitespace(current_text)));
    WildcardMatchResult::Fail(output_lines.join("\n"))
  }
}

#[derive(Debug)]
enum WildcardPatternPart<'a> {
  Wildcard,
  Text(&'a str),
  UnorderedLines(Vec<&'a str>),
}

fn parse_wildcard_pattern_text(
  text: &str,
) -> Result<Vec<WildcardPatternPart>, monch::ParseErrorFailureError> {
  use monch::*;

  fn parse_unordered_lines(input: &str) -> ParseResult<Vec<&str>> {
    const END_TEXT: &str = "\n[UNORDERED_END]\n";
    let (input, _) = tag("[UNORDERED_START]\n")(input)?;
    match input.find(END_TEXT) {
      Some(end_index) => ParseResult::Ok((
        &input[end_index + END_TEXT.len()..],
        input[..end_index].lines().collect::<Vec<_>>(),
      )),
      None => ParseError::fail(input, "Could not find [UNORDERED_END]"),
    }
  }

  enum InnerPart<'a> {
    Wildcard,
    UnorderedLines(Vec<&'a str>),
    Char,
  }

  struct Parser<'a> {
    current_input: &'a str,
    last_text_input: &'a str,
    parts: Vec<WildcardPatternPart<'a>>,
  }

  impl<'a> Parser<'a> {
    fn parse(mut self) -> ParseResult<'a, Vec<WildcardPatternPart<'a>>> {
      while !self.current_input.is_empty() {
        let (next_input, inner_part) = or3(
          map(tag("[WILDCARD]"), |_| InnerPart::Wildcard),
          map(parse_unordered_lines, |lines| {
            InnerPart::UnorderedLines(lines)
          }),
          map(next_char, |_| InnerPart::Char),
        )(self.current_input)?;
        match inner_part {
          InnerPart::Wildcard => {
            self.queue_previous_text(next_input);
            self.parts.push(WildcardPatternPart::Wildcard);
          }
          InnerPart::UnorderedLines(expected_lines) => {
            self.queue_previous_text(next_input);
            self
              .parts
              .push(WildcardPatternPart::UnorderedLines(expected_lines));
          }
          InnerPart::Char => {
            // ignore
          }
        }
        self.current_input = next_input;
      }

      self.queue_previous_text("");

      ParseResult::Ok(("", self.parts))
    }

    fn queue_previous_text(&mut self, next_input: &'a str) {
      let previous_text = &self.last_text_input
        [..self.last_text_input.len() - self.current_input.len()];
      if !previous_text.is_empty() {
        self.parts.push(WildcardPatternPart::Text(previous_text));
      }
      self.last_text_input = next_input;
    }
  }

  with_failure_handling(|input| {
    Parser {
      current_input: input,
      last_text_input: input,
      parts: Vec::new(),
    }
    .parse()
  })(text)
}

pub fn with_pty(deno_args: &[&str], action: impl FnMut(Pty)) {
  let context = TestContextBuilder::default().use_temp_cwd().build();
  context.new_command().args_vec(deno_args).with_pty(action);
}

pub struct WrkOutput {
  pub latency: f64,
  pub requests: u64,
}

pub fn parse_wrk_output(output: &str) -> WrkOutput {
  static REQUESTS_RX: Lazy<Regex> =
    lazy_regex::lazy_regex!(r"Requests/sec:\s+(\d+)");
  static LATENCY_RX: Lazy<Regex> =
    lazy_regex::lazy_regex!(r"\s+99%(?:\s+(\d+.\d+)([a-z]+))");

  let mut requests = None;
  let mut latency = None;

  for line in output.lines() {
    if requests.is_none() {
      if let Some(cap) = REQUESTS_RX.captures(line) {
        requests =
          Some(str::parse::<u64>(cap.get(1).unwrap().as_str()).unwrap());
      }
    }
    if latency.is_none() {
      if let Some(cap) = LATENCY_RX.captures(line) {
        let time = cap.get(1).unwrap();
        let unit = cap.get(2).unwrap();

        latency = Some(
          str::parse::<f64>(time.as_str()).unwrap()
            * match unit.as_str() {
              "ms" => 1.0,
              "us" => 0.001,
              "s" => 1000.0,
              _ => unreachable!(),
            },
        );
      }
    }
  }

  WrkOutput {
    requests: requests.unwrap(),
    latency: latency.unwrap(),
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct StraceOutput {
  pub percent_time: f64,
  pub seconds: f64,
  pub usecs_per_call: Option<u64>,
  pub calls: u64,
  pub errors: u64,
}

pub fn parse_strace_output(output: &str) -> HashMap<String, StraceOutput> {
  let mut summary = HashMap::new();

  // Filter out non-relevant lines. See the error log at
  // https://github.com/denoland/deno/pull/3715/checks?check_run_id=397365887
  // This is checked in testdata/strace_summary2.out
  let mut lines = output.lines().filter(|line| {
    !line.is_empty()
      && !line.contains("detached ...")
      && !line.contains("unfinished ...")
      && !line.contains("????")
  });
  let count = lines.clone().count();

  if count < 4 {
    return summary;
  }

  let total_line = lines.next_back().unwrap();
  lines.next_back(); // Drop separator
  let data_lines = lines.skip(2);

  for line in data_lines {
    let syscall_fields = line.split_whitespace().collect::<Vec<_>>();
    let len = syscall_fields.len();
    let syscall_name = syscall_fields.last().unwrap();
    if (5..=6).contains(&len) {
      summary.insert(
        syscall_name.to_string(),
        StraceOutput {
          percent_time: str::parse::<f64>(syscall_fields[0]).unwrap(),
          seconds: str::parse::<f64>(syscall_fields[1]).unwrap(),
          usecs_per_call: Some(str::parse::<u64>(syscall_fields[2]).unwrap()),
          calls: str::parse::<u64>(syscall_fields[3]).unwrap(),
          errors: if syscall_fields.len() < 6 {
            0
          } else {
            str::parse::<u64>(syscall_fields[4]).unwrap()
          },
        },
      );
    }
  }

  let total_fields = total_line.split_whitespace().collect::<Vec<_>>();

  let mut usecs_call_offset = 0;
  summary.insert(
    "total".to_string(),
    StraceOutput {
      percent_time: str::parse::<f64>(total_fields[0]).unwrap(),
      seconds: str::parse::<f64>(total_fields[1]).unwrap(),
      usecs_per_call: if total_fields.len() > 5 {
        usecs_call_offset = 1;
        Some(str::parse::<u64>(total_fields[2]).unwrap())
      } else {
        None
      },
      calls: str::parse::<u64>(total_fields[2 + usecs_call_offset]).unwrap(),
      errors: str::parse::<u64>(total_fields[3 + usecs_call_offset]).unwrap(),
    },
  );

  summary
}

pub fn parse_max_mem(output: &str) -> Option<u64> {
  // Takes the output from "time -v" as input and extracts the 'maximum
  // resident set size' and returns it in bytes.
  for line in output.lines() {
    if line
      .to_lowercase()
      .contains("maximum resident set size (kbytes)")
    {
      let value = line.split(": ").nth(1).unwrap();
      return Some(str::parse::<u64>(value).unwrap() * 1024);
    }
  }

  None
}

pub(crate) mod colors {
  use std::io::Write;

  use termcolor::Ansi;
  use termcolor::Color;
  use termcolor::ColorSpec;
  use termcolor::WriteColor;

  pub fn bold<S: AsRef<str>>(s: S) -> String {
    let mut style_spec = ColorSpec::new();
    style_spec.set_bold(true);
    style(s, style_spec)
  }

  pub fn red<S: AsRef<str>>(s: S) -> String {
    fg_color(s, Color::Red)
  }

  pub fn bold_red<S: AsRef<str>>(s: S) -> String {
    bold_fg_color(s, Color::Red)
  }

  pub fn green<S: AsRef<str>>(s: S) -> String {
    fg_color(s, Color::Green)
  }

  pub fn bold_green<S: AsRef<str>>(s: S) -> String {
    bold_fg_color(s, Color::Green)
  }

  pub fn bold_blue<S: AsRef<str>>(s: S) -> String {
    bold_fg_color(s, Color::Blue)
  }

  pub fn gray<S: AsRef<str>>(s: S) -> String {
    fg_color(s, Color::Ansi256(245))
  }

  fn bold_fg_color<S: AsRef<str>>(s: S, color: Color) -> String {
    let mut style_spec = ColorSpec::new();
    style_spec.set_bold(true);
    style_spec.set_fg(Some(color));
    style(s, style_spec)
  }

  fn fg_color<S: AsRef<str>>(s: S, color: Color) -> String {
    let mut style_spec = ColorSpec::new();
    style_spec.set_fg(Some(color));
    style(s, style_spec)
  }

  fn style<S: AsRef<str>>(s: S, colorspec: ColorSpec) -> String {
    let mut v = Vec::new();
    let mut ansi_writer = Ansi::new(&mut v);
    ansi_writer.set_color(&colorspec).unwrap();
    ansi_writer.write_all(s.as_ref().as_bytes()).unwrap();
    ansi_writer.reset().unwrap();
    String::from_utf8_lossy(&v).into_owned()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use pretty_assertions::assert_eq;

  #[test]
  fn parse_wrk_output_1() {
    const TEXT: &str = include_str!("./testdata/wrk1.txt");
    let wrk = parse_wrk_output(TEXT);
    assert_eq!(wrk.requests, 1837);
    assert!((wrk.latency - 6.25).abs() < f64::EPSILON);
  }

  #[test]
  fn parse_wrk_output_2() {
    const TEXT: &str = include_str!("./testdata/wrk2.txt");
    let wrk = parse_wrk_output(TEXT);
    assert_eq!(wrk.requests, 53435);
    assert!((wrk.latency - 6.22).abs() < f64::EPSILON);
  }

  #[test]
  fn parse_wrk_output_3() {
    const TEXT: &str = include_str!("./testdata/wrk3.txt");
    let wrk = parse_wrk_output(TEXT);
    assert_eq!(wrk.requests, 96037);
    assert!((wrk.latency - 6.36).abs() < f64::EPSILON);
  }

  #[test]
  fn strace_parse_1() {
    const TEXT: &str = include_str!("./testdata/strace_summary.out");
    let strace = parse_strace_output(TEXT);

    // first syscall line
    let munmap = strace.get("munmap").unwrap();
    assert_eq!(munmap.calls, 60);
    assert_eq!(munmap.errors, 0);

    // line with errors
    assert_eq!(strace.get("mkdir").unwrap().errors, 2);

    // last syscall line
    let prlimit = strace.get("prlimit64").unwrap();
    assert_eq!(prlimit.calls, 2);
    assert!((prlimit.percent_time - 0.0).abs() < f64::EPSILON);

    // summary line
    assert_eq!(strace.get("total").unwrap().calls, 704);
    assert_eq!(strace.get("total").unwrap().errors, 5);
    assert_eq!(strace.get("total").unwrap().usecs_per_call, None);
  }

  #[test]
  fn strace_parse_2() {
    const TEXT: &str = include_str!("./testdata/strace_summary2.out");
    let strace = parse_strace_output(TEXT);

    // first syscall line
    let futex = strace.get("futex").unwrap();
    assert_eq!(futex.calls, 449);
    assert_eq!(futex.errors, 94);

    // summary line
    assert_eq!(strace.get("total").unwrap().calls, 821);
    assert_eq!(strace.get("total").unwrap().errors, 107);
    assert_eq!(strace.get("total").unwrap().usecs_per_call, None);
  }

  #[test]
  fn strace_parse_3() {
    const TEXT: &str = include_str!("./testdata/strace_summary3.out");
    let strace = parse_strace_output(TEXT);

    // first syscall line
    let futex = strace.get("mprotect").unwrap();
    assert_eq!(futex.calls, 90);
    assert_eq!(futex.errors, 0);

    // summary line
    assert_eq!(strace.get("total").unwrap().calls, 543);
    assert_eq!(strace.get("total").unwrap().errors, 36);
    assert_eq!(strace.get("total").unwrap().usecs_per_call, Some(6));
  }

  #[test]
  fn parse_parse_wildcard_match_text() {
    let result =
      parse_wildcard_pattern_text("[UNORDERED_START]\ntesting\ntesting")
        .err()
        .unwrap();
    assert_contains!(result.to_string(), "Could not find [UNORDERED_END]");
  }

  #[test]
  fn test_wildcard_match() {
    let fixtures = vec![
      ("foobarbaz", "foobarbaz", true),
      ("[WILDCARD]", "foobarbaz", true),
      ("foobar", "foobarbaz", false),
      ("foo[WILDCARD]baz", "foobarbaz", true),
      ("foo[WILDCARD]baz", "foobazbar", false),
      ("foo[WILDCARD]baz[WILDCARD]qux", "foobarbazqatqux", true),
      ("foo[WILDCARD]", "foobar", true),
      ("foo[WILDCARD]baz[WILDCARD]", "foobarbazqat", true),
      // check with different line endings
      ("foo[WILDCARD]\nbaz[WILDCARD]\n", "foobar\nbazqat\n", true),
      (
        "foo[WILDCARD]\nbaz[WILDCARD]\n",
        "foobar\r\nbazqat\r\n",
        true,
      ),
      (
        "foo[WILDCARD]\r\nbaz[WILDCARD]\n",
        "foobar\nbazqat\r\n",
        true,
      ),
      (
        "foo[WILDCARD]\r\nbaz[WILDCARD]\r\n",
        "foobar\nbazqat\n",
        true,
      ),
      (
        "foo[WILDCARD]\r\nbaz[WILDCARD]\r\n",
        "foobar\r\nbazqat\r\n",
        true,
      ),
    ];

    // Iterate through the fixture lists, testing each one
    for (pattern, string, expected) in fixtures {
      let actual = wildcard_match(pattern, string);
      dbg!(pattern, string, expected);
      assert_eq!(actual, expected);
    }
  }

  #[test]
  fn test_wildcard_match2() {
    // foo, bar, baz, qux, quux, quuz, corge, grault, garply, waldo, fred, plugh, xyzzy

    assert!(wildcard_match("foo[WILDCARD]baz", "foobarbaz"));
    assert!(!wildcard_match("foo[WILDCARD]baz", "foobazbar"));

    let multiline_pattern = "[WILDCARD]
foo:
[WILDCARD]baz[WILDCARD]";

    fn multi_line_builder(input: &str, leading_text: Option<&str>) -> String {
      // If there is leading text add a newline so it's on it's own line
      let head = match leading_text {
        Some(v) => format!("{v}\n"),
        None => "".to_string(),
      };
      format!(
        "{head}foo:
quuz {input} corge
grault"
      )
    }

    // Validate multi-line string builder
    assert_eq!(
      "QUUX=qux
foo:
quuz BAZ corge
grault",
      multi_line_builder("BAZ", Some("QUUX=qux"))
    );

    // Correct input & leading line
    assert!(wildcard_match(
      multiline_pattern,
      &multi_line_builder("baz", Some("QUX=quux")),
    ));

    // Should fail when leading line
    assert!(!wildcard_match(
      multiline_pattern,
      &multi_line_builder("baz", None),
    ));

    // Incorrect input & leading line
    assert!(!wildcard_match(
      multiline_pattern,
      &multi_line_builder("garply", Some("QUX=quux")),
    ));

    // Incorrect input & no leading line
    assert!(!wildcard_match(
      multiline_pattern,
      &multi_line_builder("garply", None),
    ));
  }

  #[test]
  fn test_wildcard_match_unordered_lines() {
    // matching
    assert!(wildcard_match(
      concat!("[UNORDERED_START]\n", "B\n", "A\n", "[UNORDERED_END]\n"),
      concat!("A\n", "B\n",)
    ));
    // different line
    assert!(!wildcard_match(
      concat!("[UNORDERED_START]\n", "Ba\n", "A\n", "[UNORDERED_END]\n"),
      concat!("A\n", "B\n",)
    ));
    // different number of lines
    assert!(!wildcard_match(
      concat!(
        "[UNORDERED_START]\n",
        "B\n",
        "A\n",
        "C\n",
        "[UNORDERED_END]\n"
      ),
      concat!("A\n", "B\n",)
    ));
  }

  #[test]
  fn max_mem_parse() {
    const TEXT: &str = include_str!("./testdata/time.out");
    let size = parse_max_mem(TEXT);

    assert_eq!(size, Some(120380 * 1024));
  }
}
