// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use deno_ast::ModuleSpecifier;
use deno_core::serde::Deserialize;
use deno_core::serde::Serialize;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::url::Url;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::fs;
use std::process::Stdio;
use test_util::deno_cmd_with_deno_dir;
use test_util::env_vars_for_npm_tests;
use test_util::lsp::LspClient;
use test_util::lsp::LspClientBuilder;
use test_util::testdata_path;
use test_util::TestContextBuilder;
use tower_lsp::lsp_types as lsp;

fn did_open<V>(
  client: &mut LspClient,
  params: V,
) -> Vec<lsp::PublishDiagnosticsParams>
where
  V: Serialize,
{
  client
    .write_notification("textDocument/didOpen", params)
    .unwrap();

  handle_configuration_request(
    client,
    json!([{
      "enable": true,
      "codeLens": {
        "test": true
      }
    }]),
  );
  read_diagnostics(client).0
}

fn handle_configuration_request(client: &mut LspClient, result: Value) {
  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client.write_response(id, result).unwrap();
}

fn read_diagnostics(client: &mut LspClient) -> CollectedDiagnostics {
  // diagnostics come in batches of three unless they're cancelled
  let mut diagnostics = vec![];
  for _ in 0..3 {
    let (method, response) = client
      .read_notification::<lsp::PublishDiagnosticsParams>()
      .unwrap();
    assert_eq!(method, "textDocument/publishDiagnostics");
    diagnostics.push(response.unwrap());
  }
  CollectedDiagnostics(diagnostics)
}

// todo(dsherret): get rid of this in favour of LspClient
struct TestSession {
  client: LspClient,
  open_file_count: usize,
}

impl TestSession {
  pub fn from_client(client: LspClient) -> Self {
    Self {
      client,
      open_file_count: 0,
    }
  }

  pub fn did_open<V>(&mut self, params: V) -> CollectedDiagnostics
  where
    V: Serialize,
  {
    self
      .client
      .write_notification("textDocument/didOpen", params)
      .unwrap();

    let (id, method, _) = self.client.read_request::<Value>().unwrap();
    assert_eq!(method, "workspace/configuration");
    self
      .client
      .write_response(
        id,
        json!([{
          "enable": true,
          "codeLens": {
            "test": true
          }
        }]),
      )
      .unwrap();

    self.open_file_count += 1;
    self.read_diagnostics()
  }

  pub fn read_diagnostics(&mut self) -> CollectedDiagnostics {
    let mut all_diagnostics = Vec::new();
    for _ in 0..self.open_file_count {
      all_diagnostics.extend(read_diagnostics(&mut self.client).0);
    }
    CollectedDiagnostics(all_diagnostics)
  }

  pub fn shutdown_and_exit(&mut self) {
    self.client.shutdown();
  }
}

#[derive(Debug, Clone)]
struct CollectedDiagnostics(Vec<lsp::PublishDiagnosticsParams>);

impl CollectedDiagnostics {
  /// Gets the diagnostics that the editor will see after all the publishes.
  pub fn viewed(&self) -> Vec<lsp::Diagnostic> {
    self
      .viewed_messages()
      .into_iter()
      .flat_map(|m| m.diagnostics)
      .collect()
  }

  /// Gets the messages that the editor will see after all the publishes.
  pub fn viewed_messages(&self) -> Vec<lsp::PublishDiagnosticsParams> {
    // go over the publishes in reverse order in order to get
    // the final messages that will be shown in the editor
    let mut messages = Vec::new();
    let mut had_specifier = HashSet::new();
    for message in self.0.iter().rev() {
      if had_specifier.insert(message.uri.clone()) {
        messages.insert(0, message.clone());
      }
    }
    messages
  }

  pub fn with_source(&self, source: &str) -> lsp::PublishDiagnosticsParams {
    self
      .viewed_messages()
      .iter()
      .find(|p| {
        p.diagnostics
          .iter()
          .any(|d| d.source == Some(source.to_string()))
      })
      .map(ToOwned::to_owned)
      .unwrap()
  }

  pub fn with_file_and_source(
    &self,
    specifier: &str,
    source: &str,
  ) -> lsp::PublishDiagnosticsParams {
    let specifier = ModuleSpecifier::parse(specifier).unwrap();
    self
      .viewed_messages()
      .iter()
      .find(|p| {
        p.uri == specifier
          && p
            .diagnostics
            .iter()
            .any(|d| d.source == Some(source.to_string()))
      })
      .map(ToOwned::to_owned)
      .unwrap()
  }
}

#[test]
fn lsp_startup_shutdown() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  client.shutdown();
}

#[test]
fn lsp_init_tsconfig() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  temp_dir.write(
    "lib.tsconfig.json",
    r#"{
  "compilerOptions": {
    "lib": ["deno.ns", "deno.unstable", "dom"]
  }
}"#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("lib.tsconfig.json");
  });

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "location.pathname;\n"
      }
    }),
  );

  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);

  client.shutdown();
}

#[test]
fn lsp_tsconfig_types() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  temp_dir.write(
    "types.tsconfig.json",
    r#"{
  "compilerOptions": {
    "types": ["./a.d.ts"]
  },
  "lint": {
    "rules": {
      "tags": []
    }
  }
}"#,
  );
  let a_dts = "// deno-lint-ignore-file no-var\ndeclare var a: string;";
  temp_dir.write("a.d.ts", a_dts);

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("types.tsconfig.json");
  });

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": Url::from_file_path(temp_dir.path().join("test.ts")).unwrap(),
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(a);\n"
      }
    }),
  );

  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);

  client.shutdown();
}

#[test]
fn lsp_tsconfig_bad_config_path() {
  let mut client = LspClientBuilder::new().build();
  client.initialize(|builder| {
    builder
      .set_config("bad_tsconfig.json")
      .set_maybe_root_uri(None);
  });
  let (method, maybe_params) = client.read_notification().unwrap();
  assert_eq!(method, "window/showMessage");
  assert_eq!(maybe_params, Some(lsp::ShowMessageParams {
    typ: lsp::MessageType::WARNING,
    message: "The path to the configuration file (\"bad_tsconfig.json\") is not resolvable.".to_string()
  }));
  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Deno.args);\n"
      }
    }),
  );
  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);
}

#[test]
fn lsp_triple_slash_types() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  let a_dts = "// deno-lint-ignore-file no-var\ndeclare var a: string;";
  temp_dir.write("a.d.ts", a_dts);
  let mut client = context.new_lsp_command().build();
  client.initialize_default();

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": temp_dir.uri().join("test.ts").unwrap(),
        "languageId": "typescript",
        "version": 1,
        "text": "/// <reference types=\"./a.d.ts\" />\n\nconsole.log(a);\n"
      }
    }),
  );

  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);

  client.shutdown();
}

#[test]
fn lsp_import_map() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  let import_map = r#"{
  "imports": {
    "/~/": "./lib/"
  }
}"#;
  temp_dir.write("import-map.json", import_map);
  temp_dir.create_dir_all("lib");
  temp_dir.write("lib/b.ts", r#"export const b = "b";"#);

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_import_map("import-map.json");
  });

  let uri = Url::from_file_path(temp_dir.path().join("a.ts")).unwrap();

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": uri,
        "languageId": "typescript",
        "version": 1,
        "text": "import { b } from \"/~/b.ts\";\n\nconsole.log(b);\n"
      }
    }),
  );

  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": uri
        },
        "position": { "line": 2, "character": 12 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value":"(alias) const b: \"b\"\nimport b"
        },
        ""
      ],
      "range": {
        "start": { "line": 2, "character": 12 },
        "end": { "line": 2, "character": 13 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_import_map_data_url() {
  let context = TestContextBuilder::new().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_import_map("data:application/json;utf8,{\"imports\": { \"example\": \"https://deno.land/x/example/mod.ts\" }}");
  });
  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import example from \"example\";\n"
      }
    }),
  );

  let mut diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  // This indicates that the import map is applied correctly.
  assert!(diagnostics.any(|diagnostic| diagnostic.code
    == Some(lsp::NumberOrString::String("no-cache".to_string()))
    && diagnostic
      .message
      .contains("https://deno.land/x/example/mod.ts")));
  client.shutdown();
}

#[test]
fn lsp_import_map_config_file() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  temp_dir.write(
    "deno.import_map.jsonc",
    r#"{
  "importMap": "import-map.json"
}"#,
  );
  temp_dir.write(
    "import-map.json",
    r#"{
  "imports": {
    "/~/": "./lib/"
  }
}"#,
  );
  temp_dir.create_dir_all("lib");
  temp_dir.write("lib/b.ts", r#"export const b = "b";"#);

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.import_map.jsonc");
  });

  let uri = temp_dir.uri().join("a.ts").unwrap();

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": uri,
        "languageId": "typescript",
        "version": 1,
        "text": "import { b } from \"/~/b.ts\";\n\nconsole.log(b);\n"
      }
    }),
  );

  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": uri
        },
        "position": { "line": 2, "character": 12 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value":"(alias) const b: \"b\"\nimport b"
        },
        ""
      ],
      "range": {
        "start": { "line": 2, "character": 12 },
        "end": { "line": 2, "character": 13 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_import_map_embedded_in_config_file() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  temp_dir.write(
    "deno.embedded_import_map.jsonc",
    r#"{
  "imports": {
    "/~/": "./lib/"
  }
}"#,
  );
  temp_dir.create_dir_all("lib");
  temp_dir.write("lib/b.ts", r#"export const b = "b";"#);

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.embedded_import_map.jsonc");
  });

  let uri = temp_dir.uri().join("a.ts").unwrap();

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": uri,
        "languageId": "typescript",
        "version": 1,
        "text": "import { b } from \"/~/b.ts\";\n\nconsole.log(b);\n"
      }
    }),
  );

  let diagnostics = diagnostics.into_iter().flat_map(|x| x.diagnostics);
  assert_eq!(diagnostics.count(), 0);

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": uri
        },
        "position": { "line": 2, "character": 12 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value":"(alias) const b: \"b\"\nimport b"
        },
        ""
      ],
      "range": {
        "start": { "line": 2, "character": 12 },
        "end": { "line": 2, "character": 13 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_deno_task() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  temp_dir.write(
    "deno.jsonc",
    r#"{
    "tasks": {
      "build": "deno test",
      "some:test": "deno bundle mod.ts"
    }
  }"#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.jsonc");
  });

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>("deno/task", json!(null))
    .unwrap();

  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([
      {
        "name": "build",
        "detail": "deno test"
      }, {
        "name": "some:test",
        "detail": "deno bundle mod.ts"
      }
    ]))
  );
}

#[test]
fn lsp_import_assertions() {
  let context = TestContextBuilder::new().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_import_map("data:application/json;utf8,{\"imports\": { \"example\": \"https://deno.land/x/example/mod.ts\" }}");
  });

  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/test.json",
          "languageId": "json",
          "version": 1,
          "text": "{\"a\":1}"
        }
      }),
    )
    .unwrap();
  handle_configuration_request(
    &mut client,
    json!([{
      "enable": true,
      "codeLens": {
        "test": true
      }
    }]),
  );

  let diagnostics = CollectedDiagnostics(did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/a.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import a from \"./test.json\";\n\nconsole.log(a);\n"
      }
    }),
  ));

  assert_eq!(
    json!(
      diagnostics
        .with_file_and_source("file:///a/a.ts", "deno")
        .diagnostics
    ),
    json!([
      {
        "range": {
          "start": { "line": 0, "character": 14 },
          "end": { "line": 0, "character": 27 }
        },
        "severity": 1,
        "code": "no-assert-type",
        "source": "deno",
        "message": "The module is a JSON module and not being imported with an import assertion. Consider adding `assert { type: \"json\" }` to the import statement."
      }
    ])
  );

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/a.ts"
        },
        "range": {
          "start": { "line": 0, "character": 14 },
          "end": { "line": 0, "character": 27 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 0, "character": 14 },
              "end": { "line": 0, "character": 27 }
            },
            "severity": 1,
            "code": "no-assert-type",
            "source": "deno",
            "message": "The module is a JSON module and not being imported with an import assertion. Consider adding `assert { type: \"json\" }` to the import statement."
          }],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Insert import assertion.",
      "kind": "quickfix",
      "diagnostics": [
        {
          "range": {
            "start": { "line": 0, "character": 14 },
            "end": { "line": 0, "character": 27 }
          },
          "severity": 1,
          "code": "no-assert-type",
          "source": "deno",
          "message": "The module is a JSON module and not being imported with an import assertion. Consider adding `assert { type: \"json\" }` to the import statement."
        }
      ],
      "edit": {
        "changes": {
          "file:///a/a.ts": [
            {
              "range": {
                "start": { "line": 0, "character": 27 },
                "end": { "line": 0, "character": 27 }
              },
              "newText": " assert { type: \"json\" }"
            }
          ]
        }
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_import_map_import_completions() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  temp_dir.write(
    "import-map.json",
    r#"{
  "imports": {
    "/~/": "./lib/",
    "fs": "https://example.com/fs/index.js",
    "std/": "https://example.com/std@0.123.0/"
  }
}"#,
  );
  temp_dir.create_dir_all("lib");
  temp_dir.write("lib/b.ts", r#"export const b = "b";"#);

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_import_map("import-map.json");
  });

  let uri = temp_dir.uri().join("a.ts").unwrap();

  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": uri,
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"/~/b.ts\";\nimport * as b from \"\""
      }
    }),
  );

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": uri
        },
        "position": { "line": 1, "character": 20 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "\""
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "isIncomplete": false,
      "items": [
        {
          "label": ".",
          "kind": 19,
          "detail": "(local)",
          "sortText": "1",
          "insertText": ".",
          "commitCharacters": ["\"", "'"],
        }, {
          "label": "..",
          "kind": 19,
          "detail": "(local)",
          "sortText": "1",
          "insertText": "..",
          "commitCharacters": ["\"", "'"],
        }, {
          "label": "std",
          "kind": 19,
          "detail": "(import map)",
          "sortText": "std",
          "insertText": "std",
          "commitCharacters": ["\"", "'"],
        }, {
          "label": "fs",
          "kind": 17,
          "detail": "(import map)",
          "sortText": "fs",
          "insertText": "fs",
          "commitCharacters": ["\"", "'"],
        }, {
          "label": "/~",
          "kind": 19,
          "detail": "(import map)",
          "sortText": "/~",
          "insertText": "/~",
          "commitCharacters": ["\"", "'"],
        }
      ]
    }))
  );

  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": uri,
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 1, "character": 20 },
              "end": { "line": 1, "character": 20 }
            },
            "text": "/~/"
          }
        ]
      }),
    )
    .unwrap();
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": uri
        },
        "position": { "line": 1, "character": 23 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "/"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "isIncomplete": false,
      "items": [
        {
          "label": "b.ts",
          "kind": 9,
          "detail": "(import map)",
          "sortText": "1",
          "filterText": "/~/b.ts",
          "textEdit": {
            "range": {
              "start": { "line": 1, "character": 20 },
              "end": { "line": 1, "character": 23 }
            },
            "newText": "/~/b.ts"
          },
          "commitCharacters": ["\"", "'"],
        }
      ]
    }))
  );

  client.shutdown();
}

