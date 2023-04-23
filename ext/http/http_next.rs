// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
use crate::extract_network_stream;
use crate::request_body::HttpRequestBody;
use crate::request_properties::DefaultHttpRequestProperties;
use crate::request_properties::HttpConnectionProperties;
use crate::request_properties::HttpListenProperties;
use crate::request_properties::HttpPropertyExtractor;
use crate::response_body::CompletionHandle;
use crate::response_body::ResponseBytes;
use crate::response_body::ResponseBytesInner;
use crate::response_body::V8StreamHttpResponseBody;
use crate::LocalExecutor;
use deno_core::error::AnyError;
use deno_core::futures::TryFutureExt;
use deno_core::op;
use deno_core::AsyncRefCell;
use deno_core::BufView;
use deno_core::ByteString;
use deno_core::CancelFuture;
use deno_core::CancelHandle;
use deno_core::CancelTryFuture;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::ZeroCopyBuf;
use deno_net::ops_tls::TlsStream;
use deno_net::raw::put_network_stream_resource;
use deno_net::raw::NetworkStream;
use deno_net::raw::NetworkStreamAddress;
use http::request::Parts;
use hyper1::body::Incoming;
use hyper1::header::COOKIE;
use hyper1::http::HeaderName;
use hyper1::http::HeaderValue;
use hyper1::server::conn::http1;
use hyper1::server::conn::http2;
use hyper1::service::service_fn;
use hyper1::upgrade::OnUpgrade;
use hyper1::StatusCode;
use pin_project::pin_project;
use pin_project::pinned_drop;
use slab::Slab;
use std::borrow::Cow;
use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::pin::Pin;
use std::rc::Rc;
use tokio::task::spawn_local;
use tokio::task::JoinHandle;

type Request = hyper1::Request<Incoming>;
type Response = hyper1::Response<ResponseBytes>;

pub struct HttpSlabRecord {
  request_info: HttpConnectionProperties,
  request_parts: Parts,
  request_body: Option<Incoming>,
  // The response may get taken before we tear this down
  response: Option<Response>,
  body: Option<Rc<HttpRequestBody>>,
  promise: CompletionHandle,
  #[cfg(__zombie_http_tracking)]
  alive: bool,
}

thread_local! {
  pub static SLAB: RefCell<Slab<HttpSlabRecord>> = RefCell::new(Slab::with_capacity(1024));
}

/// Generates getters and setters for the [`SLAB`]. For example,
/// `with!(with_req, with_req_mut, Parts, http, http.request_parts);` expands to:
///
/// ```ignore
/// #[inline(always)]
/// #[allow(dead_code)]
/// pub(crate) fn with_req_mut<T>(key: usize, f: impl FnOnce(&mut Parts) -> T) -> T {
///   SLAB.with(|slab| {
///     let mut borrow = slab.borrow_mut();
///     let mut http = borrow.get_mut(key).unwrap();
///     #[cfg(__zombie_http_tracking)]
///     if !http.alive {
///       panic!("Attempted to access a dead HTTP object")
///     }
///     f(&mut http.expr)
///   })
/// }

