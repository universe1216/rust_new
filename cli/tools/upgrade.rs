// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

//! This module provides feature to upgrade deno executable

use crate::args::Flags;
use crate::args::UpgradeFlags;
use crate::colors;
use crate::factory::CliFactory;
use crate::http_util::HttpClient;
use crate::http_util::HttpClientProvider;
use crate::util::archive;
use crate::util::progress_bar::ProgressBar;
use crate::util::progress_bar::ProgressBarStyle;
use crate::version;

use async_trait::async_trait;
use deno_core::anyhow::bail;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::unsync::spawn;
use deno_core::url::Url;
use deno_semver::Version;
use once_cell::sync::Lazy;
use std::borrow::Cow;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::ops::Sub;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const RELEASE_URL: &str = "https://github.com/denoland/deno/releases";
const CANARY_URL: &str = "https://dl.deno.land/canary";

pub static ARCHIVE_NAME: Lazy<String> =
  Lazy::new(|| format!("deno-{}.zip", env!("TARGET")));

// How often query server for new version. In hours.
const UPGRADE_CHECK_INTERVAL: i64 = 24;

const UPGRADE_CHECK_FETCH_DELAY: Duration = Duration::from_millis(500);

/// Environment necessary for doing the update checker.
/// An alternate trait implementation can be provided for testing purposes.
trait UpdateCheckerEnvironment: Clone {
  fn read_check_file(&self) -> String;
  fn write_check_file(&self, text: &str);
  fn current_time(&self) -> chrono::DateTime<chrono::Utc>;
}

#[derive(Clone)]
struct RealUpdateCheckerEnvironment {
  cache_file_path: PathBuf,
  current_time: chrono::DateTime<chrono::Utc>,
}

impl RealUpdateCheckerEnvironment {
  pub fn new(cache_file_path: PathBuf) -> Self {
    Self {
      cache_file_path,
      // cache the current time
      current_time: chrono::Utc::now(),
    }
  }
}

impl UpdateCheckerEnvironment for RealUpdateCheckerEnvironment {
  fn read_check_file(&self) -> String {
    std::fs::read_to_string(&self.cache_file_path).unwrap_or_default()
  }

  fn write_check_file(&self, text: &str) {
    let _ = std::fs::write(&self.cache_file_path, text);
  }

  fn current_time(&self) -> chrono::DateTime<chrono::Utc> {
    self.current_time
  }
}

#[derive(Debug, Copy, Clone)]
enum UpgradeCheckKind {
  Execution,
  Lsp,
}

#[async_trait(?Send)]
trait VersionProvider: Clone {
  /// Fetch latest available version for the given release channel
  async fn latest_version(
    &self,
    release_channel: ReleaseChannel,
  ) -> Result<String, AnyError>;

  // TODO(bartlomieju): what this one actually returns?
  fn current_version(&self) -> Cow<str>;

  // TODO(bartlomieju): update to handle `Lts` and `Rc` channels
  async fn get_current_exe_release_channel(
    &self,
  ) -> Result<ReleaseChannel, AnyError>;
}

#[derive(Clone)]
struct RealVersionProvider {
  http_client_provider: Arc<HttpClientProvider>,
  check_kind: UpgradeCheckKind,
}

impl RealVersionProvider {
  pub fn new(
    http_client_provider: Arc<HttpClientProvider>,
    check_kind: UpgradeCheckKind,
  ) -> Self {
    Self {
      http_client_provider,
      check_kind,
    }
  }
}

#[async_trait(?Send)]
impl VersionProvider for RealVersionProvider {
  async fn latest_version(
    &self,
    release_channel: ReleaseChannel,
  ) -> Result<String, AnyError> {
    fetch_latest_version(
      &self.http_client_provider.get_or_create()?,
      release_channel,
      self.check_kind,
    )
    .await
  }

  fn current_version(&self) -> Cow<str> {
    Cow::Borrowed(version::release_version_or_canary_commit_hash())
  }

  // TODO(bartlomieju): update to handle `Lts` and `Rc` channels
  async fn get_current_exe_release_channel(
    &self,
  ) -> Result<ReleaseChannel, AnyError> {
    if version::is_canary() {
      Ok(ReleaseChannel::Canary)
    } else {
      Ok(ReleaseChannel::Stable)
    }
  }
}

struct UpdateChecker<
  TEnvironment: UpdateCheckerEnvironment,
  TVersionProvider: VersionProvider,
> {
  env: TEnvironment,
  version_provider: TVersionProvider,
  maybe_file: Option<CheckVersionFile>,
}