#[test]
fn lsp_hover() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Deno.args);\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "const Deno.args: string[]"
        },
        "Returns the script arguments to the program.\n\nGive the following command line invocation of Deno:\n\n```sh\ndeno run --allow-read https://deno.land/std/examples/cat.ts /etc/passwd\n```\n\nThen `Deno.args` will contain:\n\n```ts\n[ \"/etc/passwd\" ]\n```\n\nIf you are looking for a structured way to parse arguments, there is the\n[`std/flags`](https://deno.land/std/flags) module as part of the Deno\nstandard library.",
        "\n\n*@category* - Runtime Environment",
      ],
      "range": {
        "start": { "line": 0, "character": 17 },
        "end": { "line": 0, "character": 21 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_asset() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Date.now());\n"
      }
    }),
  );
  let (_, maybe_error) = client
    .write_request::<_, _, Value>(
      "textDocument/definition",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 14 }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  let (_, maybe_error) = client
    .write_request::<_, _, Value>(
      "deno/virtualTextDocument",
      json!({
        "textDocument": {
          "uri": "deno:/asset/lib.deno.shared_globals.d.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "deno:/asset/lib.es2015.symbol.wellknown.d.ts"
        },
        "position": { "line": 109, "character": 13 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "interface Date",
        },
        "Enables basic storage and retrieval of dates and times."
      ],
      "range": {
        "start": { "line": 109, "character": 10, },
        "end": { "line": 109, "character": 14, }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_disabled() {
  let context = TestContextBuilder::new().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_deno_enable(false);
  });
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "languageId": "typescript",
          "version": 1,
          "text": "console.log(Date.now());\n"
        }
      }),
    )
    .unwrap();

  handle_configuration_request(&mut client, json!([{ "enable": false }]));

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));
  client.shutdown();
}

#[test]
fn lsp_inlay_hints() {
  let context = TestContextBuilder::new().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.enable_inlay_hints();
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": r#"function a(b: string) {
          return b;
        }

        a("foo");

        enum C {
          A,
        }

        parseInt("123", 8);

        const d = Date.now();

        class E {
          f = Date.now();
        }

        ["a"].map((v) => v + v);
        "#
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/inlayHint",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 19, "character": 0, }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    json!(maybe_res),
    json!([
      {
        "position": { "line": 0, "character": 21 },
        "label": ": string",
        "kind": 1,
        "paddingLeft": true
      }, {
        "position": { "line": 4, "character": 10 },
        "label": "b:",
        "kind": 2,
        "paddingRight": true
      }, {
        "position": { "line": 7, "character": 11 },
        "label": "= 0",
        "paddingLeft": true
      }, {
        "position": { "line": 10, "character": 17 },
        "label": "string:",
        "kind": 2,
        "paddingRight": true
      }, {
        "position": { "line": 10, "character": 24 },
        "label": "radix:",
        "kind": 2,
        "paddingRight": true
      }, {
        "position": { "line": 12, "character": 15 },
        "label": ": number",
        "kind": 1,
        "paddingLeft": true
      }, {
        "position": { "line": 15, "character": 11 },
        "label": ": number",
        "kind": 1,
        "paddingLeft": true
      }, {
        "position": { "line": 18, "character": 18 },
        "label": "callbackfn:",
        "kind": 2,
        "paddingRight": true
      }, {
        "position": { "line": 18, "character": 20 },
        "label": ": string",
        "kind": 1,
        "paddingLeft": true
      }, {
        "position": { "line": 18, "character": 21 },
        "label": ": string",
        "kind": 1,
        "paddingLeft": true
      }
    ])
  );
}

#[test]
fn lsp_inlay_hints_not_enabled() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": r#"function a(b: string) {
          return b;
        }

        a("foo");

        enum C {
          A,
        }

        parseInt("123", 8);

        const d = Date.now();

        class E {
          f = Date.now();
        }

        ["a"].map((v) => v + v);
        "#
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/inlayHint",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 19, "character": 0, }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(json!(maybe_res), json!(null));
}

#[test]
fn lsp_workspace_enable_paths() {
  let context = TestContextBuilder::new().build();
  // we aren't actually writing anything to the tempdir in this test, but we
  // just need a legitimate file path on the host system so that logic that
  // tries to convert to and from the fs paths works on all env
  let temp_dir = context.deno_dir();

  let root_specifier = temp_dir.uri();

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder
      .set_enable_paths(vec!["./worker".to_string()])
      .set_root_uri(root_specifier.clone())
      .set_workspace_folders(vec![lsp::WorkspaceFolder {
        uri: root_specifier.clone(),
        name: "project".to_string(),
      }])
      .set_deno_enable(false);
  });

  handle_configuration_request(
    &mut client,
    json!([{
      "enable": false,
      "enablePaths": ["./worker"],
    }]),
  );

  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": root_specifier.join("./file.ts").unwrap(),
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Date.now());\n"
      }
    }),
  );

  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": root_specifier.join("./other/file.ts").unwrap(),
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Date.now());\n"
      }
    }),
  );

  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": root_specifier.join("./worker/file.ts").unwrap(),
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Date.now());\n"
      }
    }),
  );

  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": root_specifier.join("./worker/subdir/file.ts").unwrap(),
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Date.now());\n"
      }
    }),
  );

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": root_specifier.join("./file.ts").unwrap(),
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": root_specifier.join("./other/file.ts").unwrap(),
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": root_specifier.join("./worker/file.ts").unwrap(),
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "(method) DateConstructor.now(): number",
        },
        "Returns the number of milliseconds elapsed since midnight, January 1, 1970 Universal Coordinated Time (UTC)."
      ],
      "range": {
        "start": { "line": 0, "character": 17, },
        "end": { "line": 0, "character": 20, }
      }
    }))
  );

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": root_specifier.join("./worker/subdir/file.ts").unwrap(),
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "(method) DateConstructor.now(): number",
        },
        "Returns the number of milliseconds elapsed since midnight, January 1, 1970 Universal Coordinated Time (UTC)."
      ],
      "range": {
        "start": { "line": 0, "character": 17, },
        "end": { "line": 0, "character": 20, }
      }
    }))
  );

  client.shutdown();
}

#[test]
fn lsp_hover_unstable_disabled() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Deno.dlopen);\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "any"
        }
      ],
      "range": {
        "start": { "line": 0, "character": 17 },
        "end": { "line": 0, "character": 23 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_unstable_enabled() {
  let mut client = LspClientBuilder::new().build();
  client.initialize(|builder| {
    builder.set_unstable(true);
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Deno.ppid);\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents":[
        {
          "language":"typescript",
          "value":"const Deno.ppid: number"
        },
        "The process ID of parent process of this instance of the Deno CLI.\n\n```ts\nconsole.log(Deno.ppid);\n```",
        "\n\n*@category* - Runtime Environment",
      ],
      "range":{
        "start":{ "line":0, "character":17 },
        "end":{ "line":0, "character":21 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_change_mbc() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "const a = `编写软件很难`;\nconst b = `👍🦕😃`;\nconsole.log(a, b);\n"
      }
    }),
  );
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 1, "character": 11 },
              "end": {
                "line": 1,
                // the LSP uses utf16 encoded characters indexes, so
                // after the deno emoiji is character index 15
                "character": 15
              }
            },
            "text": ""
          }
        ]
      }),
    )
    .unwrap();
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 2, "character": 15 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "const b: \"😃\"",
        },
        "",
      ],
      "range": {
        "start": { "line": 2, "character": 15, },
        "end": { "line": 2, "character": 16, },
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_closed_document() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  temp_dir.write("a.ts", r#"export const a = "a";"#);
  temp_dir.write("b.ts", r#"export * from "./a.ts";"#);
  temp_dir.write("c.ts", "import { a } from \"./b.ts\";\nconsole.log(a);\n");

  let b_specifier = temp_dir.uri().join("b.ts").unwrap();
  let c_specifier = temp_dir.uri().join("c.ts").unwrap();

  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": b_specifier,
          "languageId": "typescript",
          "version": 1,
          "text": r#"export * from "./a.ts";"#
        }
      }),
    )
    .unwrap();
  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client
    .write_response(id, json!([{ "enable": true }]))
    .unwrap();

  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": c_specifier,
          "languageId": "typescript",
          "version": 1,
          "text": "import { a } from \"./b.ts\";\nconsole.log(a);\n",
        }
      }),
    )
    .unwrap();
  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client
    .write_response(id, json!([{ "enable": true }]))
    .unwrap();

  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": c_specifier,
        },
        "position": { "line": 0, "character": 10 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "(alias) const a: \"a\"\nimport a"
        },
        ""
      ],
      "range": {
        "start": { "line": 0, "character": 9 },
        "end": { "line": 0, "character": 10 }
      }
    }))
  );
  client
    .write_notification(
      "textDocument/didClose",
      json!({
        "textDocument": {
          "uri": b_specifier,
        }
      }),
    )
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": c_specifier,
        },
        "position": { "line": 0, "character": 10 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "(alias) const a: \"a\"\nimport a"
        },
        ""
      ],
      "range": {
        "start": { "line": 0, "character": 9 },
        "end": { "line": 0, "character": 10 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_dependency() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file_01.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export const a = \"a\";\n",
      }
    }),
  );
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://127.0.0.1:4545/xTypeScriptTypes.js\";\n// @deno-types=\"http://127.0.0.1:4545/type_definitions/foo.d.ts\"\nimport * as b from \"http://127.0.0.1:4545/type_definitions/foo.js\";\nimport * as c from \"http://127.0.0.1:4545/subdir/type_reference.js\";\nimport * as d from \"http://127.0.0.1:4545/subdir/mod1.ts\";\nimport * as e from \"data:application/typescript;base64,ZXhwb3J0IGNvbnN0IGEgPSAiYSI7CgpleHBvcnQgZW51bSBBIHsKICBBLAogIEIsCiAgQywKfQo=\";\nimport * as f from \"./file_01.ts\";\nimport * as g from \"http://localhost:4545/x/a/mod.ts\";\n\nconsole.log(a, b, c, d, e, f, g);\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 0, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://127.0.0.1:4545/xTypeScriptTypes.js\n\n**Types**: http&#8203;://127.0.0.1:4545/xTypeScriptTypes.d.ts\n"
      },
      "range": {
        "start": { "line": 0, "character": 19 },
        "end":{ "line": 0, "character": 62 }
      }
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 3, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://127.0.0.1:4545/subdir/type_reference.js\n\n**Types**: http&#8203;://127.0.0.1:4545/subdir/type_reference.d.ts\n"
      },
      "range": {
        "start": { "line": 3, "character": 19 },
        "end":{ "line": 3, "character": 67 }
      }
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 4, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://127.0.0.1:4545/subdir/mod1.ts\n"
      },
      "range": {
        "start": { "line": 4, "character": 19 },
        "end":{ "line": 4, "character": 57 }
      }
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 5, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: _(a data url)_\n"
      },
      "range": {
        "start": { "line": 5, "character": 19 },
        "end":{ "line": 5, "character": 132 }
      }
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 6, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: file&#8203;:///a/file_01.ts\n"
      },
      "range": {
        "start": { "line": 6, "character": 19 },
        "end":{ "line": 6, "character": 33 }
      }
    }))
  );
}

// This tests for a regression covered by denoland/deno#12753 where the lsp was
// unable to resolve dependencies when there was an invalid syntax in the module
#[test]
fn lsp_hover_deps_preserved_when_invalid_parse() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file1.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export type Foo = { bar(): string };\n"
      }
    }),
  );
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file2.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import { Foo } from './file1.ts'; declare const f: Foo; f\n"
      }
    }),
  );
  let (maybe_res, maybe_error) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file2.ts"
        },
        "position": { "line": 0, "character": 56 }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "const f: Foo",
        },
        ""
      ],
      "range": {
        "start": { "line": 0, "character": 56, },
        "end": { "line": 0, "character": 57, }
      }
    }))
  );
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file2.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 0, "character": 57 },
              "end": { "line": 0, "character": 58 }
            },
            "text": "."
          }
        ]
      }),
    )
    .unwrap();
  let (maybe_res, maybe_error) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file2.ts"
        },
        "position": { "line": 0, "character": 56 }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "const f: Foo",
        },
        ""
      ],
      "range": {
        "start": { "line": 0, "character": 56, },
        "end": { "line": 0, "character": 57, }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_hover_typescript_types() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://127.0.0.1:4545/xTypeScriptTypes.js\";\n\nconsole.log(a.foo);\n",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [
          {
            "uri": "http://127.0.0.1:4545/xTypeScriptTypes.js",
          }
        ],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 24 }
      }),
    )
    .unwrap();
  assert!(maybe_res.is_some());
  assert!(maybe_err.is_none());
  assert_eq!(
    json!(maybe_res.unwrap()),
    json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://127.0.0.1:4545/xTypeScriptTypes.js\n\n**Types**: http&#8203;://127.0.0.1:4545/xTypeScriptTypes.d.ts\n"
      },
      "range": {
        "start": { "line": 0, "character": 19 },
        "end": { "line": 0, "character": 62 }
      }
    })
  );
  client.shutdown();
}

#[test]
fn lsp_hover_jsdoc_symbol_link() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/b.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export function hello() {}\n"
      }
    }),
  );
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import { hello } from \"./b.ts\";\n\nhello();\n\nconst b = \"b\";\n\n/** JSDoc {@link hello} and {@linkcode b} */\nfunction a() {}\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 7, "character": 10 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": [
        {
          "language": "typescript",
          "value": "function a(): void"
        },
        "JSDoc [hello](file:///a/file.ts#L1,10) and [`b`](file:///a/file.ts#L5,7)"
      ],
      "range": {
        "start": { "line": 7, "character": 9 },
        "end": { "line": 7, "character": 10 }
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_goto_type_definition() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "interface A {\n  a: string;\n}\n\nexport class B implements A {\n  a = \"a\";\n  log() {\n    console.log(this.a);\n  }\n}\n\nconst b = new B();\nb;\n",
      }
    }),
  );
  let (maybe_res, maybe_error) = client
    .write_request::<_, _, Value>(
      "textDocument/typeDefinition",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 12, "character": 1 }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([
      {
        "targetUri": "file:///a/file.ts",
        "targetRange": {
          "start": { "line": 4, "character": 0 },
          "end": { "line": 9, "character": 1 }
        },
        "targetSelectionRange": {
          "start": { "line": 4, "character": 13 },
          "end": { "line": 4, "character": 14 }
        }
      }
    ]))
  );
  client.shutdown();
}

