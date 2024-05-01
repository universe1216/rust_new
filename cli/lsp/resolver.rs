// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use crate::args::package_json;
use crate::args::CacheSetting;
use crate::cache::DenoDir;
use crate::cache::FastInsecureHasher;
use crate::http_util::HttpClient;
use crate::lsp::config::Config;
use crate::lsp::config::ConfigData;
use crate::lsp::logging::lsp_warn;
use crate::npm::create_cli_npm_resolver_for_lsp;
use crate::npm::CliNpmResolver;
use crate::npm::CliNpmResolverByonmCreateOptions;
use crate::npm::CliNpmResolverCreateOptions;
use crate::npm::CliNpmResolverManagedCreateOptions;
use crate::npm::CliNpmResolverManagedPackageJsonInstallerOption;
use crate::npm::CliNpmResolverManagedSnapshotOption;
use crate::npm::ManagedCliNpmResolver;
use crate::resolver::CliGraphResolver;
use crate::resolver::CliGraphResolverOptions;
use crate::resolver::CliNodeResolver;
use crate::util::progress_bar::ProgressBar;
use crate::util::progress_bar::ProgressBarStyle;
use deno_core::error::AnyError;
use deno_graph::source::NpmResolver;
use deno_graph::source::Resolver;
use deno_graph::ModuleSpecifier;
use deno_npm::NpmSystemInfo;
use deno_runtime::deno_fs;
use deno_runtime::deno_node::NodeResolution;
use deno_runtime::deno_node::NodeResolutionMode;
use deno_runtime::deno_node::NodeResolver;
use deno_runtime::deno_node::PackageJson;
use deno_runtime::fs_util::specifier_to_file_path;
use deno_runtime::permissions::PermissionsContainer;
use deno_semver::npm::NpmPackageReqReference;
use deno_semver::package::PackageReq;
use package_json::PackageJsonDepsProvider;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct LspResolver {
  graph_resolver: Arc<CliGraphResolver>,
  npm_resolver: Option<Arc<dyn CliNpmResolver>>,
  node_resolver: Option<Arc<CliNodeResolver>>,
  npm_config_hash: LspNpmConfigHash,
  config: Arc<Config>,
}

impl Default for LspResolver {
  fn default() -> Self {
    Self {
      graph_resolver: create_graph_resolver(&Default::default(), None, None),
      npm_resolver: None,
      node_resolver: None,
      npm_config_hash: LspNpmConfigHash(0),
      config: Default::default(),
    }
  }
}

impl LspResolver {
  pub async fn with_new_config(
    &self,
    config: &Config,
    global_cache_path: Option<&Path>,
    http_client: Option<&Arc<HttpClient>>,
  ) -> Arc<Self> {
    let npm_config_hash = LspNpmConfigHash::new(config, global_cache_path);
    let mut npm_resolver = None;
    let mut node_resolver = None;
    if npm_config_hash != self.npm_config_hash {
      if let (Some(http_client), Some(config_data)) =
        (http_client, config.tree.root_data())
      {
        npm_resolver =
          create_npm_resolver(config_data, global_cache_path, http_client)
            .await;
        node_resolver = create_node_resolver(npm_resolver.as_ref());
      }
    } else {
      npm_resolver = self.npm_resolver.clone();
      node_resolver = self.node_resolver.clone();
    }
    let graph_resolver = create_graph_resolver(
      config,
      npm_resolver.as_ref(),
      node_resolver.as_ref(),
    );
    Arc::new(Self {
      graph_resolver,
      npm_resolver,
      node_resolver,
      npm_config_hash,
      config: Arc::new(config.clone()),
    })
  }

  pub fn snapshot(&self) -> Arc<Self> {
    let npm_resolver =
      self.npm_resolver.as_ref().map(|r| r.clone_snapshotted());
    let node_resolver = create_node_resolver(npm_resolver.as_ref());
    let graph_resolver = create_graph_resolver(
      &self.config,
      npm_resolver.as_ref(),
      node_resolver.as_ref(),
    );
    Arc::new(Self {
      graph_resolver,
      npm_resolver,
      node_resolver,
      npm_config_hash: self.npm_config_hash.clone(),
      config: self.config.clone(),
    })
  }