/// #[inline(always)]
/// #[allow(dead_code)]
/// pub(crate) fn with_req<T>(key: usize, f: impl FnOnce(&Parts) -> T) -> T {
///   SLAB.with(|slab| {
///     let mut borrow = slab.borrow();
///     let mut http = borrow.get(key).unwrap();
///     #[cfg(__zombie_http_tracking)]
///     if !http.alive {
///       panic!("Attempted to access a dead HTTP object")
///     }
///     f(&http.expr)
///   })
/// }
/// ```
macro_rules! with {
  ($ref:ident, $mut:ident, $type:ty, $http:ident, $expr:expr) => {
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn $mut<T>(key: usize, f: impl FnOnce(&mut $type) -> T) -> T {
      SLAB.with(|slab| {
        let mut borrow = slab.borrow_mut();
        #[allow(unused_mut)] // TODO(mmastrac): compiler issue?
        let mut $http = match borrow.get_mut(key) {
          Some(http) => http,
          None => panic!(
            "Attemped to access invalid request {} ({} in total available)",
            key,
            borrow.len()
          ),
        };
        #[cfg(__zombie_http_tracking)]
        if !$http.alive {
          panic!("Attempted to access a dead HTTP object")
        }
        f(&mut $expr)
      })
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn $ref<T>(key: usize, f: impl FnOnce(&$type) -> T) -> T {
      SLAB.with(|slab| {
        let borrow = slab.borrow();
        let $http = borrow.get(key).unwrap();
        #[cfg(__zombie_http_tracking)]
        if !$http.alive {
          panic!("Attempted to access a dead HTTP object")
        }
        f(&$expr)
      })
    }
  };
}

with!(with_req, with_req_mut, Parts, http, http.request_parts);
with!(
  with_req_body,
  with_req_body_mut,
  Option<Incoming>,
  http,
  http.request_body
);
with!(
  with_resp,
  with_resp_mut,
  Option<Response>,
  http,
  http.response
);
with!(
  with_body,
  with_body_mut,
  Option<Rc<HttpRequestBody>>,
  http,
  http.body
);
with!(
  with_promise,
  with_promise_mut,
  CompletionHandle,
  http,
  http.promise
);
with!(with_http, with_http_mut, HttpSlabRecord, http, http);

fn slab_insert(
  request: Request,
  request_info: HttpConnectionProperties,
) -> usize {
  SLAB.with(|slab| {
    let (request_parts, request_body) = request.into_parts();
    slab.borrow_mut().insert(HttpSlabRecord {
      request_info,
      request_parts,
      request_body: Some(request_body),
      response: Some(Response::new(ResponseBytes::default())),
      body: None,
      promise: CompletionHandle::default(),
      #[cfg(__zombie_http_tracking)]
      alive: true,
    })
  })
}

#[op]
pub fn op_upgrade_raw(_index: usize) {}

#[op]
pub async fn op_upgrade(
  state: Rc<RefCell<OpState>>,
  index: usize,
  headers: Vec<(ByteString, ByteString)>,
) -> Result<(ResourceId, ZeroCopyBuf), AnyError> {
  // Stage 1: set the respnse to 101 Switching Protocols and send it
  let upgrade = with_http_mut(index, |http| {
    // Manually perform the upgrade. We're peeking into hyper's underlying machinery here a bit
    let upgrade = http
      .request_parts
      .extensions
      .remove::<OnUpgrade>()
      .ok_or_else(|| AnyError::msg("upgrade unavailable"))?;

    let response = http.response.as_mut().unwrap();
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    for (name, value) in headers {
      response.headers_mut().append(
        HeaderName::from_bytes(&name).unwrap(),
        HeaderValue::from_bytes(&value).unwrap(),
      );
    }
    http.promise.complete(true);
    Ok::<_, AnyError>(upgrade)
  })?;

  // Stage 2: wait for the request to finish upgrading
  let upgraded = upgrade.await?;

  // Stage 3: return the extracted raw network stream
  let (stream, bytes) = extract_network_stream(upgraded);

  // We're allocating for those extra bytes, but they are probably going to be empty most of the time
  Ok((
    put_network_stream_resource(
      &mut state.borrow_mut().resource_table,
      stream,
    )?,
    ZeroCopyBuf::from(bytes.to_vec()),
  ))
}

#[op]
pub fn op_set_promise_complete(index: usize, status: u16) {
  with_resp_mut(index, |resp| {
    // The Javascript code will never provide a status that is invalid here (see 23_response.js)
    *resp.as_mut().unwrap().status_mut() =
      StatusCode::from_u16(status).unwrap();
  });
  with_promise_mut(index, |promise| {
    promise.complete(true);
  });
}

#[op]
pub fn op_get_request_method_and_url(
  index: usize,
) -> (String, Option<String>, String, String, Option<u16>) {
  // TODO(mmastrac): Passing method can be optimized
  with_http(index, |http| {
    let request_properties = DefaultHttpRequestProperties::request_properties(
      &http.request_info,
      &http.request_parts.uri,
      &http.request_parts.headers,
    );

    // Only extract the path part - we handle authority elsewhere
    let path = match &http.request_parts.uri.path_and_query() {
      Some(path_and_query) => path_and_query.to_string(),
      None => "".to_owned(),
    };

    (
      http.request_parts.method.as_str().to_owned(),
      request_properties.authority,
      path,
      String::from(http.request_info.peer_address.as_ref()),
      http.request_info.peer_port,
    )
  })
}

#[op]
pub fn op_get_request_header(index: usize, name: String) -> Option<ByteString> {
  with_req(index, |req| {
    let value = req.headers.get(name);
    value.map(|value| value.as_bytes().into())
  })
}

#[op]
pub fn op_get_request_headers(index: usize) -> Vec<(ByteString, ByteString)> {
  with_req(index, |req| {
    let headers = &req.headers;
    let mut vec = Vec::with_capacity(headers.len());
    let mut cookies: Option<Vec<&[u8]>> = None;
    for (name, value) in headers {
      if name == COOKIE {
        if let Some(ref mut cookies) = cookies {
          cookies.push(value.as_bytes());
        } else {
          cookies = Some(vec![value.as_bytes()]);
        }
      } else {
        let name: &[u8] = name.as_ref();
        vec.push((name.into(), value.as_bytes().into()))
      }
    }

    // We treat cookies specially, because we don't want them to get them
    // mangled by the `Headers` object in JS. What we do is take all cookie
    // headers and concat them into a single cookie header, separated by
    // semicolons.
    // TODO(mmastrac): This should probably happen on the JS side on-demand
    if let Some(cookies) = cookies {
      let cookie_sep = "; ".as_bytes();
      vec.push((
        ByteString::from(COOKIE.as_str()),
        ByteString::from(cookies.join(cookie_sep)),
      ));
    }
    vec
  })
}

#[op]
pub fn op_read_request_body(state: &mut OpState, index: usize) -> ResourceId {
  let incoming = with_req_body_mut(index, |body| body.take().unwrap());
  let body_resource = Rc::new(HttpRequestBody::new(incoming));
  let res = state.resource_table.add_rc(body_resource.clone());
  with_body_mut(index, |body| {
    *body = Some(body_resource);
  });
  res
}

#[op]
pub fn op_set_response_header(
  index: usize,
  name: ByteString,
  value: ByteString,
) {
  with_resp_mut(index, |resp| {
    let resp_headers = resp.as_mut().unwrap().headers_mut();
    // These are valid latin-1 strings
    let name = HeaderName::from_bytes(&name).unwrap();
    let value = HeaderValue::from_bytes(&value).unwrap();
    resp_headers.append(name, value);
  });
}

#[op]
pub fn op_set_response_headers(
  index: usize,
  headers: Vec<(ByteString, ByteString)>,
) {
  // TODO(mmastrac): Invalid headers should be handled?
  with_resp_mut(index, |resp| {
    let resp_headers = resp.as_mut().unwrap().headers_mut();
    resp_headers.reserve(headers.len());
    for (name, value) in headers {
      // These are valid latin-1 strings
      let name = HeaderName::from_bytes(&name).unwrap();
      let value = HeaderValue::from_bytes(&value).unwrap();
      resp_headers.append(name, value);
    }
  })
}

#[op]
pub fn op_set_response_body_resource(
  state: &mut OpState,
  index: usize,
  stream_rid: ResourceId,
  auto_close: bool,
) -> Result<(), AnyError> {
  // If the stream is auto_close, we will hold the last ref to it until the response is complete.
  let resource = if auto_close {
    state.resource_table.take_any(stream_rid)?
  } else {
    state.resource_table.get_any(stream_rid)?
  };

  with_resp_mut(index, move |response| {
    let future = resource.clone().read(64 * 1024);
    response
      .as_mut()
      .unwrap()
      .body_mut()
      .initialize(ResponseBytesInner::Resource(auto_close, resource, future));
  });

  Ok(())
}

#[op]
pub fn op_set_response_body_stream(
  state: &mut OpState,
  index: usize,
) -> Result<ResourceId, AnyError> {
  // TODO(mmastrac): what should this channel size be?
  let (tx, rx) = tokio::sync::mpsc::channel(1);
  let (tx, rx) = (
    V8StreamHttpResponseBody::new(tx),
    ResponseBytesInner::V8Stream(rx),
  );

  with_resp_mut(index, move |response| {
    response.as_mut().unwrap().body_mut().initialize(rx);
  });

  Ok(state.resource_table.add(tx))
}

#[op]
pub fn op_set_response_body_text(index: usize, text: String) {
  if !text.is_empty() {
    with_resp_mut(index, move |response| {
      response
        .as_mut()
        .unwrap()
        .body_mut()
        .initialize(ResponseBytesInner::Bytes(BufView::from(text.into_bytes())))
    });
  }
}

#[op]
pub fn op_set_response_body_bytes(index: usize, buffer: ZeroCopyBuf) {
  if !buffer.is_empty() {
    with_resp_mut(index, |response| {
      response
        .as_mut()
        .unwrap()
        .body_mut()
        .initialize(ResponseBytesInner::Bytes(BufView::from(buffer)))
    });
  };
}

#[op]
pub async fn op_http_track(
  state: Rc<RefCell<OpState>>,
  index: usize,
  server_rid: ResourceId,
) -> Result<(), AnyError> {
  let handle = with_resp(index, |resp| {
    resp.as_ref().unwrap().body().completion_handle()
  });

  let join_handle = state
    .borrow_mut()
    .resource_table
    .get::<HttpJoinHandle>(server_rid)?;

  match handle.or_cancel(join_handle.cancel_handle()).await {
    Ok(true) => Ok(()),
    Ok(false) => {
      Err(AnyError::msg("connection closed before message completed"))
    }
    Err(_e) => Ok(()),
  }
}

#[pin_project(PinnedDrop)]
pub struct SlabFuture<F: Future<Output = ()>>(usize, #[pin] F);

pub fn new_slab_future(
  request: Request,
  request_info: HttpConnectionProperties,
  tx: tokio::sync::mpsc::Sender<usize>,
) -> SlabFuture<impl Future<Output = ()>> {
  let index = slab_insert(request, request_info);
  let rx = with_promise(index, |promise| promise.clone());
  SlabFuture(index, async move {
    if tx.send(index).await.is_ok() {
      // We only need to wait for completion if we aren't closed
      rx.await;
    }
  })
}

impl<F: Future<Output = ()>> SlabFuture<F> {}

#[pinned_drop]
impl<F: Future<Output = ()>> PinnedDrop for SlabFuture<F> {
  fn drop(self: Pin<&mut Self>) {
    SLAB.with(|slab| {
      #[cfg(__zombie_http_tracking)]
      {
        slab.borrow_mut().get_mut(self.0).unwrap().alive = false;
      }
      #[cfg(not(__zombie_http_tracking))]
      {
        slab.borrow_mut().remove(self.0);
      }
    });
  }
}

impl<F: Future<Output = ()>> Future for SlabFuture<F> {
  type Output = Result<Response, hyper::Error>;

  fn poll(
    self: Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<Self::Output> {
    let index = self.0;
    self
      .project()
      .1
      .poll(cx)
      .map(|_| Ok(with_resp_mut(index, |resp| resp.take().unwrap())))
  }
}

fn serve_https(
  mut io: TlsStream,
  request_info: HttpConnectionProperties,
  cancel: RcRef<CancelHandle>,
  tx: tokio::sync::mpsc::Sender<usize>,
) -> JoinHandle<Result<(), AnyError>> {
  // TODO(mmastrac): This is faster if we can use tokio::spawn but then the send bounds get us
  let svc = service_fn(move |req: Request| {
    new_slab_future(req, request_info.clone(), tx.clone())
  });
  spawn_local(async {
    io.handshake().await?;
    let handshake = io.get_ref().1.alpn_protocol();
    // h2
    if handshake == Some(&[104, 50]) {
      let conn = http2::Builder::new(LocalExecutor).serve_connection(io, svc);

      conn.map_err(AnyError::from).try_or_cancel(cancel).await
    } else {
      let conn = http1::Builder::new()
        .keep_alive(true)
        .serve_connection(io, svc);

      conn
        .with_upgrades()
        .map_err(AnyError::from)
        .try_or_cancel(cancel)
        .await
    }
  })
}

fn serve_http(
  io: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
  request_info: HttpConnectionProperties,
  cancel: RcRef<CancelHandle>,
  tx: tokio::sync::mpsc::Sender<usize>,
) -> JoinHandle<Result<(), AnyError>> {
  // TODO(mmastrac): This is faster if we can use tokio::spawn but then the send bounds get us
  let svc = service_fn(move |req: Request| {
    new_slab_future(req, request_info.clone(), tx.clone())
  });
  spawn_local(async {
    let conn = http1::Builder::new()
      .keep_alive(true)
      .serve_connection(io, svc);
    conn
      .with_upgrades()
      .map_err(AnyError::from)
      .try_or_cancel(cancel)
      .await
  })
}

fn serve_http_on(
  network_stream: NetworkStream,
  listen_properties: &HttpListenProperties,
  cancel: RcRef<CancelHandle>,
  tx: tokio::sync::mpsc::Sender<usize>,
) -> JoinHandle<Result<(), AnyError>> {
  // We always want some sort of peer address. If we can't get one, just make up one.
  let peer_address = network_stream.peer_address().unwrap_or_else(|_| {
    NetworkStreamAddress::Ip(SocketAddr::V4(SocketAddrV4::new(
      Ipv4Addr::new(0, 0, 0, 0),
      0,
    )))
  });
  let connection_properties: HttpConnectionProperties =
    DefaultHttpRequestProperties::connection_properties(
      listen_properties,
      &peer_address,
    );

  match network_stream {
    NetworkStream::Tcp(conn) => {
      serve_http(conn, connection_properties, cancel, tx)
    }
    NetworkStream::Tls(conn) => {
      serve_https(conn, connection_properties, cancel, tx)
    }
    #[cfg(unix)]
    NetworkStream::Unix(conn) => {
      serve_http(conn, connection_properties, cancel, tx)
    }
  }
}

struct HttpJoinHandle(
  AsyncRefCell<Option<JoinHandle<Result<(), AnyError>>>>,
  CancelHandle,
  AsyncRefCell<tokio::sync::mpsc::Receiver<usize>>,
);

impl HttpJoinHandle {
  fn cancel_handle(self: &Rc<Self>) -> RcRef<CancelHandle> {
    RcRef::map(self, |this| &this.1)
  }
}

impl Resource for HttpJoinHandle {
  fn name(&self) -> Cow<str> {
    "http".into()
  }

  fn close(self: Rc<Self>) {
    self.1.cancel()
  }
}

#[op(v8)]
pub fn op_serve_http(
  state: Rc<RefCell<OpState>>,
  listener_rid: ResourceId,
) -> Result<(ResourceId, &'static str, String), AnyError> {
  let listener =
    DefaultHttpRequestProperties::get_network_stream_listener_for_rid(
      &mut state.borrow_mut(),
      listener_rid,
    )?;

  let local_address = listener.listen_address()?;
  let listen_properties = DefaultHttpRequestProperties::listen_properties(
    listener.stream(),
    &local_address,
  );

  let (tx, rx) = tokio::sync::mpsc::channel(10);
  let resource: Rc<HttpJoinHandle> = Rc::new(HttpJoinHandle(
    AsyncRefCell::new(None),
    CancelHandle::new(),
    AsyncRefCell::new(rx),
  ));
  let cancel_clone = resource.cancel_handle();

  let listen_properties_clone = listen_properties.clone();
  let handle = spawn_local(async move {
    loop {
      let conn = listener
        .accept()
        .try_or_cancel(cancel_clone.clone())
        .await?;
      serve_http_on(
        conn,
        &listen_properties_clone,
        cancel_clone.clone(),
        tx.clone(),
      );
    }
    #[allow(unreachable_code)]
    Ok::<_, AnyError>(())
  });

  // Set the handle after we start the future
  *RcRef::map(&resource, |this| &this.0)
    .try_borrow_mut()
    .unwrap() = Some(handle);

  Ok((
    state.borrow_mut().resource_table.add_rc(resource),
    listen_properties.scheme,
    listen_properties.fallback_host,
  ))
}

#[op(v8)]
pub fn op_serve_http_on(
  state: Rc<RefCell<OpState>>,
  conn: ResourceId,
) -> Result<(ResourceId, &'static str, String), AnyError> {
  let network_stream =
    DefaultHttpRequestProperties::get_network_stream_for_rid(
      &mut state.borrow_mut(),
      conn,
    )?;

  let local_address = network_stream.local_address()?;
  let listen_properties = DefaultHttpRequestProperties::listen_properties(
    network_stream.stream(),
    &local_address,
  );

  let (tx, rx) = tokio::sync::mpsc::channel(10);
  let resource: Rc<HttpJoinHandle> = Rc::new(HttpJoinHandle(
    AsyncRefCell::new(None),
    CancelHandle::new(),
    AsyncRefCell::new(rx),
  ));

  let handle = serve_http_on(
    network_stream,
    &listen_properties,
    resource.cancel_handle(),
    tx,
  );

  // Set the handle after we start the future
  *RcRef::map(&resource, |this| &this.0)
    .try_borrow_mut()
    .unwrap() = Some(handle);

  Ok((
    state.borrow_mut().resource_table.add_rc(resource),
    listen_properties.scheme,
    listen_properties.fallback_host,
  ))
}

#[op]
pub async fn op_http_wait(
  state: Rc<RefCell<OpState>>,
  rid: ResourceId,
) -> Result<u32, AnyError> {
  // We will get the join handle initially, as we might be consuming requests still
  let join_handle = state
    .borrow_mut()
    .resource_table
    .get::<HttpJoinHandle>(rid)?;

  let cancel = join_handle.clone().cancel_handle();
  let next = async {
    let mut recv = RcRef::map(&join_handle, |this| &this.2).borrow_mut().await;
    recv.recv().await
  }
  .or_cancel(cancel)
  .unwrap_or_else(|_| None)
  .await;

  // Do we have a request?
  if let Some(req) = next {
    return Ok(req as u32);
  }

  // No - we're shutting down
  let res = RcRef::map(join_handle, |this| &this.0)
    .borrow_mut()
    .await
    .take()
    .unwrap()
    .await?;

  // Drop the cancel and join handles
  state
    .borrow_mut()
    .resource_table
    .take::<HttpJoinHandle>(rid)?;

  // Filter out shutdown (ENOTCONN) errors
  if let Err(err) = res {
    if let Some(err) = err.source() {
      if let Some(err) = err.downcast_ref::<io::Error>() {
        if err.kind() == io::ErrorKind::NotConnected {
          return Ok(u32::MAX);
        }
      }
    }
    return Err(err);
  }

  Ok(u32::MAX)
}