#[test]
fn lsp_call_hierarchy() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "function foo() {\n  return false;\n}\n\nclass Bar {\n  baz() {\n    return foo();\n  }\n}\n\nfunction main() {\n  const bar = new Bar();\n  bar.baz();\n}\n\nmain();"
      }
    }),
  );
  let (maybe_res, maybe_error) = client
    .write_request(
      "textDocument/prepareCallHierarchy",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 5, "character": 3 }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "name": "baz",
      "kind": 6,
      "detail": "Bar",
      "uri": "file:///a/file.ts",
      "range": {
        "start": { "line": 5, "character": 2 },
        "end": { "line": 7, "character": 3 }
      },
      "selectionRange": {
        "start": { "line": 5, "character": 2 },
        "end": { "line": 5, "character": 5 }
      }
    }]))
  );
  let (maybe_res, maybe_error) = client
    .write_request(
      "callHierarchy/incomingCalls",
      json!({
        "item": {
          "name": "baz",
          "kind": 6,
          "detail": "Bar",
          "uri": "file:///a/file.ts",
          "range": {
            "start": { "line": 5, "character": 2 },
            "end": { "line": 7, "character": 3 }
          },
          "selectionRange": {
            "start": { "line": 5, "character": 2 },
            "end": { "line": 5, "character": 5 }
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "from": {
        "name": "main",
        "kind": 12,
        "detail": "",
        "uri": "file:///a/file.ts",
        "range": {
          "start": { "line": 10, "character": 0 },
          "end": { "line": 13, "character": 1 }
        },
        "selectionRange": {
          "start": { "line": 10, "character": 9 },
          "end": { "line": 10, "character": 13 }
        }
      },
      "fromRanges": [
        {
          "start": { "line": 12, "character": 6 },
          "end": { "line": 12, "character": 9 }
        }
      ]
    }]))
  );
  let (maybe_res, maybe_error) = client
    .write_request(
      "callHierarchy/outgoingCalls",
      json!({
        "item": {
          "name": "baz",
          "kind": 6,
          "detail": "Bar",
          "uri": "file:///a/file.ts",
          "range": {
            "start": { "line": 5, "character": 2 },
            "end": { "line": 7, "character": 3 }
          },
          "selectionRange": {
            "start": { "line": 5, "character": 2 },
            "end": { "line": 5, "character": 5 }
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_error.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "to": {
        "name": "foo",
        "kind": 12,
        "detail": "",
        "uri": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 2, "character": 1 }
        },
        "selectionRange": {
          "start": { "line": 0, "character": 9 },
          "end": { "line": 0, "character": 12 }
        }
      },
      "fromRanges": [{
        "start": { "line": 6, "character": 11 },
        "end": { "line": 6, "character": 14 }
      }]
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_large_doc_changes() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let large_file_text =
    fs::read_to_string(testdata_path().join("lsp").join("large_file.txt"))
      .unwrap();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "javascript",
        "version": 1,
        "text": large_file_text,
      }
    }),
  );
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 444, "character": 11 },
              "end": { "line": 444, "character": 14 }
            },
            "text": "+++"
          }
        ]
      }),
    )
    .unwrap();
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 445, "character": 4 },
              "end": { "line": 445, "character": 4 }
            },
            "text": "// "
          }
        ]
      }),
    )
    .unwrap();
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 477, "character": 4 },
              "end": { "line": 477, "character": 9 }
            },
            "text": "error"
          }
        ]
      }),
    )
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 421, "character": 30 }
      }),
    )
    .unwrap();
  assert!(maybe_res.is_some());
  assert!(maybe_err.is_none());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 444, "character": 6 }
      }),
    )
    .unwrap();
  assert!(maybe_res.is_some());
  assert!(maybe_err.is_none());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 461, "character": 34 }
      }),
    )
    .unwrap();
  assert!(maybe_res.is_some());
  assert!(maybe_err.is_none());
  client.shutdown();

  assert!(client.duration().as_millis() <= 15000);
}

#[test]
fn lsp_document_symbol() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "interface IFoo {\n  foo(): boolean;\n}\n\nclass Bar implements IFoo {\n  constructor(public x: number) { }\n  foo() { return true; }\n  /** @deprecated */\n  baz() { return false; }\n  get value(): number { return 0; }\n  set value(newVavlue: number) { return; }\n  static staticBar = new Bar(0);\n  private static getStaticBar() { return Bar.staticBar; }\n}\n\nenum Values { value1, value2 }\n\nvar bar: IFoo = new Bar(3);"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/documentSymbol",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "name": "bar",
      "kind": 13,
      "range": {
        "start": { "line": 17, "character": 4 },
        "end": { "line": 17, "character": 26 }
      },
      "selectionRange": {
        "start": { "line": 17, "character": 4 },
        "end": { "line": 17, "character": 7 }
      }
    }, {
      "name": "Bar",
      "kind": 5,
      "range": {
        "start": { "line": 4, "character": 0 },
        "end": { "line": 13, "character": 1 }
      },
      "selectionRange": {
        "start": { "line": 4, "character": 6 },
        "end": { "line": 4, "character": 9 }
      },
      "children": [{
        "name": "constructor",
        "kind": 9,
        "range": {
          "start": { "line": 5, "character": 2 },
          "end": { "line": 5, "character": 35 }
        },
        "selectionRange": {
          "start": { "line": 5, "character": 2 },
          "end": { "line": 5, "character": 35 }
        }
      }, {
        "name": "baz",
        "kind": 6,
        "tags": [1],
        "range": {
          "start": { "line": 8, "character": 2 },
          "end": { "line": 8, "character": 25 }
        },
        "selectionRange": {
          "start": { "line": 8, "character": 2 },
          "end": { "line": 8, "character": 5 }
        }
      }, {
        "name": "foo",
        "kind": 6,
        "range": {
          "start": { "line": 6, "character": 2 },
          "end": { "line": 6, "character": 24 }
        },
        "selectionRange": {
          "start": { "line": 6, "character": 2 },
          "end": { "line": 6, "character": 5 }
        }
      }, {
        "name": "getStaticBar",
        "kind": 6,
        "range": {
          "start": { "line": 12, "character": 2 },
          "end": { "line": 12, "character": 57 }
        },
        "selectionRange": {
          "start": { "line": 12, "character": 17 },
          "end": { "line": 12, "character": 29 }
        }
      }, {
        "name": "staticBar",
        "kind": 8,
        "range": {
          "start": { "line": 11, "character": 2 },
          "end": { "line": 11, "character": 32 }
        },
        "selectionRange": {
          "start": { "line": 11, "character": 9 },
          "end": { "line": 11, "character": 18 }
        }
      }, {
        "name": "(get) value",
        "kind": 8,
        "range": {
          "start": { "line": 9, "character": 2 },
          "end": { "line": 9, "character": 35 }
        },
        "selectionRange": {
          "start": { "line": 9, "character": 6 },
          "end": { "line": 9, "character": 11 }
        }
      }, {
        "name": "(set) value",
        "kind": 8,
        "range": {
          "start": { "line": 10, "character": 2 },
          "end": { "line": 10, "character": 42 }
        },
        "selectionRange": {
          "start": { "line": 10, "character": 6 },
          "end": { "line": 10, "character": 11 }
        }
      }, {
        "name": "x",
        "kind": 8,
        "range": {
          "start": { "line": 5, "character": 14 },
          "end": { "line": 5, "character": 30 }
        },
        "selectionRange": {
          "start": { "line": 5, "character": 21 },
          "end": { "line": 5, "character": 22 }
        }
      }]
    }, {
      "name": "IFoo",
      "kind": 11,
      "range": {
        "start": { "line": 0, "character": 0 },
        "end": { "line": 2, "character": 1 }
      },
      "selectionRange": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 14 }
      },
      "children": [{
        "name": "foo",
        "kind": 6,
        "range": {
          "start": { "line": 1, "character": 2 },
          "end": { "line": 1, "character": 17 }
        },
        "selectionRange": {
          "start": { "line": 1, "character": 2 },
          "end": { "line": 1, "character": 5 }
        }
      }]
    }, {
      "name": "Values",
      "kind": 10,
      "range": {
        "start": { "line": 15, "character": 0 },
        "end": { "line": 15, "character": 30 }
      },
      "selectionRange": {
        "start": { "line": 15, "character": 5 },
        "end": { "line": 15, "character": 11 }
      },
      "children": [{
        "name": "value1",
        "kind": 22,
        "range": {
          "start": { "line": 15, "character": 14 },
          "end": { "line": 15, "character": 20 }
        },
        "selectionRange": {
          "start": { "line": 15, "character": 14 },
          "end": { "line": 15, "character": 20 }
        }
      }, {
        "name": "value2",
        "kind": 22,
        "range": {
          "start": { "line": 15, "character": 22 },
          "end": { "line": 15, "character": 28 }
        },
        "selectionRange": {
          "start": { "line": 15, "character": 22 },
          "end": { "line": 15, "character": 28 }
        }
      }]
    }]
    ))
  );
  client.shutdown();
}

