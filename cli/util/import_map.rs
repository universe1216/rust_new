// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::collections::HashSet;

use deno_ast::ParsedSource;
use deno_ast::SourceRange;
use deno_ast::SourceTextInfo;
use deno_core::serde_json;
use deno_core::ModuleSpecifier;
use deno_graph::DefaultModuleAnalyzer;
use deno_graph::DependencyDescriptor;
use deno_graph::DynamicTemplatePart;
use deno_graph::TypeScriptReference;
use deno_semver::jsr::JsrDepPackageReq;
use deno_semver::jsr::JsrPackageReqReference;
use deno_semver::npm::NpmPackageReqReference;

use crate::resolver::MappedSpecifierResolver;

pub fn deno_json_deps(
  config: &deno_config::ConfigFile,
) -> HashSet<JsrDepPackageReq> {
  let values = imports_values(config.json.imports.as_ref())
    .into_iter()
    .chain(scope_values(config.json.scopes.as_ref()));
  values_to_set(values)
}

fn imports_values(value: Option<&serde_json::Value>) -> Vec<&String> {
  let Some(obj) = value.and_then(|v| v.as_object()) else {
    return Vec::new();
  };
  let mut items = Vec::with_capacity(obj.len());
  for value in obj.values() {
    if let serde_json::Value::String(value) = value {
      items.push(value);
    }
  }
  items
}

fn scope_values(value: Option<&serde_json::Value>) -> Vec<&String> {
  let Some(obj) = value.and_then(|v| v.as_object()) else {
    return Vec::new();
  };
  obj.values().flat_map(|v| imports_values(Some(v))).collect()
}

fn values_to_set<'a>(
  values: impl Iterator<Item = &'a String>,
) -> HashSet<JsrDepPackageReq> {
  let mut entries = HashSet::new();
  for value in values {
    if let Ok(req_ref) = JsrPackageReqReference::from_str(value) {
      entries.insert(JsrDepPackageReq::jsr(req_ref.into_inner().req));
    } else if let Ok(req_ref) = NpmPackageReqReference::from_str(value) {
      entries.insert(JsrDepPackageReq::npm(req_ref.into_inner().req));
    }
  }
  entries
}

#[derive(Debug, Clone)]
pub enum ImportMapUnfurlDiagnostic {
  UnanalyzableDynamicImport {
    specifier: ModuleSpecifier,
    text_info: SourceTextInfo,
    range: SourceRange,
  },
}

impl ImportMapUnfurlDiagnostic {
  pub fn code(&self) -> &'static str {
    match self {
      Self::UnanalyzableDynamicImport { .. } => "unanalyzable-dynamic-import",
    }
  }

  pub fn message(&self) -> &'static str {
    match self {
      Self::UnanalyzableDynamicImport { .. } => {
        "unable to analyze dynamic import"
      }
    }
  }
}

pub struct ImportMapUnfurler<'a> {
  import_map: &'a MappedSpecifierResolver,
}

impl<'a> ImportMapUnfurler<'a> {
  pub fn new(import_map: &'a MappedSpecifierResolver) -> Self {
    Self { import_map }
  }

