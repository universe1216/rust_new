// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use test_util::itest;

itest!(worker_error {
  args: "run -A workers/worker_error.ts",
  output: "workers/worker_error.ts.out",
  exit_code: 1,
});

itest!(worker_nested_error {
  args: "run -A workers/worker_nested_error.ts",
  output: "workers/worker_nested_error.ts.out",
  exit_code: 1,
});

itest!(worker_async_error {
  args: "run -A --quiet --reload workers/worker_async_error.ts",
  output: "workers/worker_async_error.ts.out",
  http_server: true,
  exit_code: 1,
});

itest!(worker_message_handler_error {
  args: "run -A --quiet --reload workers/worker_message_handler_error.ts",
  output: "workers/worker_message_handler_error.ts.out",
  http_server: true,
  exit_code: 1,
});

itest!(nonexistent_worker {
  args: "run --allow-read workers/nonexistent_worker.ts",
  output: "workers/nonexistent_worker.out",
  exit_code: 1,
});

itest!(_084_worker_custom_inspect {
  args: "run --allow-read workers/custom_inspect/main.ts",
  output: "workers/custom_inspect/main.out",
});

itest!(error_worker_permissions_local {
  args: "run --reload workers/error_worker_permissions_local.ts",
  output: "workers/error_worker_permissions_local.ts.out",
  exit_code: 1,
});

itest!(error_worker_permissions_remote {
  args: "run --reload workers/error_worker_permissions_remote.ts",
  http_server: true,
  output: "workers/error_worker_permissions_remote.ts.out",
  exit_code: 1,
});

itest!(worker_permissions_remote_remote {
    args: "run --quiet --reload --allow-net=localhost:4545 workers/permissions_remote_remote.ts",
    output: "workers/permissions_remote_remote.ts.out",
    http_server: true,
    exit_code: 1,
  });

itest!(worker_permissions_dynamic_remote {
    args: "run --quiet --reload --allow-net --unstable-worker-options workers/permissions_dynamic_remote.ts",
    output: "workers/permissions_dynamic_remote.ts.out",
    http_server: true,
    exit_code: 1,
  });

itest!(worker_permissions_data_remote {
    args: "run --quiet --reload --allow-net=localhost:4545 workers/permissions_data_remote.ts",
    output: "workers/permissions_data_remote.ts.out",
    http_server: true,
    exit_code: 1,
  });

itest!(worker_permissions_blob_remote {
    args: "run --quiet --reload --allow-net=localhost:4545 workers/permissions_blob_remote.ts",
    output: "workers/permissions_blob_remote.ts.out",
    http_server: true,
    exit_code: 1,
  });

itest!(worker_permissions_data_local {
    args: "run --quiet --reload --allow-net=localhost:4545 workers/permissions_data_local.ts",
    output: "workers/permissions_data_local.ts.out",
    http_server: true,
    exit_code: 1,
  });

itest!(worker_permissions_blob_local {
    args: "run --quiet --reload --allow-net=localhost:4545 workers/permissions_blob_local.ts",
    output: "workers/permissions_blob_local.ts.out",
    http_server: true,
    exit_code: 1,
  });

itest!(worker_terminate_tla_crash {
  args: "run --quiet --reload workers/terminate_tla_crash.js",
  output: "workers/terminate_tla_crash.js.out",
});

itest!(worker_error_event {
  args: "run --quiet -A workers/error_event.ts",
  output: "workers/error_event.ts.out",
  exit_code: 1,
});

// Regression test for https://github.com/denoland/deno/issues/19903
itest!(worker_doest_stall_event_loop {
  args: "run --quiet -A workers/worker_doest_stall_event_loop.ts",
  output: "workers/worker_doest_stall_event_loop.ts.out",
  exit_code: 0,
});

itest!(worker_ids_are_sequential {
  args: "run --quiet -A workers/worker_ids_are_sequential.ts",
  output: "workers/worker_ids_are_sequential.ts.out",
  exit_code: 0,
});

// Test for https://github.com/denoland/deno/issues/22629
// Test for https://github.com/denoland/deno/issues/22934
itest!(node_worker_auto_exits {
  args: "run --quiet --allow-read workers/node_worker_auto_exits.mjs",
  output: "workers/node_worker_auto_exits.mjs.out",
  exit_code: 0,
});

itest!(node_worker_message_port {
  args: "run --quiet --allow-read workers/node_worker_message_port.mjs",
  output: "workers/node_worker_message_port.mjs.out",
  exit_code: 0,
});

itest!(node_worker_transfer_port {
  args: "run --quiet --allow-read workers/node_worker_transfer_port.mjs",
  output: "workers/node_worker_transfer_port.mjs.out",
  exit_code: 0,
});

itest!(node_worker_message_port_unref {
  args: "run --quiet --allow-env --allow-read workers/node_worker_message_port_unref.mjs",
  output: "workers/node_worker_message_port_unref.mjs.out",
  exit_code: 0,
});

itest!(node_worker_parent_port_unref {
  envs: vec![("PARENT_PORT".into(), "1".into())],
  args: "run --quiet --allow-env --allow-read workers/node_worker_message_port_unref.mjs",
  output: "workers/node_worker_message_port_unref.mjs.out",
  exit_code: 0,
});
