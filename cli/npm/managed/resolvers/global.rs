// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

//! Code for global npm cache resolution.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use deno_ast::ModuleSpecifier;
use deno_core::anyhow::bail;
use deno_core::error::AnyError;
use deno_core::url::Url;
use deno_npm::NpmPackageCacheFolderId;
use deno_npm::NpmPackageId;
use deno_npm::NpmSystemInfo;
use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_node::NodePermissions;

use super::super::cache::NpmCache;
use super::super::cache::TarballCache;
use super::super::resolution::NpmResolution;
use super::common::cache_packages;
use super::common::NpmPackageFsResolver;
use super::common::RegistryReadPermissionChecker;

/// Resolves packages from the global npm cache.
#[derive(Debug)]
pub struct GlobalNpmPackageResolver {
  cache: Arc<NpmCache>,
  tarball_cache: Arc<TarballCache>,
  resolution: Arc<NpmResolution>,
  system_info: NpmSystemInfo,
  registry_read_permission_checker: RegistryReadPermissionChecker,
}

impl GlobalNpmPackageResolver {
  pub fn new(
    cache: Arc<NpmCache>,
    fs: Arc<dyn FileSystem>,
    tarball_cache: Arc<TarballCache>,
    resolution: Arc<NpmResolution>,
    system_info: NpmSystemInfo,
  ) -> Self {
    Self {
      registry_read_permission_checker: RegistryReadPermissionChecker::new(
        fs,
        cache.root_folder(),
      ),
      cache,
      tarball_cache,
      resolution,
      system_info,
    }
  }
}

#[async_trait(?Send)]
impl NpmPackageFsResolver for GlobalNpmPackageResolver {
  fn root_dir_url(&self) -> &Url {
    self.cache.root_dir_url()
  }

  fn node_modules_path(&self) -> Option<&PathBuf> {
    None
  }

  fn package_folder(&self, id: &NpmPackageId) -> Result<PathBuf, AnyError> {
    let folder_id = self
      .resolution
      .resolve_pkg_cache_folder_id_from_pkg_id(id)
      .unwrap();
    Ok(self.cache.package_folder_for_id(&folder_id))
  }

  fn resolve_package_folder_from_package(
    &self,
    name: &str,
    referrer: &ModuleSpecifier,
  ) -> Result<PathBuf, AnyError> {
    let Some(referrer_pkg_id) = self
      .cache
      .resolve_package_folder_id_from_specifier(referrer)
    else {
      bail!("could not find npm package for '{}'", referrer);
    };
    let pkg = self
      .resolution
      .resolve_package_from_package(name, &referrer_pkg_id)?;
    self.package_folder(&pkg.id)
  }

  fn resolve_package_cache_folder_id_from_specifier(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<Option<NpmPackageCacheFolderId>, AnyError> {
    Ok(
      self
        .cache
        .resolve_package_folder_id_from_specifier(specifier),
    )
  }

  async fn cache_packages(&self) -> Result<(), AnyError> {
    let package_partitions = self
      .resolution
      .all_system_packages_partitioned(&self.system_info);

    cache_packages(package_partitions.packages, &self.tarball_cache).await?;

    // create the copy package folders
    for copy in package_partitions.copy_packages {
      self
        .cache
        .ensure_copy_package(&copy.get_package_cache_folder_id())?;
    }

    Ok(())
  }

  fn ensure_read_permission(
    &self,
    permissions: &mut dyn NodePermissions,
    path: &Path,
  ) -> Result<(), AnyError> {
    self
      .registry_read_permission_checker
      .ensure_registry_read_permission(permissions, path)
  }
}