  pub fn unfurl(
    &self,
    url: &ModuleSpecifier,
    parsed_source: &ParsedSource,
    diagnostic_reporter: &mut dyn FnMut(ImportMapUnfurlDiagnostic),
  ) -> String {
    let mut text_changes = Vec::new();
    let module_info = DefaultModuleAnalyzer::module_info(parsed_source);
    let analyze_specifier =
      |specifier: &str,
       range: &deno_graph::PositionRange,
       text_changes: &mut Vec<deno_ast::TextChange>| {
        let resolved = self.import_map.resolve(specifier, url);
        if let Ok(resolved) = resolved {
          if let Some(resolved) = resolved.into_specifier() {
            text_changes.push(deno_ast::TextChange {
              range: to_range(parsed_source, range),
              new_text: make_relative_to(url, &resolved),
            });
          }
        }
      };
    for dep in &module_info.dependencies {
      match dep {
        DependencyDescriptor::Static(dep) => {
          analyze_specifier(
            &dep.specifier,
            &dep.specifier_range,
            &mut text_changes,
          );
        }
        DependencyDescriptor::Dynamic(dep) => {
          let success = try_unfurl_dynamic_dep(
            self.import_map,
            url,
            parsed_source,
            dep,
            &mut text_changes,
          );

          if !success {
            let start_pos = parsed_source
              .text_info()
              .line_start(dep.argument_range.start.line)
              + dep.argument_range.start.character;
            let end_pos = parsed_source
              .text_info()
              .line_start(dep.argument_range.end.line)
              + dep.argument_range.end.character;
            diagnostic_reporter(
              ImportMapUnfurlDiagnostic::UnanalyzableDynamicImport {
                specifier: url.to_owned(),
                range: SourceRange::new(start_pos, end_pos),
                text_info: parsed_source.text_info().clone(),
              },
            );
          }
        }
      }
    }
    for ts_ref in &module_info.ts_references {
      let specifier_with_range = match ts_ref {
        TypeScriptReference::Path(range) => range,
        TypeScriptReference::Types(range) => range,
      };
      analyze_specifier(
        &specifier_with_range.text,
        &specifier_with_range.range,
        &mut text_changes,
      );
    }
    for specifier_with_range in &module_info.jsdoc_imports {
      analyze_specifier(
        &specifier_with_range.text,
        &specifier_with_range.range,
        &mut text_changes,
      );
    }
    if let Some(specifier_with_range) = &module_info.jsx_import_source {
      analyze_specifier(
        &specifier_with_range.text,
        &specifier_with_range.range,
        &mut text_changes,
      );
    }

    let rewritten_text = deno_ast::apply_text_changes(
      parsed_source.text_info().text_str(),
      text_changes,
    );
    rewritten_text
  }
}

fn make_relative_to(from: &ModuleSpecifier, to: &ModuleSpecifier) -> String {
  if to.scheme() == "file" {
    format!("./{}", from.make_relative(to).unwrap())
  } else {
    to.to_string()
  }
}

/// Attempts to unfurl the dynamic dependency returning `true` on success
/// or `false` when the import was not analyzable.
fn try_unfurl_dynamic_dep(
  mapped_resolver: &MappedSpecifierResolver,
  module_url: &lsp_types::Url,
  parsed_source: &ParsedSource,
  dep: &deno_graph::DynamicDependencyDescriptor,
  text_changes: &mut Vec<deno_ast::TextChange>,
) -> bool {
  match &dep.argument {
    deno_graph::DynamicArgument::String(value) => {
      let range = to_range(parsed_source, &dep.argument_range);
      let maybe_relative_index =
        parsed_source.text_info().text_str()[range.start..].find(value);
      let Some(relative_index) = maybe_relative_index else {
        return false;
      };
      let resolved = mapped_resolver.resolve(value, module_url);
      let Ok(resolved) = resolved else {
        return false;
      };
      let Some(resolved) = resolved.into_specifier() else {
        return false;
      };
      let start = range.start + relative_index;
      text_changes.push(deno_ast::TextChange {
        range: start..start + value.len(),
        new_text: make_relative_to(module_url, &resolved),
      });
      true
    }
    deno_graph::DynamicArgument::Template(parts) => match parts.first() {
      Some(DynamicTemplatePart::String { value }) => {
        // relative doesn't need to be modified
        let is_relative = value.starts_with("./") || value.starts_with("../");
        if is_relative {
          return true;
        }
        if !value.ends_with('/') {
          return false;
        }
        let Ok(resolved) = mapped_resolver.resolve(value, module_url) else {
          return false;
        };
        let Some(resolved) = resolved.into_specifier() else {
          return false;
        };
        let range = to_range(parsed_source, &dep.argument_range);
        let maybe_relative_index =
          parsed_source.text_info().text_str()[range.start..].find(value);
        let Some(relative_index) = maybe_relative_index else {
          return false;
        };
        let start = range.start + relative_index;
        text_changes.push(deno_ast::TextChange {
          range: start..start + value.len(),
          new_text: make_relative_to(module_url, &resolved),
        });
        true
      }
      Some(DynamicTemplatePart::Expr) => {
        false // failed analyzing
      }
      None => {
        true // ignore
      }
    },
    deno_graph::DynamicArgument::Expr => {
      false // failed analyzing
    }
  }
}