#[test]
fn lsp_folding_range() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "// #region 1\n/*\n * Some comment\n */\nclass Foo {\n  bar(a, b) {\n    if (a === b) {\n      return true;\n    }\n    return false;\n  }\n}\n// #endregion"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/foldingRange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "startLine": 0,
      "endLine": 12,
      "kind": "region"
    }, {
      "startLine": 1,
      "endLine": 3,
      "kind": "comment"
    }, {
      "startLine": 4,
      "endLine": 10
    }, {
      "startLine": 5,
      "endLine": 9
    }, {
      "startLine": 6,
      "endLine": 7
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_rename() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        // this should not rename in comments and strings
        "text": "let variable = 'a'; // variable\nconsole.log(variable);\n\"variable\";\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/rename",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 4 },
        "newName": "variable_modified"
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "documentChanges": [{
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 1
        },
        "edits": [{
          "range": {
            "start": { "line": 0, "character": 4 },
            "end": { "line": 0, "character": 12 }
          },
          "newText": "variable_modified"
        }, {
          "range": {
            "start": { "line": 1, "character": 12 },
            "end": { "line": 1, "character": 20 }
          },
          "newText": "variable_modified"
        }]
      }]
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_selection_range() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "class Foo {\n  bar(a, b) {\n    if (a === b) {\n      return true;\n    }\n    return false;\n  }\n}"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/selectionRange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "positions": [{ "line": 2, "character": 8 }]
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "range": {
        "start": { "line": 2, "character": 8 },
        "end": { "line": 2, "character": 9 }
      },
      "parent": {
        "range": {
          "start": { "line": 2, "character": 8 },
          "end": { "line": 2, "character": 15 }
        },
        "parent": {
          "range": {
            "start": { "line": 2, "character": 4 },
            "end": { "line": 4, "character": 5 }
          },
          "parent": {
            "range": {
              "start": { "line": 1, "character": 13 },
              "end": { "line": 6, "character": 2 }
            },
            "parent": {
              "range": {
                "start": { "line": 1, "character": 12 },
                "end": { "line": 6, "character": 3 }
              },
              "parent": {
                "range": {
                  "start": { "line": 1, "character": 2 },
                  "end": { "line": 6, "character": 3 }
                },
                "parent": {
                  "range": {
                    "start": { "line": 0, "character": 11 },
                    "end": { "line": 7, "character": 0 }
                  },
                  "parent": {
                    "range": {
                      "start": { "line": 0, "character": 0 },
                      "end": { "line": 7, "character": 1 }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_semantic_tokens() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "enum Values { value1, value2 }\n\nasync function baz(s: string): Promise<string> {\n  const r = s.slice(0);\n  return r;\n}\n\ninterface IFoo {\n  readonly x: number;\n  foo(): boolean;\n}\n\nclass Bar implements IFoo {\n  constructor(public readonly x: number) { }\n  foo() { return true; }\n  static staticBar = new Bar(0);\n  private static getStaticBar() { return Bar.staticBar; }\n}\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/semanticTokens/full",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "data": [
        0, 5, 6, 1, 1, 0, 9, 6, 8, 9, 0, 8, 6, 8, 9, 2, 15, 3, 10, 5, 0, 4, 1,
        6, 1, 0, 12, 7, 2, 16, 1, 8, 1, 7, 41, 0, 4, 1, 6, 0, 0, 2, 5, 11, 16,
        1, 9, 1, 7, 40, 3, 10, 4, 2, 1, 1, 11, 1, 9, 9, 1, 2, 3, 11, 1, 3, 6, 3,
        0, 1, 0, 15, 4, 2, 0, 1, 30, 1, 6, 9, 1, 2, 3, 11,1, 1, 9, 9, 9, 3, 0,
        16, 3, 0, 0, 1, 17, 12, 11, 3, 0, 24, 3, 0, 0, 0, 4, 9, 9, 2
      ]
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/semanticTokens/range",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 6, "character": 0 }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "data": [
        0, 5, 6, 1, 1, 0, 9, 6, 8, 9, 0, 8, 6, 8, 9, 2, 15, 3, 10, 5, 0, 4, 1,
        6, 1, 0, 12, 7, 2, 16, 1, 8, 1, 7, 41, 0, 4, 1, 6, 0, 0, 2, 5, 11, 16,
        1, 9, 1, 7, 40
      ]
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_code_lens() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "class A {\n  a = \"a\";\n\n  b() {\n    console.log(this.a);\n  }\n\n  c() {\n    this.a = \"c\";\n  }\n}\n\nconst a = new A();\na.b();\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "range": {
        "start": { "line": 0, "character": 6 },
        "end": { "line": 0, "character": 7 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 1, "character": 2 },
        "end": { "line": 1, "character": 3 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }]))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "codeLens/resolve",
      json!({
        "range": {
          "start": { "line": 0, "character": 6 },
          "end": { "line": 0, "character": 7 }
        },
        "data": {
          "specifier": "file:///a/file.ts",
          "source": "references"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "range": {
        "start": { "line": 0, "character": 6 },
        "end": { "line": 0, "character": 7 }
      },
      "command": {
        "title": "2 references",
        "command": "deno.showReferences",
        "arguments": [
          "file:///a/file.ts",
          { "line": 0, "character": 6 },
          [{
            "uri": "file:///a/file.ts",
            "range": {
              "start": { "line": 0, "character": 6 },
              "end": { "line": 0, "character": 7 }
            }
          }, {
            "uri": "file:///a/file.ts",
            "range": {
              "start": { "line": 12, "character": 14 },
              "end": { "line": 12, "character": 15 }
            }
          }]
        ]
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_code_lens_impl() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "interface A {\n  b(): void;\n}\n\nclass B implements A {\n  b() {\n    console.log(\"b\");\n  }\n}\n\ninterface C {\n  c: string;\n}\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([ {
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "implementations"
      }
    }, {
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 4, "character": 6 },
        "end": { "line": 4, "character": 7 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 10, "character": 10 },
        "end": { "line": 10, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "implementations"
      }
    }, {
      "range": {
        "start": { "line": 10, "character": 10 },
        "end": { "line": 10, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 11, "character": 2 },
        "end": { "line": 11, "character": 3 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }]))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "codeLens/resolve",
      json!({
        "range": {
          "start": { "line": 0, "character": 10 },
          "end": { "line": 0, "character": 11 }
        },
        "data": {
          "specifier": "file:///a/file.ts",
          "source": "implementations"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "command": {
        "title": "1 implementation",
        "command": "deno.showReferences",
        "arguments": [
          "file:///a/file.ts",
          { "line": 0, "character": 10 },
          [{
            "uri": "file:///a/file.ts",
            "range": {
              "start": { "line": 4, "character": 6 },
              "end": { "line": 4, "character": 7 }
            }
          }]
        ]
      }
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "codeLens/resolve",
      json!({
        "range": {
          "start": { "line": 10, "character": 10 },
          "end": { "line": 10, "character": 11 }
        },
        "data": {
          "specifier": "file:///a/file.ts",
          "source": "implementations"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "range": {
        "start": { "line": 10, "character": 10 },
        "end": { "line": 10, "character": 11 }
      },
      "command": {
        "title": "0 implementations",
        "command": ""
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_code_lens_test() {
  let mut client = LspClientBuilder::new().build();
  client.initialize(|builder| {
    builder.disable_testing_api().set_code_lens(None);
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "const { test } = Deno;\nconst { test: test2 } = Deno;\nconst test3 = Deno.test;\n\nDeno.test(\"test a\", () => {});\nDeno.test({\n  name: \"test b\",\n  fn() {},\n});\ntest({\n  name: \"test c\",\n  fn() {},\n});\ntest(\"test d\", () => {});\ntest2({\n  name: \"test e\",\n  fn() {},\n});\ntest2(\"test f\", () => {});\ntest3({\n  name: \"test g\",\n  fn() {},\n});\ntest3(\"test h\", () => {});\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "range": {
        "start": { "line": 4, "character": 5 },
        "end": { "line": 4, "character": 9 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test a",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 4, "character": 5 },
        "end": { "line": 4, "character": 9 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test a",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 5, "character": 5 },
        "end": { "line": 5, "character": 9 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test b",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 5, "character": 5 },
        "end": { "line": 5, "character": 9 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test b",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 9, "character": 0 },
        "end": { "line": 9, "character": 4 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test c",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 9, "character": 0 },
        "end": { "line": 9, "character": 4 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test c",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 13, "character": 0 },
        "end": { "line": 13, "character": 4 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test d",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 13, "character": 0 },
        "end": { "line": 13, "character": 4 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test d",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 14, "character": 0 },
        "end": { "line": 14, "character": 5 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test e",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 14, "character": 0 },
        "end": { "line": 14, "character": 5 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test e",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 18, "character": 0 },
        "end": { "line": 18, "character": 5 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test f",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 18, "character": 0 },
        "end": { "line": 18, "character": 5 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test f",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 19, "character": 0 },
        "end": { "line": 19, "character": 5 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test g",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 19, "character": 0 },
        "end": { "line": 19, "character": 5 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test g",
          { "inspect": true }
        ]
      }
    }, {
      "range": {
        "start": { "line": 23, "character": 0 },
        "end": { "line": 23, "character": 5 }
      },
      "command": {
        "title": "▶︎ Run Test",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test h",
          { "inspect": false }
        ]
      }
    }, {
      "range": {
        "start": { "line": 23, "character": 0 },
        "end": { "line": 23, "character": 5 }
      },
      "command": {
        "title": "Debug",
        "command": "deno.test",
        "arguments": [
          "file:///a/file.ts",
          "test h",
          { "inspect": true }
        ]
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_code_lens_test_disabled() {
  let mut client = LspClientBuilder::new().build();
  client.initialize(|builder| {
    builder.disable_testing_api().set_code_lens(Some(json!({
      "implementations": true,
      "references": true,
      "test": false
    })));
  });
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "languageId": "typescript",
          "version": 1,
          "text": "const { test } = Deno;\nconst { test: test2 } = Deno;\nconst test3 = Deno.test;\n\nDeno.test(\"test a\", () => {});\nDeno.test({\n  name: \"test b\",\n  fn() {},\n});\ntest({\n  name: \"test c\",\n  fn() {},\n});\ntest(\"test d\", () => {});\ntest2({\n  name: \"test e\",\n  fn() {},\n});\ntest2(\"test f\", () => {});\ntest3({\n  name: \"test g\",\n  fn() {},\n});\ntest3(\"test h\", () => {});\n"
        }
      }),
    )
    .unwrap();

  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client
    .write_response(
      id,
      json!([{
        "enable": true,
        "codeLens": {
          "test": false
        }
      }]),
    )
    .unwrap();

  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (method, _) = client.read_notification::<Value>().unwrap();
  assert_eq!(method, "textDocument/publishDiagnostics");
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!([])));
  client.shutdown();
}

#[test]
fn lsp_code_lens_non_doc_nav_tree() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Date.now());\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/references",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 3 },
        "context": {
          "includeDeclaration": true
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/virtualTextDocument",
      json!({
        "textDocument": {
          "uri": "deno:/asset/lib.deno.shared_globals.d.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Vec<lsp::CodeLens>>(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "deno:/asset/lib.deno.shared_globals.d.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let res = maybe_res.unwrap();
  assert!(res.len() > 50);
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, lsp::CodeLens>(
      "codeLens/resolve",
      json!({
        "range": {
          "start": { "line": 416, "character": 12 },
          "end": { "line": 416, "character": 19 }
        },
        "data": {
          "specifier": "asset:///lib.deno.shared_globals.d.ts",
          "source": "references"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  client.shutdown();
}

#[test]
fn lsp_nav_tree_updates() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "interface A {\n  b(): void;\n}\n\nclass B implements A {\n  b() {\n    console.log(\"b\");\n  }\n}\n\ninterface C {\n  c: string;\n}\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(Some(json!([ {
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "implementations"
      }
    }, {
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 4, "character": 6 },
        "end": { "line": 4, "character": 7 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 10, "character": 10 },
        "end": { "line": 10, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "implementations"
      }
    }, {
      "range": {
        "start": { "line": 10, "character": 10 },
        "end": { "line": 10, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 11, "character": 2 },
        "end": { "line": 11, "character": 3 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }])))
  );
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 10, "character": 0 },
              "end": { "line": 13, "character": 0 }
            },
            "text": ""
          }
        ]
      }),
    )
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeLens",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "implementations"
      }
    }, {
      "range": {
        "start": { "line": 0, "character": 10 },
        "end": { "line": 0, "character": 11 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }, {
      "range": {
        "start": { "line": 4, "character": 6 },
        "end": { "line": 4, "character": 7 }
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "source": "references"
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_signature_help() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "/**\n * Adds two numbers.\n * @param a This is a first number.\n * @param b This is a second number.\n */\nfunction add(a: number, b: number) {\n  return a + b;\n}\n\nadd("
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/signatureHelp",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "character": 4, "line": 9 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "(",
          "isRetrigger": false
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "signatures": [
        {
          "label": "add(a: number, b: number): number",
          "documentation": {
            "kind": "markdown",
            "value": "Adds two numbers."
          },
          "parameters": [
            {
              "label": "a: number",
              "documentation": {
                "kind": "markdown",
                "value": "This is a first number."
              }
            }, {
              "label": "b: number",
              "documentation": {
                "kind": "markdown",
                "value": "This is a second number."
              }
            }
          ]
        }
      ],
      "activeSignature": 0,
      "activeParameter": 0
    }))
  );
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 9, "character": 4 },
              "end": { "line": 9, "character": 4 }
            },
            "text": "123, "
          }
        ]
      }),
    )
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/signatureHelp",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "character": 8, "line": 9 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "signatures": [
        {
          "label": "add(a: number, b: number): number",
          "documentation": {
            "kind": "markdown",
            "value": "Adds two numbers."
          },
          "parameters": [
            {
              "label": "a: number",
              "documentation": {
                "kind": "markdown",
                "value": "This is a first number."
              }
            }, {
              "label": "b: number",
              "documentation": {
                "kind": "markdown",
                "value": "This is a second number."
              }
            }
          ]
        }
      ],
      "activeSignature": 0,
      "activeParameter": 1
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_code_actions() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export function a(): void {\n  await Promise.resolve(\"a\");\n}\n\nexport function b(): void {\n  await Promise.resolve(\"b\");\n}\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 1, "character": 2 },
          "end": { "line": 1, "character": 7 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 1, "character": 2 },
              "end": { "line": 1, "character": 7 }
            },
            "severity": 1,
            "code": 1308,
            "source": "deno-ts",
            "message": "'await' expressions are only allowed within async functions and at the top levels of modules.",
            "relatedInformation": []
          }],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Add async modifier to containing function",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 1, "character": 2 },
          "end": { "line": 1, "character": 7 }
        },
        "severity": 1,
        "code": 1308,
        "source": "deno-ts",
        "message": "'await' expressions are only allowed within async functions and at the top levels of modules.",
        "relatedInformation": []
      }],
      "edit": {
        "documentChanges": [{
          "textDocument": {
            "uri": "file:///a/file.ts",
            "version": 1
          },
          "edits": [{
            "range": {
              "start": { "line": 0, "character": 7 },
              "end": { "line": 0, "character": 7 }
            },
            "newText": "async "
          }, {
            "range": {
              "start": { "line": 0, "character": 21 },
              "end": { "line": 0, "character": 25 }
            },
            "newText": "Promise<void>"
          }]
        }]
      }
    }, {
      "title": "Add all missing 'async' modifiers",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 1, "character": 2 },
          "end": { "line": 1, "character": 7 }
        },
        "severity": 1,
        "code": 1308,
        "source": "deno-ts",
        "message": "'await' expressions are only allowed within async functions and at the top levels of modules.",
        "relatedInformation": []
      }],
      "data": {
        "specifier": "file:///a/file.ts",
        "fixId": "fixAwaitInSyncFunction"
      }
    }]))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "codeAction/resolve",
      json!({
        "title": "Add all missing 'async' modifiers",
        "kind": "quickfix",
        "diagnostics": [{
          "range": {
            "start": { "line": 1, "character": 2 },
            "end": { "line": 1, "character": 7 }
          },
          "severity": 1,
          "code": 1308,
          "source": "deno-ts",
          "message": "'await' expressions are only allowed within async functions and at the top levels of modules.",
          "relatedInformation": []
        }],
        "data": {
          "specifier": "file:///a/file.ts",
          "fixId": "fixAwaitInSyncFunction"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "title": "Add all missing 'async' modifiers",
      "kind": "quickfix",
      "diagnostics": [
        {
          "range": {
            "start": {
              "line": 1,
              "character": 2
            },
            "end": {
              "line": 1,
              "character": 7
            }
          },
          "severity": 1,
          "code": 1308,
          "source": "deno-ts",
          "message": "'await' expressions are only allowed within async functions and at the top levels of modules.",
          "relatedInformation": []
        }
      ],
      "edit": {
        "documentChanges": [{
          "textDocument": {
            "uri": "file:///a/file.ts",
            "version": 1
          },
          "edits": [{
            "range": {
              "start": { "line": 0, "character": 7 },
              "end": { "line": 0, "character": 7 }
            },
            "newText": "async "
          }, {
            "range": {
              "start": { "line": 0, "character": 21 },
              "end": { "line": 0, "character": 25 }
            },
            "newText": "Promise<void>"
          }, {
            "range": {
              "start": { "line": 4, "character": 7 },
              "end": { "line": 4, "character": 7 }
            },
            "newText": "async "
          }, {
            "range": {
              "start": { "line": 4, "character": 21 },
              "end": { "line": 4, "character": 25 }
            },
            "newText": "Promise<void>"
          }]
        }]
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "fixId": "fixAwaitInSyncFunction"
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_code_actions_deno_cache() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let mut session = TestSession::from_client(client);
  let diagnostics = session.did_open(json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"https://deno.land/x/a/mod.ts\";\n\nconsole.log(a);\n"
      }
    }));
  assert_eq!(
    diagnostics.with_source("deno"),
    serde_json::from_value(json!({
      "uri": "file:///a/file.ts",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 19 },
          "end": { "line": 0, "character": 49 }
        },
        "severity": 1,
        "code": "no-cache",
        "source": "deno",
        "message": "Uncached or missing remote URL: \"https://deno.land/x/a/mod.ts\".",
        "data": { "specifier": "https://deno.land/x/a/mod.ts" }
      }],
      "version": 1
    })).unwrap()
  );

  let (maybe_res, maybe_err) = session
    .client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 19 },
          "end": { "line": 0, "character": 49 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 0, "character": 19 },
              "end": { "line": 0, "character": 49 }
            },
            "severity": 1,
            "code": "no-cache",
            "source": "deno",
            "message": "Unable to load the remote module: \"https://deno.land/x/a/mod.ts\".",
            "data": {
              "specifier": "https://deno.land/x/a/mod.ts"
            }
          }],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Cache \"https://deno.land/x/a/mod.ts\" and its dependencies.",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 19 },
          "end": { "line": 0, "character": 49 }
        },
        "severity": 1,
        "code": "no-cache",
        "source": "deno",
        "message": "Unable to load the remote module: \"https://deno.land/x/a/mod.ts\".",
        "data": {
          "specifier": "https://deno.land/x/a/mod.ts"
        }
      }],
      "command": {
        "title": "",
        "command": "deno.cache",
        "arguments": [["https://deno.land/x/a/mod.ts"]]
      }
    }]))
  );
  session.shutdown_and_exit();
}

#[test]
fn lsp_code_actions_deno_cache_npm() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let mut session = TestSession::from_client(client);
  let diagnostics = session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "import chalk from \"npm:chalk\";\n\nconsole.log(chalk.green);\n"
    }
  }));
  assert_eq!(
    diagnostics.with_source("deno"),
    serde_json::from_value(json!({
      "uri": "file:///a/file.ts",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 18 },
          "end": { "line": 0, "character": 29 }
        },
        "severity": 1,
        "code": "no-cache-npm",
        "source": "deno",
        "message": "Uncached or missing npm package: \"chalk\".",
        "data": { "specifier": "npm:chalk" }
      }],
      "version": 1
    }))
    .unwrap()
  );

  let (maybe_res, maybe_err) = session
    .client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 18 },
          "end": { "line": 0, "character": 29 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 0, "character": 18 },
              "end": { "line": 0, "character": 29 }
            },
            "severity": 1,
            "code": "no-cache-npm",
            "source": "deno",
            "message": "Uncached or missing npm package: \"chalk\".",
            "data": { "specifier": "npm:chalk" }
          }],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Cache \"npm:chalk\" and its dependencies.",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 18 },
          "end": { "line": 0, "character": 29 }
        },
        "severity": 1,
        "code": "no-cache-npm",
        "source": "deno",
        "message": "Uncached or missing npm package: \"chalk\".",
        "data": { "specifier": "npm:chalk" }
      }],
      "command": {
        "title": "",
        "command": "deno.cache",
        "arguments": [["npm:chalk"]]
      }
    }]))
  );
  session.shutdown_and_exit();
}

#[test]
fn lsp_code_actions_imports() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let mut session = TestSession::from_client(client);
  session.did_open(json!({
      "textDocument": {
        "uri": "file:///a/file00.ts",
        "languageId": "typescript",
        "version": 1,
        "text": r#"export interface MallardDuckConfigOptions extends DuckConfigOptions {
  kind: "mallard";
}

export class MallardDuckConfig extends DuckConfig {
  constructor(options: MallardDuckConfigOptions) {
    super(options);
  }
}
"#
      }
    }));
  session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file01.ts",
      "languageId": "typescript",
      "version": 1,
      "text": r#"import { DuckConfigOptions } from "./file02.ts";

