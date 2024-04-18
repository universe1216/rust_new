// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

import { assertEquals, assertInstanceOf } from "@std/assert/mod.ts";
import { delay } from "@std/async/delay.ts";
import { fromFileUrl, join } from "@std/path/mod.ts";
import * as tls from "node:tls";
import * as net from "node:net";
import * as stream from "node:stream";

const tlsTestdataDir = fromFileUrl(
  new URL("../testdata/tls", import.meta.url),
);
const keyFile = join(tlsTestdataDir, "localhost.key");
const certFile = join(tlsTestdataDir, "localhost.crt");
const key = Deno.readTextFileSync(keyFile);
const cert = Deno.readTextFileSync(certFile);
const rootCaCert = Deno.readTextFileSync(join(tlsTestdataDir, "RootCA.pem"));

for (
  const [alpnServer, alpnClient, expected] of [
    [["a", "b"], ["a"], ["a"]],
    [["a", "b"], ["b"], ["b"]],
    [["a", "b"], ["a", "b"], ["a"]],
    [["a", "b"], [], []],
    [[], ["a", "b"], []],
  ]
) {
  Deno.test(`tls.connect sends correct ALPN: '${alpnServer}' + '${alpnClient}' = '${expected}'`, async () => {
    const listener = Deno.listenTls({
      port: 0,
      key,
      cert,
      alpnProtocols: alpnServer,
    });
    const outgoing = tls.connect({
      host: "localhost",
      port: listener.addr.port,
      ALPNProtocols: alpnClient,
      secureContext: {
        ca: rootCaCert,
        // deno-lint-ignore no-explicit-any
      } as any,
    });

    const conn = await listener.accept();
    const handshake = await conn.handshake();
    assertEquals(handshake.alpnProtocol, expected[0] || null);
    conn.close();
    outgoing.destroy();
    listener.close();
  });
}

Deno.test("tls.connect makes tls connection", async () => {
  const ctl = new AbortController();
  let port;
  const serve = Deno.serve({
    port: 0,
    key,
    cert,
    signal: ctl.signal,
    onListen: (listen) => port = listen.port,
  }, () => new Response("hello"));

  await delay(200);

  const conn = tls.connect({
    host: "localhost",
    port,
    secureContext: {
      ca: rootCaCert,
      // deno-lint-ignore no-explicit-any
    } as any,
  });
  conn.write(`GET / HTTP/1.1
Host: localhost
Connection: close

`);
  const chunk = Promise.withResolvers<Uint8Array>();
  conn.on("data", (received) => {
    conn.destroy();
    ctl.abort();
    chunk.resolve(received);
  });

  await serve.finished;

  const text = new TextDecoder().decode(await chunk.promise);
  const bodyText = text.split("\r\n\r\n").at(-1)?.trim();
  assertEquals(bodyText, "hello");
});

// https://github.com/denoland/deno/pull/20120
Deno.test("tls.connect mid-read tcp->tls upgrade", async () => {
  const ctl = new AbortController();
  const serve = Deno.serve({
    port: 8443,
    key,
    cert,
    signal: ctl.signal,
  }, () => new Response("hello"));

  await delay(200);

  const conn = tls.connect({
    host: "localhost",
    port: 8443,
    secureContext: {
      ca: rootCaCert,
      // deno-lint-ignore no-explicit-any
    } as any,
  });

  conn.setEncoding("utf8");
  conn.write(`GET / HTTP/1.1\nHost: www.google.com\n\n`);

  conn.on("data", (_) => {
    conn.destroy();
    ctl.abort();
  });

  await serve.finished;
});

Deno.test("tls.createServer creates a TLS server", async () => {
  const deferred = Promise.withResolvers<void>();
  const server = tls.createServer(
    // deno-lint-ignore no-explicit-any
    { host: "0.0.0.0", key, cert } as any,
    (socket: net.Socket) => {
      socket.write("welcome!\n");
      socket.setEncoding("utf8");
      socket.pipe(socket).on("data", (data) => {
        if (data.toString().trim() === "goodbye") {
          socket.destroy();
        }
      });
    },
  );
  server.listen(0, async () => {
    const conn = await Deno.connectTls({
      hostname: "127.0.0.1",
      // deno-lint-ignore no-explicit-any
      port: (server.address() as any).port,
      caCerts: [rootCaCert],
    });

    const buf = new Uint8Array(100);
    await conn.read(buf);
    let text: string;
    text = new TextDecoder().decode(buf);
    assertEquals(text.replaceAll("\0", ""), "welcome!\n");
    buf.fill(0);

    await conn.write(new TextEncoder().encode("hey\n"));
    await conn.read(buf);
    text = new TextDecoder().decode(buf);
    assertEquals(text.replaceAll("\0", ""), "hey\n");
    buf.fill(0);

    await conn.write(new TextEncoder().encode("goodbye\n"));
    await conn.read(buf);
    text = new TextDecoder().decode(buf);
    assertEquals(text.replaceAll("\0", ""), "goodbye\n");

    conn.close();
    server.close();
    deferred.resolve();
  });
  await deferred.promise;
});

Deno.test("TLSSocket can construct without options", () => {
  // deno-lint-ignore no-explicit-any
  new tls.TLSSocket(new stream.PassThrough() as any);
});

Deno.test("tlssocket._handle._parentWrap is set", () => {
  // Note: This feature is used in popular 'http2-wrapper' module
  // https://github.com/szmarczak/http2-wrapper/blob/51eeaf59ff9344fb192b092241bfda8506983620/source/utils/js-stream-socket.js#L6
  const parentWrap =
    // deno-lint-ignore no-explicit-any
    ((new tls.TLSSocket(new stream.PassThrough() as any, {}) as any)
      // deno-lint-ignore no-explicit-any
      ._handle as any)!
      ._parentWrap;
  assertInstanceOf(parentWrap, stream.PassThrough);
});