  pub async fn set_npm_package_reqs(
    &self,
    reqs: &[PackageReq],
  ) -> Result<(), AnyError> {
    if let Some(npm_resolver) = self.npm_resolver.as_ref() {
      if let Some(npm_resolver) = npm_resolver.as_managed() {
        return npm_resolver.set_package_reqs(reqs).await;
      }
    }
    Ok(())
  }

  pub fn as_graph_resolver(&self) -> &dyn Resolver {
    self.graph_resolver.as_ref()
  }

  pub fn as_graph_npm_resolver(&self) -> &dyn NpmResolver {
    self.graph_resolver.as_ref()
  }

  pub fn maybe_managed_npm_resolver(&self) -> Option<&ManagedCliNpmResolver> {
    self.npm_resolver.as_ref().and_then(|r| r.as_managed())
  }

  pub fn in_npm_package(&self, specifier: &ModuleSpecifier) -> bool {
    if let Some(npm_resolver) = &self.npm_resolver {
      return npm_resolver.in_npm_package(specifier);
    }
    false
  }

  pub fn node_resolve(
    &self,
    specifier: &str,
    referrer: &ModuleSpecifier,
    mode: NodeResolutionMode,
  ) -> Result<Option<NodeResolution>, AnyError> {
    let Some(node_resolver) = self.node_resolver.as_ref() else {
      return Ok(None);
    };
    node_resolver.resolve(
      specifier,
      referrer,
      mode,
      &PermissionsContainer::allow_all(),
    )
  }

  pub fn resolve_npm_req_reference(
    &self,
    req_ref: &NpmPackageReqReference,
    referrer: &ModuleSpecifier,
    mode: NodeResolutionMode,
  ) -> Result<Option<NodeResolution>, AnyError> {
    let Some(node_resolver) = self.node_resolver.as_ref() else {
      return Ok(None);
    };
    node_resolver
      .resolve_req_reference(
        req_ref,
        &PermissionsContainer::allow_all(),
        referrer,
        mode,
      )
      .map(Some)
  }

  pub fn url_to_node_resolution(
    &self,
    specifier: ModuleSpecifier,
  ) -> Result<Option<NodeResolution>, AnyError> {
    let Some(node_resolver) = self.node_resolver.as_ref() else {
      return Ok(None);
    };
    node_resolver.url_to_node_resolution(specifier).map(Some)
  }

  pub fn get_closest_package_json(
    &self,
    referrer: &ModuleSpecifier,
  ) -> Result<Option<Rc<PackageJson>>, AnyError> {
    let Some(node_resolver) = self.node_resolver.as_ref() else {
      return Ok(None);
    };
    node_resolver
      .get_closest_package_json(referrer, &PermissionsContainer::allow_all())
  }
}