export class DuckConfig {
  readonly kind;
  constructor(options: DuckConfigOptions) {
    this.kind = options.kind;
  }
}
"#
    }
  }));
  session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file02.ts",
      "languageId": "typescript",
      "version": 1,
      "text": r#"export interface DuckConfigOptions {
  kind: string;
  quacks: boolean;
}
"#
    }
  }));

  let (maybe_res, maybe_err) = session
    .client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file00.ts"
        },
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 6, "character": 0 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 0, "character": 50 },
              "end": { "line": 0, "character": 67 }
            },
            "severity": 1,
            "code": 2304,
            "source": "deno-ts",
            "message": "Cannot find name 'DuckConfigOptions'."
          }, {
            "range": {
              "start": { "line": 4, "character": 39 },
              "end": { "line": 4, "character": 49 }
            },
            "severity": 1,
            "code": 2304,
            "source": "deno-ts",
            "message": "Cannot find name 'DuckConfig'."
          }],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Add import from \"./file02.ts\"",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 50 },
          "end": { "line": 0, "character": 67 }
        },
        "severity": 1,
        "code": 2304,
        "source": "deno-ts",
        "message": "Cannot find name 'DuckConfigOptions'."
      }],
      "edit": {
        "documentChanges": [{
          "textDocument": {
            "uri": "file:///a/file00.ts",
            "version": 1
          },
          "edits": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "import { DuckConfigOptions } from \"./file02.ts\";\n\n"
          }]
        }]
      }
    }, {
      "title": "Add all missing imports",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 50 },
          "end": { "line": 0, "character": 67 }
        },
        "severity": 1,
        "code": 2304,
        "source": "deno-ts",
        "message": "Cannot find name 'DuckConfigOptions'."
      }],
      "data": {
        "specifier": "file:///a/file00.ts",
        "fixId": "fixMissingImport"
      }
    }, {
      "title": "Add import from \"./file01.ts\"",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 4, "character": 39 },
          "end": { "line": 4, "character": 49 }
        },
        "severity": 1,
        "code": 2304,
        "source": "deno-ts",
        "message": "Cannot find name 'DuckConfig'."
      }],
      "edit": {
        "documentChanges": [{
          "textDocument": {
            "uri": "file:///a/file00.ts",
            "version": 1
          },
          "edits": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "import { DuckConfig } from \"./file01.ts\";\n\n"
          }]
        }]
      }
    }]))
  );
  let (maybe_res, maybe_err) = session
    .client
    .write_request(
      "codeAction/resolve",
      json!({
        "title": "Add all missing imports",
        "kind": "quickfix",
        "diagnostics": [{
          "range": {
            "start": { "line": 0, "character": 50 },
            "end": { "line": 0, "character": 67 }
          },
          "severity": 1,
          "code": 2304,
          "source": "deno-ts",
          "message": "Cannot find name 'DuckConfigOptions'."
        }, {
          "range": {
            "start": { "line": 4, "character": 39 },
            "end": { "line": 4, "character": 49 }
          },
          "severity": 1,
          "code": 2304,
          "source": "deno-ts",
          "message": "Cannot find name 'DuckConfig'."
        }],
        "data": {
          "specifier": "file:///a/file00.ts",
          "fixId": "fixMissingImport"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "title": "Add all missing imports",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 0, "character": 50 },
          "end": { "line": 0, "character": 67 }
        },
        "severity": 1,
        "code": 2304,
        "source": "deno-ts",
        "message": "Cannot find name 'DuckConfigOptions'."
      }, {
        "range": {
          "start": { "line": 4, "character": 39 },
          "end": { "line": 4, "character": 49 }
        },
        "severity": 1,
        "code": 2304,
        "source": "deno-ts",
        "message": "Cannot find name 'DuckConfig'."
      }],
      "edit": {
        "documentChanges": [{
          "textDocument": {
            "uri": "file:///a/file00.ts",
            "version": 1
          },
          "edits": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "import { DuckConfig } from \"./file01.ts\";\nimport { DuckConfigOptions } from \"./file02.ts\";\n\n"
          }]
        }]
      },
      "data": {
        "specifier": "file:///a/file00.ts",
        "fixId": "fixMissingImport"
      }
    }))
  );

  session.shutdown_and_exit();
}

#[test]
fn lsp_code_actions_refactor() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "var x: { a?: number; b?: string } = {};\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "context": {
          "diagnostics": [],
          "only": ["refactor"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Extract to function in module scope",
      "kind": "refactor.extract.function",
      "isPreferred": false,
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Extract Symbol",
        "actionName": "function_scope_0"
      }
    }, {
      "title": "Extract to constant in enclosing scope",
      "kind": "refactor.extract.constant",
      "isPreferred": false,
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Extract Symbol",
        "actionName": "constant_scope_0"
      }
    }, {
      "title": "Move to a new file",
      "kind": "refactor.move.newFile",
      "isPreferred": false,
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Move to a new file",
        "actionName": "Move to a new file"
      }
    }, {
      "title": "Convert default export to named export",
      "kind": "refactor.rewrite.export.named",
      "isPreferred": false,
      "disabled": {
        "reason": "This file already has a default export"
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Convert export",
        "actionName": "Convert default export to named export"
      }
    }, {
      "title": "Convert named export to default export",
      "kind": "refactor.rewrite.export.default",
      "isPreferred": false,
      "disabled": {
        "reason": "This file already has a default export"
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Convert export",
        "actionName": "Convert named export to default export"
      }
    }, {
      "title": "Convert namespace import to named imports",
      "kind": "refactor.rewrite.import.named",
      "isPreferred": false,
      "disabled": {
        "reason": "Selection is not an import declaration."
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Convert import",
        "actionName": "Convert namespace import to named imports"
      }
    }, {
      "title": "Convert named imports to default import",
      "kind": "refactor.rewrite.import.default",
      "isPreferred": false,
      "disabled": {
        "reason": "Selection is not an import declaration."
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Convert import",
        "actionName": "Convert named imports to default import"
      }
    }, {
      "title": "Convert named imports to namespace import",
      "kind": "refactor.rewrite.import.namespace",
      "isPreferred": false,
      "disabled": {
        "reason": "Selection is not an import declaration."
      },
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "refactorName": "Convert import",
        "actionName": "Convert named imports to namespace import"
      }
    }]))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "codeAction/resolve",
      json!({
        "title": "Extract to interface",
        "kind": "refactor.extract.interface",
        "isPreferred": true,
        "data": {
          "specifier": "file:///a/file.ts",
          "range": {
            "start": { "line": 0, "character": 7 },
            "end": { "line": 0, "character": 33 }
          },
          "refactorName": "Extract type",
          "actionName": "Extract to interface"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "title": "Extract to interface",
      "kind": "refactor.extract.interface",
      "edit": {
        "documentChanges": [{
          "textDocument": {
            "uri": "file:///a/file.ts",
            "version": 1
          },
          "edits": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "interface NewType {\n  a?: number;\n  b?: string;\n}\n\n"
          }, {
            "range": {
              "start": { "line": 0, "character": 7 },
              "end": { "line": 0, "character": 33 }
            },
            "newText": "NewType"
          }]
        }]
      },
      "isPreferred": true,
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 7 },
          "end": { "line": 0, "character": 33 }
        },
        "refactorName": "Extract type",
        "actionName": "Extract to interface"
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_code_actions_refactor_no_disabled_support() {
  let mut client = LspClientBuilder::new().build();
  client.initialize(|builder| {
    builder.with_capabilities(|c| {
      let doc = c.text_document.as_mut().unwrap();
      let code_action = doc.code_action.as_mut().unwrap();
      code_action.disabled_support = Some(false);
    });
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "interface A {\n  a: string;\n}\n\ninterface B {\n  b: string;\n}\n\nclass AB implements A, B {\n  a = \"a\";\n  b = \"b\";\n}\n\nnew AB().a;\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 14, "character": 0 }
        },
        "context": {
          "diagnostics": [],
          "only": ["refactor"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Extract to function in module scope",
      "kind": "refactor.extract.function",
      "isPreferred": false,
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 14, "character": 0 }
        },
        "refactorName": "Extract Symbol",
        "actionName": "function_scope_0"
      }
    }, {
      "title": "Move to a new file",
      "kind": "refactor.move.newFile",
      "isPreferred": false,
      "data": {
        "specifier": "file:///a/file.ts",
        "range": {
          "start": { "line": 0, "character": 0 },
          "end": { "line": 14, "character": 0 }
        },
        "refactorName": "Move to a new file",
        "actionName": "Move to a new file"
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_code_actions_deadlock() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let large_file_text =
    fs::read_to_string(testdata_path().join("lsp").join("large_file.txt"))
      .unwrap();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "languageId": "javascript",
          "version": 1,
          "text": large_file_text,
        }
      }),
    )
    .unwrap();
  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client
    .write_response(id, json!([{ "enable": true }]))
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/semanticTokens/full",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  read_diagnostics(&mut client);
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 444, "character": 11 },
              "end": { "line": 444, "character": 14 }
            },
            "text": "+++"
          }
        ]
      }),
    )
    .unwrap();
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 445, "character": 4 },
              "end": { "line": 445, "character": 4 }
            },
            "text": "// "
          }
        ]
      }),
    )
    .unwrap();
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 477, "character": 4 },
              "end": { "line": 477, "character": 9 }
            },
            "text": "error"
          }
        ]
      }),
    )
    .unwrap();
  // diagnostics only trigger after changes have elapsed in a separate thread,
  // so we need to delay the next messages a little bit to attempt to create a
  // potential for a deadlock with the codeAction
  std::thread::sleep(std::time::Duration::from_millis(50));
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 609, "character": 33, }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 441, "character": 33 },
          "end": { "line": 441, "character": 42 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 441, "character": 33 },
              "end": { "line": 441, "character": 42 }
            },
            "severity": 1,
            "code": 7031,
            "source": "deno-ts",
            "message": "Binding element 'debugFlag' implicitly has an 'any' type."
          }],
          "only": [ "quickfix" ]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());

  read_diagnostics(&mut client);

  client.shutdown();
}

#[test]
fn lsp_completions() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "Deno."
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 5 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "."
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert!(list.items.len() > 90);
  } else {
    panic!("unexpected response");
  }
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "build",
        "kind": 6,
        "sortText": "1",
        "insertTextFormat": 1,
        "data": {
          "tsc": {
            "specifier": "file:///a/file.ts",
            "position": 5,
            "name": "build",
            "useCodeSnippet": false
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "build",
      "kind": 6,
      "detail": "const Deno.build: {\n    target: string;\n    arch: \"x86_64\" | \"aarch64\";\n    os: \"darwin\" | \"linux\" | \"windows\" | \"freebsd\" | \"netbsd\" | \"aix\" | \"solaris\" | \"illumos\";\n    vendor: string;\n    env?: string | undefined;\n}",
      "documentation": {
        "kind": "markdown",
        "value": "Information related to the build of the current Deno runtime.\n\nUsers are discouraged from code branching based on this information, as\nassumptions about what is available in what build environment might change\nover time. Developers should specifically sniff out the features they\nintend to use.\n\nThe intended use for the information is for logging and debugging purposes.\n\n*@category* - Runtime Environment"
      },
      "sortText": "1",
      "insertTextFormat": 1
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_completions_private_fields() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": r#"class Foo { #myProperty = "value"; constructor() { this.# } }"#
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 57 },
        "context": {
          "triggerKind": 1
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert_eq!(list.items.len(), 1);
    let item = &list.items[0];
    assert_eq!(item.label, "#myProperty");
    assert!(!list.is_incomplete);
  } else {
    panic!("unexpected response");
  }
  client.shutdown();
}

#[test]
fn lsp_completions_optional() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "interface A {\n  b?: string;\n}\n\nconst o: A = {};\n\nfunction c(s: string) {}\n\nc(o.)"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 8, "character": 4 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "."
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "isIncomplete": false,
      "items": [
        {
          "label": "b?",
          "kind": 5,
          "sortText": "11",
          "filterText": "b",
          "insertText": "b",
          "commitCharacters": [".", ",", ";", "("],
          "data": {
            "tsc": {
              "specifier": "file:///a/file.ts",
              "position": 79,
              "name": "b",
              "useCodeSnippet": false
            }
          }
        }
      ]
    }))
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "b?",
        "kind": 5,
        "sortText": "1",
        "filterText": "b",
        "insertText": "b",
        "data": {
          "tsc": {
            "specifier": "file:///a/file.ts",
            "position": 79,
            "name": "b",
            "useCodeSnippet": false
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "b?",
      "kind": 5,
      "detail": "(property) A.b?: string | undefined",
      "documentation": {
        "kind": "markdown",
        "value": ""
      },
      "sortText": "1",
      "filterText": "b",
      "insertText": "b"
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_completions_auto_import() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/b.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export const foo = \"foo\";\n",
      }
    }),
  );
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export {};\n\n",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 2, "character": 0, },
        "context": {
          "triggerKind": 1,
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    if !list.items.iter().any(|item| item.label == "foo") {
      panic!("completions items missing 'foo' symbol");
    }
  } else {
    panic!("unexpected completion response");
  }
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "foo",
        "kind": 6,
        "sortText": "￿16",
        "commitCharacters": [
          ".",
          ",",
          ";",
          "("
        ],
        "data": {
          "tsc": {
            "specifier": "file:///a/file.ts",
            "position": 12,
            "name": "foo",
            "source": "./b",
            "data": {
              "exportName": "foo",
              "moduleSpecifier": "./b",
              "fileName": "file:///a/b.ts"
            },
            "useCodeSnippet": false
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "foo",
      "kind": 6,
      "detail": "const foo: \"foo\"",
      "documentation": {
        "kind": "markdown",
        "value": ""
      },
      "sortText": "￿16",
      "additionalTextEdits": [
        {
          "range": {
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 0 }
          },
          "newText": "import { foo } from \"./b.ts\";\n\n"
        }
      ]
    }))
  );
}

#[test]
fn lsp_completions_snippet() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/a.tsx",
        "languageId": "typescriptreact",
        "version": 1,
        "text": "function A({ type }: { type: string }) {\n  return type;\n}\n\nfunction B() {\n  return <A t\n}",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/a.tsx"
        },
        "position": { "line": 5, "character": 13, },
        "context": {
          "triggerKind": 1,
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert_eq!(
      json!(list),
      json!({
        "isIncomplete": false,
        "items": [
          {
            "label": "type",
            "kind": 5,
            "sortText": "11",
            "filterText": "type=\"$1\"",
            "insertText": "type=\"$1\"",
            "insertTextFormat": 2,
            "commitCharacters": [
              ".",
              ",",
              ";",
              "("
            ],
            "data": {
              "tsc": {
                "specifier": "file:///a/a.tsx",
                "position": 87,
                "name": "type",
                "useCodeSnippet": false
              }
            }
          }
        ]
      })
    );
  } else {
    panic!("unexpected completion response");
  }
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "type",
        "kind": 5,
        "sortText": "11",
        "filterText": "type=\"$1\"",
        "insertText": "type=\"$1\"",
        "insertTextFormat": 2,
        "commitCharacters": [
          ".",
          ",",
          ";",
          "("
        ],
        "data": {
          "tsc": {
            "specifier": "file:///a/a.tsx",
            "position": 87,
            "name": "type",
            "useCodeSnippet": false
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "type",
      "kind": 5,
      "detail": "(property) type: string",
      "documentation": {
        "kind": "markdown",
        "value": ""
      },
      "sortText": "11",
      "filterText": "type=\"$1\"",
      "insertText": "type=\"$1\"",
      "insertTextFormat": 2
    }))
  );
}