impl<
    TEnvironment: UpdateCheckerEnvironment,
    TVersionProvider: VersionProvider,
  > UpdateChecker<TEnvironment, TVersionProvider>
{
  pub fn new(env: TEnvironment, version_provider: TVersionProvider) -> Self {
    let maybe_file = CheckVersionFile::parse(env.read_check_file());
    Self {
      env,
      version_provider,
      maybe_file,
    }
  }

  pub fn should_check_for_new_version(&self) -> bool {
    match &self.maybe_file {
      Some(file) => {
        let last_check_age = self
          .env
          .current_time()
          .signed_duration_since(file.last_checked);
        last_check_age > chrono::Duration::hours(UPGRADE_CHECK_INTERVAL)
      }
      None => true,
    }
  }

  /// Returns the version if a new one is available and it should be prompted about.
  pub fn should_prompt(&self) -> Option<String> {
    let file = self.maybe_file.as_ref()?;
    // If the current version saved is not the actually current version of the binary
    // It means
    // - We already check for a new version today
    // - The user have probably upgraded today
    // So we should not prompt and wait for tomorrow for the latest version to be updated again
    let current_version = self.version_provider.current_version();
    if file.current_version != current_version {
      return None;
    }
    if file.latest_version == current_version {
      return None;
    }

    if let Ok(current) = Version::parse_standard(&current_version) {
      if let Ok(latest) = Version::parse_standard(&file.latest_version) {
        if current >= latest {
          return None;
        }
      }
    }

    let last_prompt_age = self
      .env
      .current_time()
      .signed_duration_since(file.last_prompt);
    if last_prompt_age > chrono::Duration::hours(UPGRADE_CHECK_INTERVAL) {
      Some(file.latest_version.clone())
    } else {
      None
    }
  }

  /// Store that we showed the update message to the user.
  pub fn store_prompted(self) {
    if let Some(file) = self.maybe_file {
      self.env.write_check_file(
        &file.with_last_prompt(self.env.current_time()).serialize(),
      );
    }
  }
}

fn get_minor_version(version: &str) -> &str {
  version.rsplitn(2, '.').collect::<Vec<&str>>()[1]
}

fn print_release_notes(current_version: &str, new_version: &str) {
  if get_minor_version(current_version) == get_minor_version(new_version) {
    return;
  }

  log::info!(
    "Release notes:\n\n  {}\n",
    colors::bold(format!(
      "https://github.com/denoland/deno/releases/tag/v{}",
      &new_version,
    ))
  );
  log::info!(
    "Blog post:\n\n  {}\n",
    colors::bold(format!(
      "https://deno.com/blog/v{}",
      get_minor_version(new_version)
    ))
  );
}

pub fn upgrade_check_enabled() -> bool {
  matches!(
    env::var("DENO_NO_UPDATE_CHECK"),
    Err(env::VarError::NotPresent)
  )
}