async fn create_npm_resolver(
  config_data: &ConfigData,
  global_cache_path: Option<&Path>,
  http_client: &Arc<HttpClient>,
) -> Option<Arc<dyn CliNpmResolver>> {
  let deno_dir = DenoDir::new(global_cache_path.map(|p| p.to_owned()))
    .inspect_err(|err| {
      lsp_warn!("Error getting deno dir: {:#}", err);
    })
    .ok()?;
  let node_modules_dir = config_data
    .node_modules_dir
    .clone()
    .or_else(|| specifier_to_file_path(&config_data.scope).ok())?;
  let options = if config_data.byonm {
    CliNpmResolverCreateOptions::Byonm(CliNpmResolverByonmCreateOptions {
      fs: Arc::new(deno_fs::RealFs),
      root_node_modules_dir: node_modules_dir,
    })
  } else {
    CliNpmResolverCreateOptions::Managed(CliNpmResolverManagedCreateOptions {
      http_client: http_client.clone(),
      snapshot: match config_data.lockfile.as_ref() {
        Some(lockfile) => {
          CliNpmResolverManagedSnapshotOption::ResolveFromLockfile(
            lockfile.clone(),
          )
        }
        None => CliNpmResolverManagedSnapshotOption::Specified(None),
      },
      // Don't provide the lockfile. We don't want these resolvers
      // updating it. Only the cache request should update the lockfile.
      maybe_lockfile: None,
      fs: Arc::new(deno_fs::RealFs),
      npm_global_cache_dir: deno_dir.npm_folder_path(),
      // Use an "only" cache setting in order to make the
      // user do an explicit "cache" command and prevent
      // the cache from being filled with lots of packages while
      // the user is typing.
      cache_setting: CacheSetting::Only,
      text_only_progress_bar: ProgressBar::new(ProgressBarStyle::TextOnly),
      maybe_node_modules_path: config_data.node_modules_dir.clone(),
      // do not install while resolving in the lsp—leave that to the cache command
      package_json_installer:
        CliNpmResolverManagedPackageJsonInstallerOption::NoInstall,
      npm_registry_url: crate::args::npm_registry_url().to_owned(),
      npm_system_info: NpmSystemInfo::default(),
    })
  };
  Some(create_cli_npm_resolver_for_lsp(options).await)
}

fn create_node_resolver(
  npm_resolver: Option<&Arc<dyn CliNpmResolver>>,
) -> Option<Arc<CliNodeResolver>> {
  let npm_resolver = npm_resolver?;
  let fs = Arc::new(deno_fs::RealFs);
  let node_resolver_inner = Arc::new(NodeResolver::new(
    fs.clone(),
    npm_resolver.clone().into_npm_resolver(),
  ));
  Some(Arc::new(CliNodeResolver::new(
    None,
    fs,
    node_resolver_inner,
    npm_resolver.clone(),
  )))
}

fn create_graph_resolver(
  config: &Config,
  npm_resolver: Option<&Arc<dyn CliNpmResolver>>,
  node_resolver: Option<&Arc<CliNodeResolver>>,
) -> Arc<CliGraphResolver> {
  let config_data = config.tree.root_data();
  let config_file = config_data.and_then(|d| d.config_file.as_deref());
  Arc::new(CliGraphResolver::new(CliGraphResolverOptions {
    node_resolver: node_resolver.cloned(),
    npm_resolver: npm_resolver.cloned(),
    package_json_deps_provider: Arc::new(PackageJsonDepsProvider::new(
      config_data
        .and_then(|d| d.package_json.as_ref())
        .map(|package_json| {
          package_json::get_local_package_json_version_reqs(package_json)
        }),
    )),
    maybe_jsx_import_source_config: config_file
      .and_then(|cf| cf.to_maybe_jsx_import_source_config().ok().flatten()),
    maybe_import_map: config_data.and_then(|d| d.import_map.clone()),
    maybe_vendor_dir: config_data.and_then(|d| d.vendor_dir.as_ref()),
    bare_node_builtins_enabled: config_file
      .map(|config| config.has_unstable("bare-node-builtins"))
      .unwrap_or(false),
    // Don't set this for the LSP because instead we'll use the OpenDocumentsLoader
    // because it's much easier and we get diagnostics/quick fixes about a redirected
    // specifier for free.
    sloppy_imports_resolver: None,
  }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LspNpmConfigHash(u64);

impl LspNpmConfigHash {
  pub fn new(config: &Config, global_cache_path: Option<&Path>) -> Self {
    let config_data = config.tree.root_data();
    let scope = config_data.map(|d| &d.scope);
    let node_modules_dir =
      config_data.and_then(|d| d.node_modules_dir.as_ref());
    let lockfile = config_data.and_then(|d| d.lockfile.as_ref());
    let mut hasher = FastInsecureHasher::new();
    hasher.write_hashable(scope);
    hasher.write_hashable(node_modules_dir);
    hasher.write_hashable(global_cache_path);
    if let Some(lockfile) = lockfile {
      hasher.write_hashable(&*lockfile.lock());
    }
    hasher.write_hashable(global_cache_path);
    Self(hasher.finish())
  }
}