#[test]
fn lsp_completions_no_snippet() {
  let mut client = LspClientBuilder::new().build();
  client.initialize(|builder| {
    builder.with_capabilities(|c| {
      let doc = c.text_document.as_mut().unwrap();
      doc.completion = None;
    });
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/a.tsx",
        "languageId": "typescriptreact",
        "version": 1,
        "text": "function A({ type }: { type: string }) {\n  return type;\n}\n\nfunction B() {\n  return <A t\n}",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/a.tsx"
        },
        "position": { "line": 5, "character": 13, },
        "context": {
          "triggerKind": 1,
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert_eq!(
      json!(list),
      json!({
        "isIncomplete": false,
        "items": [
          {
            "label": "type",
            "kind": 5,
            "sortText": "11",
            "commitCharacters": [
              ".",
              ",",
              ";",
              "("
            ],
            "data": {
              "tsc": {
                "specifier": "file:///a/a.tsx",
                "position": 87,
                "name": "type",
                "useCodeSnippet": false
              }
            }
          }
        ]
      })
    );
  } else {
    panic!("unexpected completion response");
  }
}

#[test]
fn lsp_completions_npm() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import cjsDefault from 'npm:@denotest/cjs-default-export';import chalk from 'npm:chalk';\n\n",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [
          {
            "uri": "npm:@denotest/cjs-default-export",
          }, {
            "uri": "npm:chalk",
          }
        ]
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());

  // check importing a cjs default import
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 2, "character": 0 },
              "end": { "line": 2, "character": 0 }
            },
            "text": "cjsDefault."
          }
        ]
      }),
    )
    .unwrap();
  read_diagnostics(&mut client);

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 2, "character": 11 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "."
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert_eq!(list.items.len(), 3);
    assert!(list.items.iter().any(|i| i.label == "default"));
    assert!(list.items.iter().any(|i| i.label == "MyClass"));
  } else {
    panic!("unexpected response");
  }
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "MyClass",
        "kind": 6,
        "sortText": "1",
        "insertTextFormat": 1,
        "data": {
          "tsc": {
            "specifier": "file:///a/file.ts",
            "position": 69,
            "name": "MyClass",
            "useCodeSnippet": false
          }
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "MyClass",
      "kind": 6,
      "sortText": "1",
      "insertTextFormat": 1,
      "data": {
        "tsc": {
          "specifier": "file:///a/file.ts",
          "position": 69,
          "name": "MyClass",
          "useCodeSnippet": false
        }
      }
    }))
  );

  // now check chalk, which is esm
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 3
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 2, "character": 0 },
              "end": { "line": 2, "character": 11 }
            },
            "text": "chalk."
          }
        ]
      }),
    )
    .unwrap();
  read_diagnostics(&mut client);

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 2, "character": 6 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "."
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert!(list.items.iter().any(|i| i.label == "green"));
    assert!(list.items.iter().any(|i| i.label == "red"));
  } else {
    panic!("unexpected response");
  }

  client.shutdown();
}

#[test]
fn lsp_npm_specifier_unopened_file() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();

  // create other.ts, which re-exports an npm specifier
  client.deno_dir().write(
    "other.ts",
    "export { default as chalk } from 'npm:chalk@5';",
  );

  // cache the other.ts file to the DENO_DIR
  let deno = deno_cmd_with_deno_dir(client.deno_dir())
    .current_dir(client.deno_dir().path())
    .arg("cache")
    .arg("--quiet")
    .arg("other.ts")
    .envs(env_vars_for_npm_tests())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let output = deno.wait_with_output().unwrap();
  assert!(output.status.success());
  assert_eq!(output.status.code(), Some(0));

  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(stdout.is_empty());
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(stderr.is_empty());

  // open main.ts, which imports other.ts (unopened)
  let main_url =
    ModuleSpecifier::from_file_path(client.deno_dir().path().join("main.ts"))
      .unwrap();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": main_url,
        "languageId": "typescript",
        "version": 1,
        "text": "import { chalk } from './other.ts';\n\n",
      }
    }),
  );

  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": main_url,
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 2, "character": 0 },
              "end": { "line": 2, "character": 0 }
            },
            "text": "chalk."
          }
        ]
      }),
    )
    .unwrap();
  read_diagnostics(&mut client);

  // now ensure completions work
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": main_url
        },
        "position": { "line": 2, "character": 6 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "."
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert_eq!(list.items.len(), 63);
    assert!(list.items.iter().any(|i| i.label == "ansi256"));
  } else {
    panic!("unexpected response");
  }
}

#[test]
fn lsp_completions_node_specifier() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  let diagnostics = CollectedDiagnostics(did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import fs from 'node:non-existent';\n\n",
      }
    }),
  ));

  let non_existent_diagnostics = diagnostics
    .with_file_and_source("file:///a/file.ts", "deno")
    .diagnostics
    .into_iter()
    .filter(|d| {
      d.code == Some(lsp::NumberOrString::String("resolver-error".to_string()))
    })
    .collect::<Vec<_>>();
  assert_eq!(
    json!(non_existent_diagnostics),
    json!([
      {
        "range": {
          "start": { "line": 0, "character": 15 },
          "end": { "line": 0, "character": 34 },
        },
        "severity": 1,
        "code": "resolver-error",
        "source": "deno",
        "message": "Unknown Node built-in module: non-existent"
      }
    ])
  );

  // update to have fs import
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 0, "character": 16 },
              "end": { "line": 0, "character": 33 },
            },
            "text": "fs"
          }
        ]
      }),
    )
    .unwrap();
  let diagnostics = read_diagnostics(&mut client);
  let diagnostics = diagnostics
    .with_file_and_source("file:///a/file.ts", "deno")
    .diagnostics
    .into_iter()
    .filter(|d| {
      d.code
        == Some(lsp::NumberOrString::String(
          "import-node-prefix-missing".to_string(),
        ))
    })
    .collect::<Vec<_>>();

  // get the quick fixes
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 16 },
          "end": { "line": 0, "character": 18 },
        },
        "context": {
          "diagnostics": json!(diagnostics),
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Update specifier to node:fs",
      "kind": "quickfix",
      "diagnostics": [
        {
          "range": {
            "start": { "line": 0, "character": 15 },
            "end": { "line": 0, "character": 19 }
          },
          "severity": 1,
          "code": "import-node-prefix-missing",
          "source": "deno",
          "message": "Relative import path \"fs\" not prefixed with / or ./ or ../\nIf you want to use a built-in Node module, add a \"node:\" prefix (ex. \"node:fs\").",
          "data": {
            "specifier": "fs"
          },
        }
      ],
      "edit": {
        "changes": {
          "file:///a/file.ts": [
            {
              "range": {
                "start": { "line": 0, "character": 15 },
                "end": { "line": 0, "character": 19 }
              },
              "newText": "\"node:fs\""
            }
          ]
        }
      }
    }]))
  );

  // update to have node:fs import
  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 3,
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 0, "character": 15 },
              "end": { "line": 0, "character": 19 },
            },
            "text": "\"node:fs\"",
          }
        ]
      }),
    )
    .unwrap();

  let diagnostics = read_diagnostics(&mut client);
  let cache_diagnostics = diagnostics
    .with_file_and_source("file:///a/file.ts", "deno")
    .diagnostics
    .into_iter()
    .filter(|d| {
      d.code == Some(lsp::NumberOrString::String("no-cache-npm".to_string()))
    })
    .collect::<Vec<_>>();

  assert_eq!(
    json!(cache_diagnostics),
    json!([
      {
        "range": {
          "start": { "line": 0, "character": 15 },
          "end": { "line": 0, "character": 24 }
        },
        "data": {
          "specifier": "npm:@types/node",
        },
        "severity": 1,
        "code": "no-cache-npm",
        "source": "deno",
        "message": "Uncached or missing npm package: \"@types/node\"."
      }
    ])
  );

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [
          {
            "uri": "npm:@types/node",
          }
        ]
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());

  client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "version": 4
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 2, "character": 0 },
              "end": { "line": 2, "character": 0 }
            },
            "text": "fs."
          }
        ]
      }),
    )
    .unwrap();
  read_diagnostics(&mut client);

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 2, "character": 3 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "."
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert!(list.items.iter().any(|i| i.label == "writeFile"));
    assert!(list.items.iter().any(|i| i.label == "writeFileSync"));
  } else {
    panic!("unexpected response");
  }

  client.shutdown();
}

#[test]
fn lsp_completions_registry() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.add_test_server_suggestions();
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://localhost:4545/x/a@\""
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 46 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "@"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert_eq!(list.items.len(), 3);
  } else {
    panic!("unexpected response");
  }
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "v2.0.0",
        "kind": 19,
        "detail": "(version)",
        "sortText": "0000000003",
        "filterText": "http://localhost:4545/x/a@v2.0.0",
        "textEdit": {
          "range": {
            "start": { "line": 0, "character": 20 },
            "end": { "line": 0, "character": 46 }
          },
          "newText": "http://localhost:4545/x/a@v2.0.0"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "v2.0.0",
      "kind": 19,
      "detail": "(version)",
      "sortText": "0000000003",
      "filterText": "http://localhost:4545/x/a@v2.0.0",
      "textEdit": {
        "range": {
          "start": { "line": 0, "character": 20 },
          "end": { "line": 0, "character": 46 }
        },
        "newText": "http://localhost:4545/x/a@v2.0.0"
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_completions_registry_empty() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.add_test_server_suggestions();
  });
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"\""
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 20 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "\""
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "isIncomplete": false,
      "items": [{
        "label": ".",
        "kind": 19,
        "detail": "(local)",
        "sortText": "1",
        "insertText": ".",
        "commitCharacters": ["\"", "'"]
      }, {
        "label": "..",
        "kind": 19,
        "detail": "(local)",
        "sortText": "1",
        "insertText": "..",
        "commitCharacters": ["\"", "'" ]
      }, {
        "label": "http://localhost:4545",
        "kind": 19,
        "detail": "(registry)",
        "sortText": "2",
        "textEdit": {
          "range": {
            "start": { "line": 0, "character": 20 },
            "end": { "line": 0, "character": 20 }
          },
          "newText": "http://localhost:4545"
        },
        "commitCharacters": ["\"", "'", "/"]
      }]
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_auto_discover_registry() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://localhost:4545/x/a@\""
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 46 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "@"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (method, maybe_res) = client.read_notification().unwrap();
  assert_eq!(method, "deno/registryState");
  assert_eq!(
    maybe_res,
    Some(json!({
      "origin": "http://localhost:4545",
      "suggestions": true,
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_cache_location() {
  let context = TestContextBuilder::new().use_http_server().build();
  let temp_dir = context.deno_dir();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_cache(".cache").add_test_server_suggestions();
  });

  let mut session = TestSession::from_client(client);
  session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file_01.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "export const a = \"a\";\n",
    }
  }));
  let diagnostics =
    session.did_open(json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://127.0.0.1:4545/xTypeScriptTypes.js\";\n// @deno-types=\"http://127.0.0.1:4545/type_definitions/foo.d.ts\"\nimport * as b from \"http://127.0.0.1:4545/type_definitions/foo.js\";\nimport * as c from \"http://127.0.0.1:4545/subdir/type_reference.js\";\nimport * as d from \"http://127.0.0.1:4545/subdir/mod1.ts\";\nimport * as e from \"data:application/typescript;base64,ZXhwb3J0IGNvbnN0IGEgPSAiYSI7CgpleHBvcnQgZW51bSBBIHsKICBBLAogIEIsCiAgQywKfQo=\";\nimport * as f from \"./file_01.ts\";\nimport * as g from \"http://localhost:4545/x/a/mod.ts\";\n\nconsole.log(a, b, c, d, e, f, g);\n"
      }
    }));
  assert_eq!(diagnostics.viewed().len(), 7);
  let (maybe_res, maybe_err) = session
    .client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = session
    .client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 0, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://127.0.0.1:4545/xTypeScriptTypes.js\n\n**Types**: http&#8203;://127.0.0.1:4545/xTypeScriptTypes.d.ts\n"
      },
      "range": {
        "start": { "line": 0, "character": 19 },
        "end": { "line": 0, "character": 62 }
      }
    }))
  );
  let (maybe_res, maybe_err) = session
    .client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 7, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://localhost:4545/x/a/mod.ts\n\n\n---\n\n**a**\n\nmod.ts"
      },
      "range": {
        "start": { "line": 7, "character": 19 },
        "end": { "line": 7, "character": 53 }
      }
    }))
  );
  let cache_path = temp_dir.path().join(".cache");
  assert!(cache_path.is_dir());
  assert!(cache_path.join("gen").is_dir());
  session.shutdown_and_exit();
}

/// Sets the TLS root certificate on startup, which allows the LSP to connect to
/// the custom signed test server and be able to retrieve the registry config
/// and cache files.
#[test]
fn lsp_tls_cert() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder
      .set_suggest_imports_hosts(vec![
        ("http://localhost:4545/".to_string(), true),
        ("https://localhost:5545/".to_string(), true),
      ])
      .set_tls_certificate("");
  });

  let mut session = TestSession::from_client(client);

  session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file_01.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "export const a = \"a\";\n",
    }
  }));
  let diagnostics = session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "import * as a from \"https://localhost:5545/xTypeScriptTypes.js\";\n// @deno-types=\"https://localhost:5545/type_definitions/foo.d.ts\"\nimport * as b from \"https://localhost:5545/type_definitions/foo.js\";\nimport * as c from \"https://localhost:5545/subdir/type_reference.js\";\nimport * as d from \"https://localhost:5545/subdir/mod1.ts\";\nimport * as e from \"data:application/typescript;base64,ZXhwb3J0IGNvbnN0IGEgPSAiYSI7CgpleHBvcnQgZW51bSBBIHsKICBBLAogIEIsCiAgQywKfQo=\";\nimport * as f from \"./file_01.ts\";\nimport * as g from \"http://localhost:4545/x/a/mod.ts\";\n\nconsole.log(a, b, c, d, e, f, g);\n"
    }
  }));
  let diagnostics = diagnostics.viewed();
  assert_eq!(diagnostics.len(), 7);
  let (maybe_res, maybe_err) = session
    .client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = session
    .client
    .write_request(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 0, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: https&#8203;://localhost:5545/xTypeScriptTypes.js\n"
      },
      "range": {
        "start": { "line": 0, "character": 19 },
        "end": { "line": 0, "character": 63 }
      }
    }))
  );
  let (maybe_res, maybe_err) = session
    .client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
        },
        "position": { "line": 7, "character": 28 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://localhost:4545/x/a/mod.ts\n\n\n---\n\n**a**\n\nmod.ts"
      },
      "range": {
        "start": { "line": 7, "character": 19 },
        "end": { "line": 7, "character": 53 }
      }
    }))
  );
  session.shutdown_and_exit();
}