fn to_range(
  parsed_source: &ParsedSource,
  range: &deno_graph::PositionRange,
) -> std::ops::Range<usize> {
  let mut range = range
    .as_source_range(parsed_source.text_info())
    .as_byte_range(parsed_source.text_info().range().start);
  let text = &parsed_source.text_info().text_str()[range.clone()];
  if text.starts_with('"') || text.starts_with('\'') {
    range.start += 1;
  }
  if text.ends_with('"') || text.ends_with('\'') {
    range.end -= 1;
  }
  range
}

#[cfg(test)]
mod tests {
  use std::sync::Arc;

  use crate::args::PackageJsonDepsProvider;

  use super::*;
  use deno_ast::MediaType;
  use deno_ast::ModuleSpecifier;
  use deno_core::serde_json::json;
  use deno_core::url::Url;
  use import_map::ImportMapWithDiagnostics;
  use pretty_assertions::assert_eq;

  fn parse_ast(specifier: &Url, source_code: &str) -> ParsedSource {
    let media_type = MediaType::from_specifier(specifier);
    deno_ast::parse_module(deno_ast::ParseParams {
      specifier: specifier.clone(),
      media_type,
      capture_tokens: false,
      maybe_syntax: None,
      scope_analysis: false,
      text_info: deno_ast::SourceTextInfo::new(source_code.into()),
    })
    .unwrap()
  }

  #[test]
  fn test_unfurling() {
    let deno_json_url =
      ModuleSpecifier::parse("file:///dev/deno.json").unwrap();
    let value = json!({
      "imports": {
        "express": "npm:express@5",
        "lib/": "./lib/",
        "fizz": "./fizz/mod.ts"
      }
    });
    let ImportMapWithDiagnostics { import_map, .. } =
      import_map::parse_from_value(deno_json_url, value).unwrap();
    let mapped_resolved = MappedSpecifierResolver::new(
      Some(Arc::new(import_map)),
      Arc::new(PackageJsonDepsProvider::new(None)),
    );
    let unfurler = ImportMapUnfurler::new(&mapped_resolved);

    // Unfurling TS file should apply changes.
    {
      let source_code = r#"import express from "express";"
import foo from "lib/foo.ts";
import bar from "lib/bar.ts";
import fizz from "fizz";

const test1 = await import("lib/foo.ts");
const test2 = await import(`lib/foo.ts`);
const test3 = await import(`lib/${expr}`);
const test4 = await import(`./lib/${expr}`);
// will warn
const test5 = await import(`lib${expr}`);
const test6 = await import(`${expr}`);
"#;
      let specifier = ModuleSpecifier::parse("file:///dev/mod.ts").unwrap();
      let source = parse_ast(&specifier, source_code);
      let mut d = Vec::new();
      let mut reporter = |diagnostic| d.push(diagnostic);
      let unfurled_source = unfurler.unfurl(&specifier, &source, &mut reporter);
      assert_eq!(d.len(), 2);
      assert!(
        matches!(
          d[0],
          ImportMapUnfurlDiagnostic::UnanalyzableDynamicImport { .. }
        ),
        "{:?}",
        d[0]
      );
      assert!(
        matches!(
          d[1],
          ImportMapUnfurlDiagnostic::UnanalyzableDynamicImport { .. }
        ),
        "{:?}",
        d[1]
      );
      let expected_source = r#"import express from "npm:express@5";"
import foo from "./lib/foo.ts";
import bar from "./lib/bar.ts";
import fizz from "./fizz/mod.ts";

const test1 = await import("./lib/foo.ts");
const test2 = await import(`./lib/foo.ts`);
const test3 = await import(`./lib/${expr}`);
const test4 = await import(`./lib/${expr}`);
// will warn
const test5 = await import(`lib${expr}`);
const test6 = await import(`${expr}`);
"#;
      assert_eq!(unfurled_source, expected_source);
    }
  }
}