pub fn check_for_upgrades(
  http_client_provider: Arc<HttpClientProvider>,
  cache_file_path: PathBuf,
) {
  if !upgrade_check_enabled() {
    return;
  }

  let env = RealUpdateCheckerEnvironment::new(cache_file_path);
  let version_provider =
    RealVersionProvider::new(http_client_provider, UpgradeCheckKind::Execution);
  let update_checker = UpdateChecker::new(env, version_provider);

  if update_checker.should_check_for_new_version() {
    let env = update_checker.env.clone();
    let version_provider = update_checker.version_provider.clone();
    // do this asynchronously on a separate task
    spawn(async move {
      // Sleep for a small amount of time to not unnecessarily impact startup
      // time.
      tokio::time::sleep(UPGRADE_CHECK_FETCH_DELAY).await;

      fetch_and_store_latest_version(&env, &version_provider).await;

      // text is used by the test suite
      log::debug!("Finished upgrade checker.")
    });
  }

  // Print a message if an update is available
  if let Some(upgrade_version) = update_checker.should_prompt() {
    if log::log_enabled!(log::Level::Info) && std::io::stderr().is_terminal() {
      if version::is_canary() {
        log::info!(
          "{} {}",
          colors::green("A new canary release of Deno is available."),
          colors::italic_gray("Run `deno upgrade --canary` to install it.")
        );
      } else {
        log::info!(
          "{} {} → {} {}",
          colors::green("A new release of Deno is available:"),
          colors::cyan(version::deno()),
          colors::cyan(&upgrade_version),
          colors::italic_gray("Run `deno upgrade` to install it.")
        );
      }

      update_checker.store_prompted();
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspVersionUpgradeInfo {
  pub latest_version: String,
  // TODO(bartlomieju): use `ReleaseChannel` instead
  pub is_canary: bool,
}

pub async fn check_for_upgrades_for_lsp(
  http_client_provider: Arc<HttpClientProvider>,
) -> Result<Option<LspVersionUpgradeInfo>, AnyError> {
  if !upgrade_check_enabled() {
    return Ok(None);
  }

  let version_provider =
    RealVersionProvider::new(http_client_provider, UpgradeCheckKind::Lsp);
  check_for_upgrades_for_lsp_with_provider(&version_provider).await
}

async fn check_for_upgrades_for_lsp_with_provider(
  version_provider: &impl VersionProvider,
) -> Result<Option<LspVersionUpgradeInfo>, AnyError> {
  let release_channel =
    version_provider.get_current_exe_release_channel().await?;
  let latest_version = version_provider.latest_version(release_channel).await?;
  let current_version = version_provider.current_version();

  // Nothing to upgrade
  if current_version == latest_version {
    return Ok(None);
  }

  match release_channel {
    ReleaseChannel::Stable => {
      if let Ok(current) = Version::parse_standard(&current_version) {
        if let Ok(latest) = Version::parse_standard(&latest_version) {
          if current >= latest {
            return Ok(None); // nothing to upgrade
          }
        }
      }
      Ok(Some(LspVersionUpgradeInfo {
        latest_version,
        is_canary: false,
      }))
    }

    ReleaseChannel::Canary => Ok(Some(LspVersionUpgradeInfo {
      latest_version,
      is_canary: true,
    })),

    // TODO(bartlomieju)
    ReleaseChannel::Lts => unreachable!(),
    // TODO(bartlomieju)
    ReleaseChannel::Rc => unreachable!(),
  }
}

async fn fetch_and_store_latest_version<
  TEnvironment: UpdateCheckerEnvironment,
  TVersionProvider: VersionProvider,
>(
  env: &TEnvironment,
  version_provider: &TVersionProvider,
) {
  // Fetch latest version or commit hash from server.
  let Ok(release_channel) =
    version_provider.get_current_exe_release_channel().await
  else {
    return;
  };
  let Ok(latest_version) =
    version_provider.latest_version(release_channel).await
  else {
    return;
  };

  env.write_check_file(
    &CheckVersionFile {
      // put a date in the past here so that prompt can be shown on next run
      last_prompt: env
        .current_time()
        .sub(chrono::Duration::hours(UPGRADE_CHECK_INTERVAL + 1)),
      last_checked: env.current_time(),
      current_version: version_provider.current_version().to_string(),
      latest_version,
    }
    .serialize(),
  );
}

pub async fn upgrade(
  flags: Arc<Flags>,
  upgrade_flags: UpgradeFlags,
) -> Result<(), AnyError> {
  let factory = CliFactory::from_flags(flags);
  let http_client_provider = factory.http_client_provider();
  let client = http_client_provider.get_or_create()?;
  let current_exe_path = std::env::current_exe()?;
  let full_path_output_flag = match &upgrade_flags.output {
    Some(output) => Some(
      std::env::current_dir()
        .context("failed getting cwd")?
        .join(output),
    ),
    None => None,
  };
  let output_exe_path =
    full_path_output_flag.as_ref().unwrap_or(&current_exe_path);

  let permissions = set_exe_permissions(&current_exe_path, output_exe_path)?;

  let force_selection_of_new_version =
    upgrade_flags.force || full_path_output_flag.is_some();

  let requested_version =
    RequestedVersion::from_upgrade_flags(upgrade_flags.clone())?;

  let maybe_install_version = match requested_version {
    RequestedVersion::Latest(channel) => {
      find_latest_version_to_upgrade(
        http_client_provider.clone(),
        channel,
        force_selection_of_new_version,
      )
      .await?
    }
    RequestedVersion::SpecificVersion(channel, version) => {
      select_specific_version_for_upgrade(
        channel,
        version,
        force_selection_of_new_version,
      )?
    }
  };

  let Some(install_version) = maybe_install_version else {
    return Ok(());
  };

  let download_url = get_download_url(&install_version, upgrade_flags.canary)?;
  log::info!("{}", colors::gray(format!("Downloading {}", &download_url)));
  let Some(archive_data) = download_package(&client, download_url).await?
  else {
    log::error!("Download could not be found, aborting");
    std::process::exit(1)
  };

  log::info!(
    "{}",
    colors::gray(format!("Deno is upgrading to version {}", &install_version))
  );

  let temp_dir = tempfile::TempDir::new()?;
  let new_exe_path = archive::unpack_into_dir(archive::UnpackArgs {
    exe_name: "deno",
    archive_name: &ARCHIVE_NAME,
    archive_data: &archive_data,
    is_windows: cfg!(windows),
    dest_path: temp_dir.path(),
  })?;
  fs::set_permissions(&new_exe_path, permissions)?;
  check_exe(&new_exe_path)?;

  if upgrade_flags.dry_run {
    fs::remove_file(&new_exe_path)?;
    log::info!("Upgraded successfully (dry run)");
    if !upgrade_flags.canary {
      print_release_notes(version::deno(), &install_version);
    }
    drop(temp_dir);
    return Ok(());
  }

  let output_exe_path =
    full_path_output_flag.as_ref().unwrap_or(&current_exe_path);
  let output_result = if *output_exe_path == current_exe_path {
    replace_exe(&new_exe_path, output_exe_path)
  } else {
    fs::rename(&new_exe_path, output_exe_path)
      .or_else(|_| fs::copy(&new_exe_path, output_exe_path).map(|_| ()))
  };
  check_windows_access_denied_error(output_result, output_exe_path)?;

  log::info!(
    "{}",
    colors::green(format!(
      "\nUpgraded successfully to Deno v{}\n",
      install_version
    ))
  );
  if !upgrade_flags.canary {
    print_release_notes(version::deno(), &install_version);
  }

  drop(temp_dir); // delete the temp dir
  Ok(())
}

enum RequestedVersion {
  Latest(ReleaseChannel),
  SpecificVersion(ReleaseChannel, String),
}

impl RequestedVersion {
  fn from_upgrade_flags(upgrade_flags: UpgradeFlags) -> Result<Self, AnyError> {
    let is_canary = upgrade_flags.canary;

    let Some(passed_version) = upgrade_flags.version else {
      let channel = if is_canary {
        ReleaseChannel::Canary
      } else {
        ReleaseChannel::Stable
      };
      return Ok(Self::Latest(channel));
    };

    let re_hash = lazy_regex::regex!("^[0-9a-f]{40}$");
    let passed_version = passed_version
      .strip_prefix('v')
      .unwrap_or(&passed_version)
      .to_string();

    let (channel, passed_version) = if is_canary {
      if !re_hash.is_match(&passed_version) {
        bail!("Invalid commit hash passed");
      }
      (ReleaseChannel::Canary, passed_version)
    } else {
      if Version::parse_standard(&passed_version).is_err() {
        bail!("Invalid version passed");
      };
      (ReleaseChannel::Stable, passed_version)
    };

    Ok(RequestedVersion::SpecificVersion(channel, passed_version))
  }
}

fn select_specific_version_for_upgrade(
  release_channel: ReleaseChannel,
  version: String,
  force: bool,
) -> Result<Option<String>, AnyError> {
  match release_channel {
    ReleaseChannel::Stable => {
      let current_is_passed = if !version::is_canary() {
        version::deno() == version
      } else {
        false
      };

      if !force && current_is_passed {
        log::info!("Version {} is already installed", version::deno());
        return Ok(None);
      }

      Ok(Some(version))
    }
    ReleaseChannel::Canary => {
      let current_is_passed = version::GIT_COMMIT_HASH == version;
      if !force && current_is_passed {
        log::info!("Version {} is already installed", version::deno());
        return Ok(None);
      }

      Ok(Some(version))
    }
    // TODO(bartlomieju)
    ReleaseChannel::Rc => unreachable!(),
    // TODO(bartlomieju)
    ReleaseChannel::Lts => unreachable!(),
  }
}

async fn find_latest_version_to_upgrade(
  http_client_provider: Arc<HttpClientProvider>,
  release_channel: ReleaseChannel,
  force: bool,
) -> Result<Option<String>, AnyError> {
  log::info!(
    "{}",
    colors::gray(&format!("Looking up {} version", release_channel.name()))
  );

  let client = http_client_provider.get_or_create()?;
  let latest_version =
    fetch_latest_version(&client, release_channel, UpgradeCheckKind::Execution)
      .await?;

  let (maybe_newer_latest_version, current_version) = match release_channel {
    ReleaseChannel::Stable => {
      let current_version = version::deno();
      let current_is_most_recent = if !version::is_canary() {
        let current = Version::parse_standard(current_version).unwrap();
        let latest = Version::parse_standard(&latest_version).unwrap();
        current >= latest
      } else {
        false
      };

      if !force && current_is_most_recent {
        (None, current_version)
      } else {
        (Some(latest_version), current_version)
      }
    }
    ReleaseChannel::Canary => {
      let current_version = version::GIT_COMMIT_HASH;
      let current_is_most_recent = current_version == latest_version;

      if !force && current_is_most_recent {
        (None, current_version)
      } else {
        (Some(latest_version), current_version)
      }
    }
    // TODO(bartlomieju)
    ReleaseChannel::Rc => unreachable!(),
    // TODO(bartlomieju)
    ReleaseChannel::Lts => unreachable!(),
  };

  log::info!("");
  if let Some(newer_latest_version) = maybe_newer_latest_version.as_ref() {
    log::info!(
      "{}",
      color_print::cformat!(
        "<g>Found latest version {}</>",
        newer_latest_version
      )
    );
  } else {
    log::info!(
      "{}",
      color_print::cformat!(
        "<g>Local deno version {} is the most recent release</>",
        current_version
      )
    );
  }
  log::info!("");

  Ok(maybe_newer_latest_version)
}

#[derive(Debug, Clone, Copy)]
enum ReleaseChannel {
  /// Stable version, eg. 1.45.4, 2.0.0, 2.1.0
  Stable,

  /// Pointing to a git hash
  Canary,

  /// Long term support release
  #[allow(unused)]
  Lts,

  /// Release candidate
  #[allow(unused)]
  Rc,
}

impl ReleaseChannel {
  fn name(&self) -> &str {
    match self {
      Self::Stable => "latest",
      Self::Canary => "canary",
      Self::Rc => "release candidate",
      Self::Lts => "LTS (long term support)",
    }
  }
}

async fn fetch_latest_version(
  client: &HttpClient,
  release_channel: ReleaseChannel,
  check_kind: UpgradeCheckKind,
) -> Result<String, AnyError> {
  let url = get_latest_version_url(release_channel, env!("TARGET"), check_kind);
  let text = client.download_text(url.parse()?).await?;
  Ok(normalize_version_from_server(release_channel, &text))
}

fn normalize_version_from_server(
  release_channel: ReleaseChannel,
  text: &str,
) -> String {
  let text = text.trim();
  match release_channel {
    ReleaseChannel::Stable => text.trim_start_matches('v').to_string(),
    ReleaseChannel::Canary => text.to_string(),
    _ => unreachable!(),
  }
}

fn get_latest_version_url(
  release_channel: ReleaseChannel,
  target_tuple: &str,
  check_kind: UpgradeCheckKind,
) -> String {
  let file_name = match release_channel {
    ReleaseChannel::Stable => Cow::Borrowed("release-latest.txt"),
    ReleaseChannel::Canary => {
      Cow::Owned(format!("canary-{target_tuple}-latest.txt"))
    }
    _ => unreachable!(),
  };
  let query_param = match check_kind {
    UpgradeCheckKind::Execution => "",
    UpgradeCheckKind::Lsp => "?lsp",
  };
  format!("{}/{}{}", base_upgrade_url(), file_name, query_param)
}

fn base_upgrade_url() -> Cow<'static, str> {
  // this is used by the test suite
  if let Ok(url) = env::var("DENO_DONT_USE_INTERNAL_BASE_UPGRADE_URL") {
    Cow::Owned(url)
  } else {
    Cow::Borrowed("https://dl.deno.land")
  }
}

fn get_download_url(version: &str, is_canary: bool) -> Result<Url, AnyError> {
  let download_url = if is_canary {
    format!("{}/{}/{}", CANARY_URL, version, *ARCHIVE_NAME)
  } else {
    format!("{}/download/v{}/{}", RELEASE_URL, version, *ARCHIVE_NAME)
  };

  Url::parse(&download_url).with_context(|| {
    format!(
      "Failed to parse URL to download new release: {}",
      download_url
    )
  })
}

async fn download_package(
  client: &HttpClient,
  download_url: Url,
) -> Result<Option<Vec<u8>>, AnyError> {
  let progress_bar = ProgressBar::new(ProgressBarStyle::DownloadBars);
  // provide an empty string here in order to prefer the downloading
  // text above which will stay alive after the progress bars are complete
  let progress = progress_bar.update("");
  let maybe_bytes = client
    .download_with_progress(download_url.clone(), None, &progress)
    .await
    .with_context(|| format!("Failed downloading {download_url}. The version you requested may not have been built for the current architecture."))?;
  Ok(maybe_bytes)
}

fn replace_exe(from: &Path, to: &Path) -> Result<(), std::io::Error> {
  if cfg!(windows) {
    // On windows you cannot replace the currently running executable.
    // so first we rename it to deno.old.exe
    fs::rename(to, to.with_extension("old.exe"))?;
  } else {
    fs::remove_file(to)?;
  }
  // Windows cannot rename files across device boundaries, so if rename fails,
  // we try again with copy.
  fs::rename(from, to).or_else(|_| fs::copy(from, to).map(|_| ()))?;
  Ok(())
}

fn check_windows_access_denied_error(
  output_result: Result<(), std::io::Error>,
  output_exe_path: &Path,
) -> Result<(), AnyError> {
  let Err(err) = output_result else {
    return Ok(());
  };

  if !cfg!(windows) {
    return Err(err.into());
  }

  const WIN_ERROR_ACCESS_DENIED: i32 = 5;
  if err.raw_os_error() != Some(WIN_ERROR_ACCESS_DENIED) {
    return Err(err.into());
  };

  Err(err).with_context(|| {
    format!(
      concat!(
        "Could not replace the deno executable. This may be because an ",
        "existing deno process is running. Please ensure there are no ",
        "running deno processes (ex. Stop-Process -Name deno ; deno {}), ",
        "close any editors before upgrading, and ensure you have ",
        "sufficient permission to '{}'."
      ),
      // skip the first argument, which is the executable path
      std::env::args().skip(1).collect::<Vec<_>>().join(" "),
      output_exe_path.display(),
    )
  })
}

fn set_exe_permissions(
  current_exe_path: &Path,
  output_exe_path: &Path,
) -> Result<std::fs::Permissions, AnyError> {
  let Ok(metadata) = fs::metadata(output_exe_path) else {
    let metadata = fs::metadata(current_exe_path)?;
    return Ok(metadata.permissions());
  };

  let permissions = metadata.permissions();
  if permissions.readonly() {
    bail!(
      "You do not have write permission to {}",
      output_exe_path.display()
    );
  }
  #[cfg(unix)]
  if std::os::unix::fs::MetadataExt::uid(&metadata) == 0
    && !nix::unistd::Uid::effective().is_root()
  {
    bail!(concat!(
      "You don't have write permission to {} because it's owned by root.\n",
      "Consider updating deno through your package manager if its installed from it.\n",
      "Otherwise run `deno upgrade` as root.",
    ), output_exe_path.display());
  }
  Ok(permissions)
}

fn check_exe(exe_path: &Path) -> Result<(), AnyError> {
  let output = Command::new(exe_path)
    .arg("-V")
    .stderr(std::process::Stdio::inherit())
    .output()?;
  assert!(output.status.success());
  Ok(())
}

#[derive(Debug)]
struct CheckVersionFile {
  pub last_prompt: chrono::DateTime<chrono::Utc>,
  pub last_checked: chrono::DateTime<chrono::Utc>,
  pub current_version: String,
  pub latest_version: String,
}

impl CheckVersionFile {
  pub fn parse(content: String) -> Option<Self> {
    let split_content = content.split('!').collect::<Vec<_>>();

    if split_content.len() != 4 {
      return None;
    }

    let latest_version = split_content[2].trim().to_owned();
    if latest_version.is_empty() {
      return None;
    }
    let current_version = split_content[3].trim().to_owned();
    if current_version.is_empty() {
      return None;
    }

    let last_prompt = chrono::DateTime::parse_from_rfc3339(split_content[0])
      .map(|dt| dt.with_timezone(&chrono::Utc))
      .ok()?;
    let last_checked = chrono::DateTime::parse_from_rfc3339(split_content[1])
      .map(|dt| dt.with_timezone(&chrono::Utc))
      .ok()?;

    Some(CheckVersionFile {
      last_prompt,
      last_checked,
      current_version,
      latest_version,
    })
  }

  fn serialize(&self) -> String {
    format!(
      "{}!{}!{}!{}",
      self.last_prompt.to_rfc3339(),
      self.last_checked.to_rfc3339(),
      self.latest_version,
      self.current_version,
    )
  }

  fn with_last_prompt(self, dt: chrono::DateTime<chrono::Utc>) -> Self {
    Self {
      last_prompt: dt,
      ..self
    }
  }
}

#[cfg(test)]
mod test {
  use std::cell::RefCell;
  use std::rc::Rc;

  use super::*;

  #[test]
  fn test_parse_upgrade_check_file() {
    let file = CheckVersionFile::parse(
      "2020-01-01T00:00:00+00:00!2020-01-01T00:00:00+00:00!1.2.3!1.2.2"
        .to_string(),
    )
    .unwrap();
    assert_eq!(
      file.last_prompt.to_rfc3339(),
      "2020-01-01T00:00:00+00:00".to_string()
    );
    assert_eq!(
      file.last_checked.to_rfc3339(),
      "2020-01-01T00:00:00+00:00".to_string()
    );
    assert_eq!(file.latest_version, "1.2.3".to_string());
    assert_eq!(file.current_version, "1.2.2".to_string());

    let result =
      CheckVersionFile::parse("2020-01-01T00:00:00+00:00!".to_string());
    assert!(result.is_none());

    let result = CheckVersionFile::parse("garbage!test".to_string());
    assert!(result.is_none());

    let result = CheckVersionFile::parse("test".to_string());
    assert!(result.is_none());
  }

  #[test]
  fn test_serialize_upgrade_check_file() {
    let file = CheckVersionFile {
      last_prompt: chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc),
      last_checked: chrono::DateTime::parse_from_rfc3339(
        "2020-01-01T00:00:00Z",
      )
      .unwrap()
      .with_timezone(&chrono::Utc),
      latest_version: "1.2.3".to_string(),
      current_version: "1.2.2".to_string(),
    };
    assert_eq!(
      file.serialize(),
      "2020-01-01T00:00:00+00:00!2020-01-01T00:00:00+00:00!1.2.3!1.2.2"
    );
  }

  #[derive(Clone)]
  struct TestUpdateCheckerEnvironment {
    file_text: Rc<RefCell<String>>,
    is_canary: Rc<RefCell<bool>>,
    current_version: Rc<RefCell<String>>,
    latest_version: Rc<RefCell<Result<String, String>>>,
    time: Rc<RefCell<chrono::DateTime<chrono::Utc>>>,
  }

  impl TestUpdateCheckerEnvironment {
    pub fn new() -> Self {
      Self {
        file_text: Default::default(),
        current_version: Default::default(),
        is_canary: Default::default(),
        latest_version: Rc::new(RefCell::new(Ok("".to_string()))),
        time: Rc::new(RefCell::new(chrono::Utc::now())),
      }
    }

    pub fn add_hours(&self, hours: i64) {
      let mut time = self.time.borrow_mut();
      *time = time
        .checked_add_signed(chrono::Duration::hours(hours))
        .unwrap();
    }

    pub fn set_file_text(&self, text: &str) {
      *self.file_text.borrow_mut() = text.to_string();
    }

    pub fn set_current_version(&self, version: &str) {
      *self.current_version.borrow_mut() = version.to_string();
    }

    pub fn set_latest_version(&self, version: &str) {
      *self.latest_version.borrow_mut() = Ok(version.to_string());
    }

    pub fn set_latest_version_err(&self, err: &str) {
      *self.latest_version.borrow_mut() = Err(err.to_string());
    }

    pub fn set_is_canary(&self, is_canary: bool) {
      *self.is_canary.borrow_mut() = is_canary;
    }
  }

  #[async_trait(?Send)]
  impl VersionProvider for TestUpdateCheckerEnvironment {
    // TODO(bartlomieju): update to handle `Lts` and `Rc` channels
    async fn latest_version(
      &self,
      _release_channel: ReleaseChannel,
    ) -> Result<String, AnyError> {
      match self.latest_version.borrow().clone() {
        Ok(result) => Ok(result),
        Err(err) => bail!("{}", err),
      }
    }

    fn current_version(&self) -> Cow<str> {
      Cow::Owned(self.current_version.borrow().clone())
    }

    // TODO(bartlomieju): update to handle `Lts` and `Rc` channels
    async fn get_current_exe_release_channel(
      &self,
    ) -> Result<ReleaseChannel, AnyError> {
      if *self.is_canary.borrow() {
        Ok(ReleaseChannel::Canary)
      } else {
        Ok(ReleaseChannel::Stable)
      }
    }
  }

  impl UpdateCheckerEnvironment for TestUpdateCheckerEnvironment {
    fn read_check_file(&self) -> String {
      self.file_text.borrow().clone()
    }

    fn write_check_file(&self, text: &str) {
      self.set_file_text(text);
    }

    fn current_time(&self) -> chrono::DateTime<chrono::Utc> {
      *self.time.borrow()
    }
  }

  #[tokio::test]
  async fn test_update_checker() {
    let env = TestUpdateCheckerEnvironment::new();
    env.set_current_version("1.0.0");
    env.set_latest_version("1.1.0");
    let checker = UpdateChecker::new(env.clone(), env.clone());

    // no version, so we should check, but not prompt
    assert!(checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), None);

    // store the latest version
    fetch_and_store_latest_version(&env, &env).await;

    // reload
    let checker = UpdateChecker::new(env.clone(), env.clone());

    // should not check for latest version because we just did
    assert!(!checker.should_check_for_new_version());
    // but should prompt
    assert_eq!(checker.should_prompt(), Some("1.1.0".to_string()));

    // fast forward an hour and bump the latest version
    env.add_hours(1);
    env.set_latest_version("1.2.0");
    assert!(!checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), Some("1.1.0".to_string()));

    // fast forward again and it should check for a newer version
    env.add_hours(UPGRADE_CHECK_INTERVAL);
    assert!(checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), Some("1.1.0".to_string()));

    fetch_and_store_latest_version(&env, &env).await;

    // reload and store that we prompted
    let checker = UpdateChecker::new(env.clone(), env.clone());
    assert!(!checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), Some("1.2.0".to_string()));
    checker.store_prompted();

    // reload and it should now say not to prompt
    let checker = UpdateChecker::new(env.clone(), env.clone());
    assert!(!checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), None);

    // but if we fast forward past the upgrade interval it should prompt again
    env.add_hours(UPGRADE_CHECK_INTERVAL + 1);
    assert!(checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), Some("1.2.0".to_string()));

    // upgrade the version and it should stop prompting
    env.set_current_version("1.2.0");
    assert!(checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), None);

    // now try failing when fetching the latest version
    env.add_hours(UPGRADE_CHECK_INTERVAL + 1);
    env.set_latest_version_err("Failed");
    env.set_latest_version("1.3.0");

    // this will silently fail
    fetch_and_store_latest_version(&env, &env).await;
    assert!(checker.should_check_for_new_version());
    assert_eq!(checker.should_prompt(), None);
  }

  #[tokio::test]
  async fn test_update_checker_current_newer_than_latest() {
    let env = TestUpdateCheckerEnvironment::new();
    let file_content = CheckVersionFile {
      last_prompt: env
        .current_time()
        .sub(chrono::Duration::hours(UPGRADE_CHECK_INTERVAL + 1)),
      last_checked: env.current_time(),
      latest_version: "1.26.2".to_string(),
      current_version: "1.27.0".to_string(),
    }
    .serialize();
    env.write_check_file(&file_content);
    env.set_current_version("1.27.0");
    env.set_latest_version("1.26.2");
    let checker = UpdateChecker::new(env.clone(), env);

    // since currently running version is newer than latest available (eg. CDN
    // propagation might be delated) we should not prompt
    assert_eq!(checker.should_prompt(), None);
  }

  #[tokio::test]
  async fn test_should_not_prompt_if_current_cli_version_has_changed() {
    let env = TestUpdateCheckerEnvironment::new();
    let file_content = CheckVersionFile {
      last_prompt: env
        .current_time()
        .sub(chrono::Duration::hours(UPGRADE_CHECK_INTERVAL + 1)),
      last_checked: env.current_time(),
      latest_version: "1.26.2".to_string(),
      current_version: "1.25.0".to_string(),
    }
    .serialize();
    env.write_check_file(&file_content);
    // simulate an upgrade done to a canary version
    env.set_current_version("61fbfabe440f1cfffa7b8d17426ffdece4d430d0");
    let checker = UpdateChecker::new(env.clone(), env);
    assert_eq!(checker.should_prompt(), None);
  }

  #[test]
  fn test_get_latest_version_url() {
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Canary,
        "aarch64-apple-darwin",
        UpgradeCheckKind::Execution
      ),
      "https://dl.deno.land/canary-aarch64-apple-darwin-latest.txt"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Canary,
        "aarch64-apple-darwin",
        UpgradeCheckKind::Lsp
      ),
      "https://dl.deno.land/canary-aarch64-apple-darwin-latest.txt?lsp"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Canary,
        "x86_64-pc-windows-msvc",
        UpgradeCheckKind::Execution
      ),
      "https://dl.deno.land/canary-x86_64-pc-windows-msvc-latest.txt"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Canary,
        "x86_64-pc-windows-msvc",
        UpgradeCheckKind::Lsp
      ),
      "https://dl.deno.land/canary-x86_64-pc-windows-msvc-latest.txt?lsp"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Stable,
        "aarch64-apple-darwin",
        UpgradeCheckKind::Execution
      ),
      "https://dl.deno.land/release-latest.txt"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Stable,
        "aarch64-apple-darwin",
        UpgradeCheckKind::Lsp
      ),
      "https://dl.deno.land/release-latest.txt?lsp"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Stable,
        "x86_64-pc-windows-msvc",
        UpgradeCheckKind::Execution
      ),
      "https://dl.deno.land/release-latest.txt"
    );
    assert_eq!(
      get_latest_version_url(
        ReleaseChannel::Stable,
        "x86_64-pc-windows-msvc",
        UpgradeCheckKind::Lsp
      ),
      "https://dl.deno.land/release-latest.txt?lsp"
    );
  }

  #[test]
  fn test_normalize_version_server() {
    // should strip v for stable
    assert_eq!(
      normalize_version_from_server(ReleaseChannel::Stable, "v1.0.0"),
      "1.0.0"
    );
    // should not replace v after start
    assert_eq!(
      normalize_version_from_server(
        ReleaseChannel::Stable,
        "  v1.0.0-test-v\n\n  "
      ),
      "1.0.0-test-v"
    );
    // should not strip v for canary
    assert_eq!(
      normalize_version_from_server(
        ReleaseChannel::Canary,
        "  v1452345asdf   \n\n   "
      ),
      "v1452345asdf"
    );
  }

  #[tokio::test]
  async fn test_upgrades_lsp() {
    let env = TestUpdateCheckerEnvironment::new();
    env.set_current_version("1.0.0");
    env.set_latest_version("2.0.0");

    // greater
    {
      let maybe_info = check_for_upgrades_for_lsp_with_provider(&env)
        .await
        .unwrap();
      assert_eq!(
        maybe_info,
        Some(LspVersionUpgradeInfo {
          latest_version: "2.0.0".to_string(),
          is_canary: false,
        })
      );
    }
    // equal
    {
      env.set_latest_version("1.0.0");
      let maybe_info = check_for_upgrades_for_lsp_with_provider(&env)
        .await
        .unwrap();
      assert_eq!(maybe_info, None);
    }
    // less
    {
      env.set_latest_version("0.9.0");
      let maybe_info = check_for_upgrades_for_lsp_with_provider(&env)
        .await
        .unwrap();
      assert_eq!(maybe_info, None);
    }
    // canary equal
    {
      env.set_current_version("123");
      env.set_latest_version("123");
      env.set_is_canary(true);
      let maybe_info = check_for_upgrades_for_lsp_with_provider(&env)
        .await
        .unwrap();
      assert_eq!(maybe_info, None);
    }
    // canary different
    {
      env.set_latest_version("1234");
      let maybe_info = check_for_upgrades_for_lsp_with_provider(&env)
        .await
        .unwrap();
      assert_eq!(
        maybe_info,
        Some(LspVersionUpgradeInfo {
          latest_version: "1234".to_string(),
          is_canary: true,
        })
      );
    }
  }
}