#[test]
fn lsp_diagnostics_warn_redirect() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://127.0.0.1:4545/x_deno_warning.js\";\n\nconsole.log(a)\n",
      },
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [
          {
            "uri": "http://127.0.0.1:4545/x_deno_warning.js",
          }
        ],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let diagnostics = read_diagnostics(&mut client);
  assert_eq!(
    diagnostics.with_source("deno"),
    lsp::PublishDiagnosticsParams {
      uri: Url::parse("file:///a/file.ts").unwrap(),
      diagnostics: vec![
        lsp::Diagnostic {
          range: lsp::Range {
            start: lsp::Position {
              line: 0,
              character: 19
            },
            end: lsp::Position {
              line: 0,
              character: 60
            }
          },
          severity: Some(lsp::DiagnosticSeverity::WARNING),
          code: Some(lsp::NumberOrString::String("deno-warn".to_string())),
          source: Some("deno".to_string()),
          message: "foobar".to_string(),
          ..Default::default()
        },
        lsp::Diagnostic {
          range: lsp::Range {
            start: lsp::Position {
              line: 0,
              character: 19
            },
            end: lsp::Position {
              line: 0,
              character: 60
            }
          },
          severity: Some(lsp::DiagnosticSeverity::INFORMATION),
          code: Some(lsp::NumberOrString::String("redirect".to_string())),
          source: Some("deno".to_string()),
          message: "The import of \"http://127.0.0.1:4545/x_deno_warning.js\" was redirected to \"http://127.0.0.1:4545/lsp/x_deno_warning_redirect.js\".".to_string(),
          data: Some(json!({"specifier": "http://127.0.0.1:4545/x_deno_warning.js", "redirect": "http://127.0.0.1:4545/lsp/x_deno_warning_redirect.js"})),
          ..Default::default()
        }
      ],
      version: Some(1),
    }
  );
  client.shutdown();
}

#[test]
fn lsp_redirect_quick_fix() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://127.0.0.1:4545/x_deno_warning.js\";\n\nconsole.log(a)\n",
      },
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.ts",
        },
        "uris": [
          {
            "uri": "http://127.0.0.1:4545/x_deno_warning.js",
          }
        ],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let diagnostics = read_diagnostics(&mut client)
    .with_source("deno")
    .diagnostics;
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!(json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 0, "character": 19 },
          "end": { "line": 0, "character": 60 }
        },
        "context": {
          "diagnostics": diagnostics,
          "only": ["quickfix"]
        }
      })),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Update specifier to its redirected specifier.",
      "kind": "quickfix",
      "diagnostics": [
        {
          "range": {
            "start": { "line": 0, "character": 19 },
            "end": { "line": 0, "character": 60 }
          },
          "severity": 3,
          "code": "redirect",
          "source": "deno",
          "message": "The import of \"http://127.0.0.1:4545/x_deno_warning.js\" was redirected to \"http://127.0.0.1:4545/lsp/x_deno_warning_redirect.js\".",
          "data": {
            "specifier": "http://127.0.0.1:4545/x_deno_warning.js",
            "redirect": "http://127.0.0.1:4545/lsp/x_deno_warning_redirect.js"
          }
        }
      ],
      "edit": {
        "changes": {
          "file:///a/file.ts": [
            {
              "range": {
                "start": { "line": 0, "character": 19 },
                "end": { "line": 0, "character": 60 }
              },
              "newText": "\"http://127.0.0.1:4545/lsp/x_deno_warning_redirect.js\""
            }
          ]
        }
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_diagnostics_deprecated() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "/** @deprecated */\nexport const a = \"a\";\n\na;\n",
      },
    }),
  );
  assert_eq!(
    json!(diagnostics),
    json!([
      {
        "uri": "file:///a/file.ts",
        "diagnostics": [],
        "version": 1
      }, {
        "uri": "file:///a/file.ts",
        "diagnostics": [],
        "version": 1
      }, {
        "uri": "file:///a/file.ts",
        "diagnostics": [
          {
            "range": {
              "start": { "line": 3, "character": 0 },
              "end": { "line": 3, "character": 1 }
            },
            "severity": 4,
            "code": 6385,
            "source": "deno-ts",
            "message": "'a' is deprecated.",
            "relatedInformation": [],
            "tags": [2]
          }
        ],
        "version": 1
      }
    ])
  );
  client.shutdown();
}

#[test]
fn lsp_diagnostics_deno_types() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "languageId": "typescript",
          "version": 1,
          "text": "/// <reference types=\"https://example.com/a/b.d.ts\" />\n/// <reference path=\"https://example.com/a/c.ts\"\n\n// @deno-types=https://example.com/a/d.d.ts\nimport * as d from \"https://example.com/a/d.js\";\n\n// @deno-types=\"https://example.com/a/e.d.ts\"\nimport * as e from \"https://example.com/a/e.js\";\n\nconsole.log(d, e);\n"
        }
      }),
    )
    .unwrap();
  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client
    .write_response(id, json!([{ "enable": true }]))
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/documentSymbol",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        }
      }),
    )
    .unwrap();
  assert!(maybe_res.is_some());
  assert!(maybe_err.is_none());
  let diagnostics = read_diagnostics(&mut client);
  assert_eq!(diagnostics.viewed().len(), 5);
  client.shutdown();
}

#[test]
fn lsp_diagnostics_refresh_dependents() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  let mut session = TestSession::from_client(client);
  session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file_00.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "export const a = \"a\";\n",
    },
  }));
  session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file_01.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "export * from \"./file_00.ts\";\n",
    },
  }));
  let diagnostics = session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file_02.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "import { a, b } from \"./file_01.ts\";\n\nconsole.log(a, b);\n"
    }
  }));
  assert_eq!(
    json!(diagnostics.with_file_and_source("file:///a/file_02.ts", "deno-ts")),
    json!({
      "uri": "file:///a/file_02.ts",
      "diagnostics": [
        {
          "range": {
            "start": { "line": 0, "character": 12 },
            "end": { "line": 0, "character": 13 }
          },
          "severity": 1,
          "code": 2305,
          "source": "deno-ts",
          "message": "Module '\"./file_01.ts\"' has no exported member 'b'."
        }
      ],
      "version": 1
    })
  );

  // fix the code causing the diagnostic
  session
    .client
    .write_notification(
      "textDocument/didChange",
      json!({
        "textDocument": {
          "uri": "file:///a/file_00.ts",
          "version": 2
        },
        "contentChanges": [
          {
            "range": {
              "start": { "line": 1, "character": 0 },
              "end": { "line": 1, "character": 0 }
            },
            "text": "export const b = \"b\";\n"
          }
        ]
      }),
    )
    .unwrap();
  let diagnostics = session.read_diagnostics();
  assert_eq!(diagnostics.viewed().len(), 0); // no diagnostics now

  session.shutdown_and_exit();
  assert_eq!(session.client.queue_len(), 0);
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerformanceAverage {
  pub name: String,
  pub count: u32,
  pub average_duration: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PerformanceAverages {
  averages: Vec<PerformanceAverage>,
}

#[test]
fn lsp_performance() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console.log(Deno.args);\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 19 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, PerformanceAverages>("deno/performance", json!(null))
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(res) = maybe_res {
    assert_eq!(res.averages.len(), 13);
  } else {
    panic!("unexpected result");
  }
  client.shutdown();
}

#[test]
fn lsp_format_no_changes() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console;\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));
  client.assert_no_notification("window/showMessage");
  client.shutdown();
}

#[test]
fn lsp_format_error() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "console test test\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));
  client.shutdown();
}

#[test]
fn lsp_format_mbc() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "const bar = '👍🇺🇸😃'\nconsole.log('hello deno')\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "range": {
        "start": { "line": 0, "character": 12 },
        "end": { "line": 0, "character": 13 }
      },
      "newText": "\""
    }, {
      "range": {
        "start": { "line": 0, "character": 21 },
        "end": { "line": 0, "character": 22 }
      },
      "newText": "\";"
    }, {
      "range": {
        "start": { "line": 1, "character": 12 },
        "end": { "line": 1, "character": 13 }
      },
      "newText": "\""
    }, {
      "range": {
        "start": { "line": 1, "character": 23 },
        "end": { "line": 1, "character": 25 }
      },
      "newText": "\");"
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_format_exclude_with_config() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  temp_dir.write(
    "deno.fmt.jsonc",
    r#"{
    "fmt": {
      "files": {
        "exclude": ["ignored.ts"]
      },
      "options": {
        "useTabs": true,
        "lineWidth": 40,
        "indentWidth": 8,
        "singleQuote": true,
        "proseWrap": "always"
      }
    }
  }"#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.fmt.jsonc");
  });

  let file_uri = temp_dir.uri().join("ignored.ts").unwrap();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": file_uri,
        "languageId": "typescript",
        "version": 1,
        "text": "function   myFunc(){}"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": file_uri
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));
  client.shutdown();
}

#[test]
fn lsp_format_exclude_default_config() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  temp_dir.write(
    "deno.fmt.jsonc",
    r#"{
    "fmt": {
      "files": {
        "exclude": ["ignored.ts"]
      },
      "options": {
        "useTabs": true,
        "lineWidth": 40,
        "indentWidth": 8,
        "singleQuote": true,
        "proseWrap": "always"
      }
    }
  }"#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.fmt.jsonc");
  });

  let file_uri = temp_dir.uri().join("ignored.ts").unwrap();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": file_uri,
        "languageId": "typescript",
        "version": 1,
        "text": "function   myFunc(){}"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": file_uri
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));
  client.shutdown();
}

#[test]
fn lsp_format_json() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          // Also test out using a non-json file extension here.
          // What should matter is the language identifier.
          "uri": "file:///a/file.lock",
          "languageId": "json",
          "version": 1,
          "text": "{\"key\":\"value\"}"
        }
      }),
    )
    .unwrap();

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/formatting",
      json!({
          "textDocument": {
            "uri": "file:///a/file.lock"
          },
          "options": {
            "tabSize": 2,
            "insertSpaces": true
          }
      }),
    )
    .unwrap();

  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([
      {
        "range": {
          "start": { "line": 0, "character": 1 },
          "end": { "line": 0, "character": 1 }
        },
        "newText": " "
      }, {
        "range": {
          "start": { "line": 0, "character": 7 },
          "end": { "line": 0, "character": 7 }
        },
        "newText": " "
      }, {
        "range": {
          "start": { "line": 0, "character": 14 },
          "end": { "line": 0, "character": 15 }
        },
        "newText": " }\n"
      }
    ]))
  );
  client.shutdown();
}

#[test]
fn lsp_json_no_diagnostics() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.json",
          "languageId": "json",
          "version": 1,
          "text": "{\"key\":\"value\"}"
        }
      }),
    )
    .unwrap();

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/semanticTokens/full",
      json!({
        "textDocument": {
          "uri": "file:///a/file.json"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.json"
        },
        "position": { "line": 0, "character": 3 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));

  client.shutdown();
}

#[test]
fn lsp_format_markdown() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.md",
          "languageId": "markdown",
          "version": 1,
          "text": "#   Hello World"
        }
      }),
    )
    .unwrap();

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": "file:///a/file.md"
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();

  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([
      {
        "range": {
          "start": { "line": 0, "character": 1 },
          "end": { "line": 0, "character": 3 }
        },
        "newText": ""
      }, {
        "range": {
          "start": { "line": 0, "character": 15 },
          "end": { "line": 0, "character": 15 }
        },
        "newText": "\n"
      }
    ]))
  );
  client.shutdown();
}

#[test]
fn lsp_format_with_config() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();
  temp_dir.write(
    "deno.fmt.jsonc",
    r#"{
    "fmt": {
      "options": {
        "useTabs": true,
        "lineWidth": 40,
        "indentWidth": 8,
        "singleQuote": true,
        "proseWrap": "always"
      }
    }
  }
  "#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.fmt.jsonc");
  });

  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts",
          "languageId": "typescript",
          "version": 1,
          "text": "export async function someVeryLongFunctionName() {\nconst response = fetch(\"http://localhost:4545/some/non/existent/path.json\");\nconsole.log(response.text());\nconsole.log(\"finished!\")\n}"
        }
      }),
    )
    .unwrap();

  // The options below should be ignored in favor of configuration from config file.
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/formatting",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "options": {
          "tabSize": 2,
          "insertSpaces": true
        }
      }),
    )
    .unwrap();

  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
        "range": {
          "start": { "line": 1, "character": 0 },
          "end": { "line": 1, "character": 0 }
        },
        "newText": "\t"
      }, {
        "range": {
          "start": { "line": 1, "character": 23 },
          "end": { "line": 1, "character": 24 }
        },
        "newText": "\n\t\t'"
      }, {
        "range": {
          "start": { "line": 1, "character": 73 },
          "end": { "line": 1, "character": 74 }
        },
        "newText": "',\n\t"
      }, {
        "range": {
          "start": { "line": 2, "character": 0 },
          "end": { "line": 2, "character": 0 }
        },
        "newText": "\t"
      }, {
        "range": {
          "start": { "line": 3, "character": 0 },
          "end": { "line": 3, "character": 0 }
        },
        "newText": "\t"
      }, {
        "range": {
          "start": { "line": 3, "character": 12 },
          "end": { "line": 3, "character": 13 }
        },
        "newText": "'"
      }, {
        "range": {
          "start": { "line": 3, "character": 22 },
          "end": { "line": 3, "character": 24 }
        },
        "newText": "');"
      }, {
        "range": {
          "start": { "line": 4, "character": 1 },
          "end": { "line": 4, "character": 1 }
        },
        "newText": "\n"
      }]
    ))
  );
  client.shutdown();
}

#[test]
fn lsp_markdown_no_diagnostics() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": "file:///a/file.md",
          "languageId": "markdown",
          "version": 1,
          "text": "# Hello World"
        }
      }),
    )
    .unwrap();

  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/semanticTokens/full",
      json!({
        "textDocument": {
          "uri": "file:///a/file.md"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.md"
        },
        "position": { "line": 0, "character": 3 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(maybe_res, Some(json!(null)));

  client.shutdown();
}

#[test]
fn lsp_configuration_did_change() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "import * as a from \"http://localhost:4545/x/a@\""
      }
    }),
  );
  client
    .write_notification(
      "workspace/didChangeConfiguration",
      json!({
        "settings": {}
      }),
    )
    .unwrap();
  let (id, method, _) = client.read_request::<Value>().unwrap();
  assert_eq!(method, "workspace/configuration");
  client
    .write_response(
      id,
      json!([{
        "enable": true,
        "codeLens": {
          "implementations": true,
          "references": true
        },
        "importMap": null,
        "lint": true,
        "suggest": {
          "autoImports": true,
          "completeFunctionCalls": false,
          "names": true,
          "paths": true,
          "imports": {
            "hosts": {
              "http://localhost:4545/": true
            }
          }
        },
        "unstable": false
      }]),
    )
    .unwrap();
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/completion",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "position": { "line": 0, "character": 46 },
        "context": {
          "triggerKind": 2,
          "triggerCharacter": "@"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  if let Some(lsp::CompletionResponse::List(list)) = maybe_res {
    assert!(!list.is_incomplete);
    assert_eq!(list.items.len(), 3);
  } else {
    panic!("unexpected response");
  }
  let (maybe_res, maybe_err) = client
    .write_request(
      "completionItem/resolve",
      json!({
        "label": "v2.0.0",
        "kind": 19,
        "detail": "(version)",
        "sortText": "0000000003",
        "filterText": "http://localhost:4545/x/a@v2.0.0",
        "textEdit": {
          "range": {
            "start": { "line": 0, "character": 20 },
            "end": { "line": 0, "character": 46 }
          },
          "newText": "http://localhost:4545/x/a@v2.0.0"
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "label": "v2.0.0",
      "kind": 19,
      "detail": "(version)",
      "sortText": "0000000003",
      "filterText": "http://localhost:4545/x/a@v2.0.0",
      "textEdit": {
        "range": {
          "start": { "line": 0, "character": 20 },
          "end": { "line": 0, "character": 46 }
        },
        "newText": "http://localhost:4545/x/a@v2.0.0"
      }
    }))
  );
  client.shutdown();
}

#[test]
fn lsp_workspace_symbol() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export class A {\n  fieldA: string;\n  fieldB: string;\n}\n",
      }
    }),
  );
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file_01.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "export class B {\n  fieldC: string;\n  fieldD: string;\n}\n",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "workspace/symbol",
      json!({
        "query": "field"
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([
      {
        "name": "fieldA",
        "kind": 8,
        "location": {
          "uri": "file:///a/file.ts",
          "range": {
            "start": { "line": 1, "character": 2 },
            "end": { "line": 1, "character": 17 }
          }
        },
        "containerName": "A"
      }, {
        "name": "fieldB",
        "kind": 8,
        "location": {
          "uri": "file:///a/file.ts",
          "range": {
            "start": { "line": 2, "character": 2 },
            "end": { "line": 2, "character": 17 }
          }
        },
        "containerName": "A"
      }, {
        "name": "fieldC",
        "kind": 8,
        "location": {
          "uri": "file:///a/file_01.ts",
          "range": {
            "start": { "line": 1, "character": 2 },
            "end": { "line": 1, "character": 17 }
          }
        },
        "containerName": "B"
      }, {
        "name": "fieldD",
        "kind": 8,
        "location": {
          "uri": "file:///a/file_01.ts",
          "range": {
            "start": { "line": 2, "character": 2 },
            "end": { "line": 2, "character": 17 }
          }
        },
        "containerName": "B"
      }
    ]))
  );
  client.shutdown();
}

#[test]
fn lsp_code_actions_ignore_lint() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text": "let message = 'Hello, Deno!';\nconsole.log(message);\n"
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 1, "character": 5 },
          "end": { "line": 1, "character": 12 }
        },
        "context": {
          "diagnostics": [
            {
              "range": {
                "start": { "line": 1, "character": 5 },
                "end": { "line": 1, "character": 12 }
              },
              "severity": 1,
              "code": "prefer-const",
              "source": "deno-lint",
              "message": "'message' is never reassigned\nUse 'const' instead",
              "relatedInformation": []
            }
          ],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Disable prefer-const for this line",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 1, "character": 5 },
          "end": { "line": 1, "character": 12 }
        },
        "severity": 1,
        "code": "prefer-const",
        "source": "deno-lint",
        "message": "'message' is never reassigned\nUse 'const' instead",
        "relatedInformation": []
      }],
      "edit": {
        "changes": {
          "file:///a/file.ts": [{
            "range": {
              "start": { "line": 1, "character": 0 },
              "end": { "line": 1, "character": 0 }
            },
            "newText": "// deno-lint-ignore prefer-const\n"
          }]
        }
      }
    }, {
      "title": "Disable prefer-const for the entire file",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 1, "character": 5 },
          "end": { "line": 1, "character": 12 }
        },
        "severity": 1,
        "code": "prefer-const",
        "source": "deno-lint",
        "message": "'message' is never reassigned\nUse 'const' instead",
        "relatedInformation": []
      }],
      "edit": {
        "changes": {
          "file:///a/file.ts": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "// deno-lint-ignore-file prefer-const\n"
          }]
        }
      }
    }, {
      "title": "Ignore lint errors for the entire file",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 1, "character": 5 },
          "end": { "line": 1, "character": 12 }
        },
        "severity": 1,
        "code": "prefer-const",
        "source": "deno-lint",
        "message": "'message' is never reassigned\nUse 'const' instead",
        "relatedInformation": []
      }],
      "edit": {
        "changes": {
          "file:///a/file.ts": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "// deno-lint-ignore-file\n"
          }]
        }
      }
    }]))
  );
  client.shutdown();
}

/// This test exercises updating an existing deno-lint-ignore-file comment.
#[test]
fn lsp_code_actions_update_ignore_lint() {
  let mut client = LspClientBuilder::new().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.ts",
        "languageId": "typescript",
        "version": 1,
        "text":
"#!/usr/bin/env -S deno run
// deno-lint-ignore-file camelcase
let snake_case = 'Hello, Deno!';
console.log(snake_case);
",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request(
      "textDocument/codeAction",
      json!({
        "textDocument": {
          "uri": "file:///a/file.ts"
        },
        "range": {
          "start": { "line": 3, "character": 5 },
          "end": { "line": 3, "character": 15 }
        },
        "context": {
          "diagnostics": [{
            "range": {
              "start": { "line": 3, "character": 5 },
              "end": { "line": 3, "character": 15 }
            },
            "severity": 1,
            "code": "prefer-const",
            "source": "deno-lint",
            "message": "'snake_case' is never reassigned\nUse 'const' instead",
            "relatedInformation": []
          }],
          "only": ["quickfix"]
        }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!([{
      "title": "Disable prefer-const for this line",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 3, "character": 5 },
          "end": { "line": 3, "character": 15 }
        },
        "severity": 1,
        "code": "prefer-const",
        "source": "deno-lint",
        "message": "'snake_case' is never reassigned\nUse 'const' instead",
        "relatedInformation": []
      }],
      "edit": {
        "changes": {
          "file:///a/file.ts": [{
            "range": {
              "start": { "line": 3, "character": 0 },
              "end": { "line": 3, "character": 0 }
            },
            "newText": "// deno-lint-ignore prefer-const\n"
          }]
        }
      }
    }, {
      "title": "Disable prefer-const for the entire file",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 3, "character": 5 },
          "end": { "line": 3, "character": 15 }
        },
        "severity": 1,
        "code": "prefer-const",
        "source": "deno-lint",
        "message": "'snake_case' is never reassigned\nUse 'const' instead",
        "relatedInformation": []
      }],
      "edit": {
        "changes": {
          "file:///a/file.ts": [{
            "range": {
              "start": { "line": 1, "character": 34 },
              "end": { "line": 1, "character": 34 }
            },
            "newText": " prefer-const"
          }]
        }
      }
    }, {
      "title": "Ignore lint errors for the entire file",
      "kind": "quickfix",
      "diagnostics": [{
        "range": {
          "start": { "line": 3, "character": 5 },
          "end": { "line": 3, "character": 15 }
        },
        "severity": 1,
        "code": "prefer-const",
        "source": "deno-lint",
        "message": "'snake_case' is never reassigned\nUse 'const' instead",
        "relatedInformation": []
      }],
      "edit": {
        "changes": {
          "file:///a/file.ts": [{
            "range": {
              "start": { "line": 0, "character": 0 },
              "end": { "line": 0, "character": 0 }
            },
            "newText": "// deno-lint-ignore-file\n"
          }]
        }
      }
    }]))
  );
  client.shutdown();
}

#[test]
fn lsp_lint_with_config() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  temp_dir.write(
    "deno.lint.jsonc",
    r#"{
    "lint": {
      "rules": {
        "exclude": ["camelcase"],
        "include": ["ban-untagged-todo"],
        "tags": []
      }
    }
  }
  "#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.lint.jsonc");
  });

  let mut session = TestSession::from_client(client);

  let diagnostics = session.did_open(json!({
    "textDocument": {
      "uri": "file:///a/file.ts",
      "languageId": "typescript",
      "version": 1,
      "text": "// TODO: fixme\nexport async function non_camel_case() {\nconsole.log(\"finished!\")\n}"
    }
  }));
  let diagnostics = diagnostics.viewed();
  assert_eq!(diagnostics.len(), 1);
  assert_eq!(
    diagnostics[0].code,
    Some(lsp::NumberOrString::String("ban-untagged-todo".to_string()))
  );
  session.shutdown_and_exit();
}

#[test]
fn lsp_lint_exclude_with_config() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  temp_dir.write(
    "deno.lint.jsonc",
    r#"{
      "lint": {
        "files": {
          "exclude": ["ignored.ts"]
        },
        "rules": {
          "exclude": ["camelcase"],
          "include": ["ban-untagged-todo"],
          "tags": []
        }
      }
    }"#,
  );

  let mut client = context.new_lsp_command().build();
  client.initialize(|builder| {
    builder.set_config("./deno.lint.jsonc");
  });

  let diagnostics = did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": ModuleSpecifier::from_file_path(temp_dir.path().join("ignored.ts")).unwrap().to_string(),
        "languageId": "typescript",
        "version": 1,
        "text": "// TODO: fixme\nexport async function non_camel_case() {\nconsole.log(\"finished!\")\n}"
      }
    }),
  );
  let diagnostics = diagnostics
    .into_iter()
    .flat_map(|x| x.diagnostics)
    .collect::<Vec<_>>();
  assert_eq!(diagnostics, Vec::new());
  client.shutdown();
}

#[test]
fn lsp_jsx_import_source_pragma() {
  let context = TestContextBuilder::new().use_http_server().build();
  let mut client = context.new_lsp_command().build();
  client.initialize_default();
  did_open(
    &mut client,
    json!({
      "textDocument": {
        "uri": "file:///a/file.tsx",
        "languageId": "typescriptreact",
        "version": 1,
        "text":
"/** @jsxImportSource http://localhost:4545/jsx */

function A() {
  return \"hello\";
}

export function B() {
  return <A></A>;
}
",
      }
    }),
  );
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "deno/cache",
      json!({
        "referrer": {
          "uri": "file:///a/file.tsx",
        },
        "uris": [{
          "uri": "http://127.0.0.1:4545/jsx/jsx-runtime",
        }],
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let (maybe_res, maybe_err) = client
    .write_request::<_, _, Value>(
      "textDocument/hover",
      json!({
        "textDocument": {
          "uri": "file:///a/file.tsx"
        },
        "position": { "line": 0, "character": 25 }
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert_eq!(
    maybe_res,
    Some(json!({
      "contents": {
        "kind": "markdown",
        "value": "**Resolved Dependency**\n\n**Code**: http&#8203;://localhost:4545/jsx/jsx-runtime\n",
      },
      "range": {
        "start": { "line": 0, "character": 21 },
        "end": { "line": 0, "character": 46 }
      }
    }))
  );
  client.shutdown();
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct TestData {
  id: String,
  label: String,
  steps: Option<Vec<TestData>>,
  range: Option<lsp::Range>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
enum TestModuleNotificationKind {
  Insert,
  Replace,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestModuleNotificationParams {
  text_document: lsp::TextDocumentIdentifier,
  kind: TestModuleNotificationKind,
  label: String,
  tests: Vec<TestData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnqueuedTestModule {
  text_document: lsp::TextDocumentIdentifier,
  ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestRunResponseParams {
  enqueued: Vec<EnqueuedTestModule>,
}

#[test]
fn lsp_testing_api() {
  let context = TestContextBuilder::new().build();
  let temp_dir = context.deno_dir();

  let contents = r#"
Deno.test({
  name: "test a",
  fn() {
    console.log("test a");
  }
});
"#;
  temp_dir.write("./test.ts", contents);
  temp_dir.write("./deno.jsonc", "{}");
  let specifier = temp_dir.uri().join("test.ts").unwrap();

  let mut client = context.new_lsp_command().build();
  client.initialize_default();

  client
    .write_notification(
      "textDocument/didOpen",
      json!({
        "textDocument": {
          "uri": specifier,
          "languageId": "typescript",
          "version": 1,
          "text": contents,
        }
      }),
    )
    .unwrap();

  handle_configuration_request(
    &mut client,
    json!([{
      "enable": true,
      "codeLens": {
        "test": true
      }
    }]),
  );

  for _ in 0..4 {
    let result = client.read_notification::<Value>();
    assert!(result.is_ok());
    let (method, notification) = result.unwrap();
    if method.as_str() == "deno/testModule" {
      let params: TestModuleNotificationParams =
        serde_json::from_value(notification.unwrap()).unwrap();
      assert_eq!(params.text_document.uri, specifier);
      assert_eq!(params.kind, TestModuleNotificationKind::Replace);
      assert_eq!(params.label, "test.ts");
      assert_eq!(params.tests.len(), 1);
      let test = &params.tests[0];
      assert_eq!(test.label, "test a");
      assert!(test.steps.is_none());
      assert_eq!(
        test.range,
        Some(lsp::Range {
          start: lsp::Position {
            line: 1,
            character: 5,
          },
          end: lsp::Position {
            line: 1,
            character: 9,
          }
        })
      );
    }
  }

  let (maybe_res, maybe_err) = client
    .write_request::<_, _, TestRunResponseParams>(
      "deno/testRun",
      json!({
        "id": 1,
        "kind": "run",
      }),
    )
    .unwrap();
  assert!(maybe_err.is_none());
  assert!(maybe_res.is_some());
  let res = maybe_res.unwrap();
  assert_eq!(res.enqueued.len(), 1);
  assert_eq!(res.enqueued[0].text_document.uri, specifier);
  assert_eq!(res.enqueued[0].ids.len(), 1);
  let id = res.enqueued[0].ids[0].clone();

  let res = client.read_notification::<Value>();
  assert!(res.is_ok());
  let (method, notification) = res.unwrap();
  assert_eq!(method, "deno/testRunProgress");
  assert_eq!(
    notification,
    Some(json!({
      "id": 1,
      "message": {
        "type": "started",
        "test": {
          "textDocument": {
            "uri": specifier,
          },
          "id": id,
        },
      }
    }))
  );

  let res = client.read_notification::<Value>();
  assert!(res.is_ok());
  let (method, notification) = res.unwrap();
  assert_eq!(method, "deno/testRunProgress");
  let notification_value = notification
    .as_ref()
    .unwrap()
    .as_object()
    .unwrap()
    .get("message")
    .unwrap()
    .as_object()
    .unwrap()
    .get("value")
    .unwrap()
    .as_str()
    .unwrap();
  // deno test's output capturing flushes with a zero-width space in order to
  // synchronize the output pipes. Occassionally this zero width space
  // might end up in the output so strip it from the output comparison here.
  assert_eq!(notification_value.replace('\u{200B}', ""), "test a\r\n");
  assert_eq!(
    notification,
    Some(json!({
      "id": 1,
      "message": {
        "type": "output",
        "value": notification_value,
        "test": {
          "textDocument": {
            "uri": specifier,
          },
          "id": id,
        },
      }
    }))
  );

  let res = client.read_notification::<Value>();
  assert!(res.is_ok());
  let (method, notification) = res.unwrap();
  assert_eq!(method, "deno/testRunProgress");
  let notification = notification.unwrap();
  let obj = notification.as_object().unwrap();
  assert_eq!(obj.get("id"), Some(&json!(1)));
  let message = obj.get("message").unwrap().as_object().unwrap();
  match message.get("type").and_then(|v| v.as_str()) {
    Some("passed") => {
      assert_eq!(
        message.get("test"),
        Some(&json!({
          "textDocument": {
            "uri": specifier
          },
          "id": id,
        }))
      );
      assert!(message.contains_key("duration"));

      let res = client.read_notification::<Value>();
      assert!(res.is_ok());
      let (method, notification) = res.unwrap();
      assert_eq!(method, "deno/testRunProgress");
      assert_eq!(
        notification,
        Some(json!({
          "id": 1,
          "message": {
            "type": "end",
          }
        }))
      );
    }
    // sometimes on windows, the messages come out of order, but it actually is
    // working, so if we do get the end before the passed, we will simply let
    // the test pass
    Some("end") => (),
    _ => panic!("unexpected message {}", json!(notification)),
  }

  client.shutdown();
}
