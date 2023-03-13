// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::rc::Rc;

use backtrace::Backtrace;
use os_pipe::pipe;
use pretty_assertions::assert_eq;

use crate::copy_dir_recursive;
use crate::deno_exe_path;
use crate::env_vars_for_npm_tests_no_sync_download;
use crate::http_server;
use crate::lsp::LspClientBuilder;
use crate::new_deno_dir;
use crate::strip_ansi_codes;
use crate::testdata_path;
use crate::wildcard_match;
use crate::HttpServerGuard;
use crate::TempDir;

#[derive(Default)]
pub struct TestContextBuilder {
  use_http_server: bool,
  use_temp_cwd: bool,
  use_separate_deno_dir: bool,
  /// Copies the files at the specified directory in the "testdata" directory
  /// to the temp folder and runs the test from there. This is useful when
  /// the test creates files in the testdata directory (ex. a node_modules folder)
  copy_temp_dir: Option<String>,
  cwd: Option<String>,
  envs: HashMap<String, String>,
  deno_exe: Option<PathBuf>,
}

impl TestContextBuilder {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn for_npm() -> Self {
    let mut builder = Self::new();
    builder.use_http_server().add_npm_env_vars();
    builder
  }

  pub fn use_http_server(&mut self) -> &mut Self {
    self.use_http_server = true;
    self
  }

  pub fn use_temp_cwd(&mut self) -> &mut Self {
    self.use_temp_cwd = true;
    self
  }

  /// By default, the temp_dir and the deno_dir will be shared.
  /// In some cases, that might cause an issue though, so calling
  /// this will use a separate directory for the deno dir and the
  /// temp directory.
  pub fn use_separate_deno_dir(&mut self) -> &mut Self {
    self.use_separate_deno_dir = true;
    self
  }

  /// Copies the files at the specified directory in the "testdata" directory
  /// to the temp folder and runs the test from there. This is useful when
  /// the test creates files in the testdata directory (ex. a node_modules folder)
  pub fn use_copy_temp_dir(&mut self, dir: impl AsRef<str>) -> &mut Self {
    self.copy_temp_dir = Some(dir.as_ref().to_string());
    self
  }

  pub fn cwd(&mut self, cwd: impl AsRef<str>) -> &mut Self {
    self.cwd = Some(cwd.as_ref().to_string());
    self
  }

  pub fn env(
    &mut self,
    key: impl AsRef<str>,
    value: impl AsRef<str>,
  ) -> &mut Self {
    self
      .envs
      .insert(key.as_ref().to_string(), value.as_ref().to_string());
    self
  }

  pub fn add_npm_env_vars(&mut self) -> &mut Self {
    for (key, value) in env_vars_for_npm_tests_no_sync_download() {
      self.env(key, value);
    }
    self
  }

  pub fn use_sync_npm_download(&mut self) -> &mut Self {
    self.env(
      // make downloads determinstic
      "DENO_UNSTABLE_NPM_SYNC_DOWNLOAD",
      "1",
    );
    self
  }

  pub fn build(&self) -> TestContext {
    let deno_dir = new_deno_dir(); // keep this alive for the test
    let temp_dir = if self.use_separate_deno_dir {
      TempDir::new()
    } else {
      deno_dir.clone()
    };
    let testdata_dir = if let Some(temp_copy_dir) = &self.copy_temp_dir {
      let test_data_path = testdata_path().join(temp_copy_dir);
      let temp_copy_dir = temp_dir.path().join(temp_copy_dir);
      std::fs::create_dir_all(&temp_copy_dir).unwrap();
      copy_dir_recursive(&test_data_path, &temp_copy_dir).unwrap();
      temp_dir.path().to_owned()
    } else {
      testdata_path()
    };

    let deno_exe = self.deno_exe.clone().unwrap_or_else(deno_exe_path);
    println!("deno_exe path {}", deno_exe.display());

    let http_server_guard = if self.use_http_server {
      Some(Rc::new(http_server()))
    } else {
      None
    };

    TestContext {
      cwd: self.cwd.clone(),
      deno_exe,
      envs: self.envs.clone(),
      use_temp_cwd: self.use_temp_cwd,
      _http_server_guard: http_server_guard,
      deno_dir,
      temp_dir,
      testdata_dir,
    }
  }
}

#[derive(Clone)]
pub struct TestContext {
  deno_exe: PathBuf,
  envs: HashMap<String, String>,
  use_temp_cwd: bool,
  cwd: Option<String>,
  _http_server_guard: Option<Rc<HttpServerGuard>>,
  deno_dir: TempDir,
  temp_dir: TempDir,
  testdata_dir: PathBuf,
}

impl Default for TestContext {
  fn default() -> Self {
    TestContextBuilder::default().build()
  }
}

impl TestContext {
  pub fn with_http_server() -> Self {
    TestContextBuilder::default().use_http_server().build()
  }

  pub fn testdata_path(&self) -> &PathBuf {
    &self.testdata_dir
  }

  pub fn deno_dir(&self) -> &TempDir {
    &self.deno_dir
  }

  pub fn temp_dir(&self) -> &TempDir {
    &self.temp_dir
  }

  pub fn new_command(&self) -> TestCommandBuilder {
    TestCommandBuilder {
      command_name: self.deno_exe.to_string_lossy().to_string(),
      args: Default::default(),
      args_vec: Default::default(),
      stdin: Default::default(),
      envs: Default::default(),
      env_clear: Default::default(),
      cwd: Default::default(),
      split_output: false,
      context: self.clone(),
    }
  }

  pub fn new_lsp_command(&self) -> LspClientBuilder {
    let mut builder = LspClientBuilder::new();
    builder.deno_exe(&self.deno_exe).set_test_context(self);
    builder
  }
}

pub struct TestCommandBuilder {
  command_name: String,
  args: String,
  args_vec: Vec<String>,
  stdin: Option<String>,
  envs: HashMap<String, String>,
  env_clear: bool,
  cwd: Option<String>,
  split_output: bool,
  context: TestContext,
}

impl TestCommandBuilder {
  pub fn command_name(&mut self, name: impl AsRef<str>) -> &mut Self {
    self.command_name = name.as_ref().to_string();
    self
  }

  pub fn args(&mut self, text: impl AsRef<str>) -> &mut Self {
    self.args = text.as_ref().to_string();
    self
  }

  pub fn args_vec(&mut self, args: Vec<String>) -> &mut Self {
    self.args_vec = args;
    self
  }

  pub fn stdin(&mut self, text: impl AsRef<str>) -> &mut Self {
    self.stdin = Some(text.as_ref().to_string());
    self
  }

  /// Splits the output into stdout and stderr rather than having them combined.
  pub fn split_output(&mut self) -> &mut Self {
    // Note: it was previously attempted to capture stdout & stderr separately
    // then forward the output to a combined pipe, but this was found to be
    // too racy compared to providing the same combined pipe to both.
    self.split_output = true;
    self
  }

  pub fn env(
    &mut self,
    key: impl AsRef<str>,
    value: impl AsRef<str>,
  ) -> &mut Self {
    self
      .envs
      .insert(key.as_ref().to_string(), value.as_ref().to_string());
    self
  }

  pub fn env_clear(&mut self) -> &mut Self {
    self.env_clear = true;
    self
  }

  pub fn cwd(&mut self, cwd: impl AsRef<str>) -> &mut Self {
    self.cwd = Some(cwd.as_ref().to_string());
    self
  }

  pub fn run(&self) -> TestCommandOutput {
    fn read_pipe_to_string(mut pipe: os_pipe::PipeReader) -> String {
      let mut output = String::new();
      pipe.read_to_string(&mut output).unwrap();
      output
    }

    fn sanitize_output(text: String, args: &[String]) -> String {
      let mut text = strip_ansi_codes(&text).to_string();
      // deno test's output capturing flushes with a zero-width space in order to
      // synchronize the output pipes. Occassionally this zero width space
      // might end up in the output so strip it from the output comparison here.
      if args.first().map(|s| s.as_str()) == Some("test") {
        text = text.replace('\u{200B}', "");
      }
      text
    }

    let cwd = self.cwd.as_ref().or(self.context.cwd.as_ref());
    let cwd = if self.context.use_temp_cwd {
      assert!(cwd.is_none());
      self.context.temp_dir.path().to_owned()
    } else if let Some(cwd_) = cwd {
      self.context.testdata_dir.join(cwd_)
    } else {
      self.context.testdata_dir.clone()
    };
    let args = if self.args_vec.is_empty() {
      std::borrow::Cow::Owned(
        self
          .args
          .split_whitespace()
          .map(|s| s.to_string())
          .collect::<Vec<_>>(),
      )
    } else {
      assert!(
        self.args.is_empty(),
        "Do not provide args when providing args_vec."
      );
      std::borrow::Cow::Borrowed(&self.args_vec)
    }
    .iter()
    .map(|arg| {
      arg.replace("$TESTDATA", &self.context.testdata_dir.to_string_lossy())
    })
    .collect::<Vec<_>>();
    let command_name = &self.command_name;
    let mut command = if command_name == "deno" {
      Command::new(deno_exe_path())
    } else {
      Command::new(command_name)
    };
    command.env("DENO_DIR", self.context.deno_dir.path());

    println!("command {} {}", command_name, args.join(" "));
    println!("command cwd {:?}", &cwd);
    command.args(args.iter());
    if self.env_clear {
      command.env_clear();
    }
    command.envs({
      let mut envs = self.context.envs.clone();
      for (key, value) in &self.envs {
        envs.insert(key.to_string(), value.to_string());
      }
      envs
    });
    command.current_dir(cwd);
    command.stdin(Stdio::piped());

    let (combined_reader, std_out_err_handle) = if self.split_output {
      let (stdout_reader, stdout_writer) = pipe().unwrap();
      let (stderr_reader, stderr_writer) = pipe().unwrap();
      command.stdout(stdout_writer);
      command.stderr(stderr_writer);
      (
        None,
        Some((
          std::thread::spawn(move || read_pipe_to_string(stdout_reader)),
          std::thread::spawn(move || read_pipe_to_string(stderr_reader)),
        )),
      )
    } else {
      let (combined_reader, combined_writer) = pipe().unwrap();
      command.stdout(combined_writer.try_clone().unwrap());
      command.stderr(combined_writer);
      (Some(combined_reader), None)
    };

    let mut process = command.spawn().unwrap();

    if let Some(input) = &self.stdin {
      let mut p_stdin = process.stdin.take().unwrap();
      write!(p_stdin, "{input}").unwrap();
    }

    // This parent process is still holding its copies of the write ends,
    // and we have to close them before we read, otherwise the read end
    // will never report EOF. The Command object owns the writers now,
    // and dropping it closes them.
    drop(command);

    let combined = combined_reader
      .map(|pipe| sanitize_output(read_pipe_to_string(pipe), &args));

    let status = process.wait().unwrap();
    let std_out_err = std_out_err_handle.map(|(stdout, stderr)| {
      (
        sanitize_output(stdout.join().unwrap(), &args),
        sanitize_output(stderr.join().unwrap(), &args),
      )
    });
    let exit_code = status.code();
    #[cfg(unix)]
    let signal = {
      use std::os::unix::process::ExitStatusExt;
      status.signal()
    };
    #[cfg(not(unix))]
    let signal = None;

    TestCommandOutput {
      exit_code,
      signal,
      combined,
      std_out_err,
      testdata_dir: self.context.testdata_dir.clone(),
      asserted_exit_code: RefCell::new(false),
      asserted_stdout: RefCell::new(false),
      asserted_stderr: RefCell::new(false),
      asserted_combined: RefCell::new(false),
      _test_context: self.context.clone(),
    }
  }
}

pub struct TestCommandOutput {
  combined: Option<String>,
  std_out_err: Option<(String, String)>,
  exit_code: Option<i32>,
  signal: Option<i32>,
  testdata_dir: PathBuf,
  asserted_stdout: RefCell<bool>,
  asserted_stderr: RefCell<bool>,
  asserted_combined: RefCell<bool>,
  asserted_exit_code: RefCell<bool>,
  // keep alive for the duration of the output reference
  _test_context: TestContext,
}

impl Drop for TestCommandOutput {
  fn drop(&mut self) {
    fn panic_unasserted_output(text: &str) {
      println!("OUTPUT\n{text}\nOUTPUT");
      panic!(
        concat!(
          "The non-empty text of the command was not asserted at {}. ",
          "Call `output.skip_output_check()` to skip if necessary.",
        ),
        failed_position()
      );
    }

    if std::thread::panicking() {
      return;
    }
    // force the caller to assert these
    if !*self.asserted_exit_code.borrow() && self.exit_code != Some(0) {
      panic!(
        "The non-zero exit code of the command was not asserted: {:?} at {}.",
        self.exit_code,
        failed_position(),
      )
    }

    // either the combined output needs to be asserted or both stdout and stderr
    if let Some(combined) = &self.combined {
      if !*self.asserted_combined.borrow() && !combined.is_empty() {
        panic_unasserted_output(combined);
      }
    }
    if let Some((stdout, stderr)) = &self.std_out_err {
      if !*self.asserted_stdout.borrow() && !stdout.is_empty() {
        panic_unasserted_output(stdout);
      }
      if !*self.asserted_stderr.borrow() && !stderr.is_empty() {
        panic_unasserted_output(stderr);
      }
    }
  }
}

impl TestCommandOutput {
  pub fn testdata_dir(&self) -> &PathBuf {
    &self.testdata_dir
  }

  pub fn skip_output_check(&self) {
    *self.asserted_combined.borrow_mut() = true;
    *self.asserted_stdout.borrow_mut() = true;
    *self.asserted_stderr.borrow_mut() = true;
  }

  pub fn skip_exit_code_check(&self) {
    *self.asserted_exit_code.borrow_mut() = true;
  }

  pub fn exit_code(&self) -> Option<i32> {
    self.skip_exit_code_check();
    self.exit_code
  }

  pub fn signal(&self) -> Option<i32> {
    self.signal
  }

  pub fn combined_output(&self) -> &str {
    self.skip_output_check();
    self
      .combined
      .as_deref()
      .expect("not available since .split_output() was called")
  }

  pub fn stdout(&self) -> &str {
    *self.asserted_stdout.borrow_mut() = true;
    self
      .std_out_err
      .as_ref()
      .map(|(stdout, _)| stdout.as_str())
      .expect("call .split_output() on the builder")
  }

  pub fn stderr(&self) -> &str {
    *self.asserted_stderr.borrow_mut() = true;
    self
      .std_out_err
      .as_ref()
      .map(|(_, stderr)| stderr.as_str())
      .expect("call .split_output() on the builder")
  }

  pub fn assert_exit_code(&self, expected_exit_code: i32) -> &Self {
    let actual_exit_code = self.exit_code();

    if let Some(exit_code) = &actual_exit_code {
      if *exit_code != expected_exit_code {
        self.print_output();
        panic!(
          "bad exit code, expected: {:?}, actual: {:?} at {}",
          expected_exit_code,
          exit_code,
          failed_position(),
        );
      }
    } else {
      self.print_output();
      if let Some(signal) = self.signal() {
        panic!(
          "process terminated by signal, expected exit code: {:?}, actual signal: {:?} at {}",
          actual_exit_code,
          signal,
          failed_position(),
        );
      } else {
        panic!(
          "process terminated without status code on non unix platform, expected exit code: {:?} at {}",
          actual_exit_code,
          failed_position(),
        );
      }
    }

    self
  }

  pub fn print_output(&self) {
    if let Some(combined) = &self.combined {
      println!("OUTPUT\n{combined}\nOUTPUT");
    } else if let Some((stdout, stderr)) = &self.std_out_err {
      println!("STDOUT OUTPUT\n{stdout}\nSTDOUT OUTPUT");
      println!("STDERR OUTPUT\n{stderr}\nSTDERR OUTPUT");
    }
  }

  pub fn assert_matches_text(&self, expected_text: impl AsRef<str>) -> &Self {
    self.inner_assert_matches_text(self.combined_output(), expected_text)
  }

  pub fn assert_matches_file(&self, file_path: impl AsRef<Path>) -> &Self {
    self.inner_assert_matches_file(self.combined_output(), file_path)
  }

  pub fn assert_stdout_matches_text(
    &self,
    expected_text: impl AsRef<str>,
  ) -> &Self {
    self.inner_assert_matches_text(self.stdout(), expected_text)
  }

  pub fn assert_stdout_matches_file(
    &self,
    file_path: impl AsRef<Path>,
  ) -> &Self {
    self.inner_assert_matches_file(self.stdout(), file_path)
  }

  pub fn assert_stderr_matches_text(
    &self,
    expected_text: impl AsRef<str>,
  ) -> &Self {
    self.inner_assert_matches_text(self.stderr(), expected_text)
  }

  pub fn assert_stderrr_matches_file(
    &self,
    file_path: impl AsRef<Path>,
  ) -> &Self {
    self.inner_assert_matches_file(self.stderr(), file_path)
  }

  fn inner_assert_matches_text(
    &self,
    actual: &str,
    expected: impl AsRef<str>,
  ) -> &Self {
    let expected = expected.as_ref();
    if !expected.contains("[WILDCARD]") {
      assert_eq!(actual, expected, "at {}", failed_position());
    } else if !wildcard_match(expected, actual) {
      println!("OUTPUT START\n{actual}\nOUTPUT END");
      println!("EXPECTED START\n{expected}\nEXPECTED END");
      panic!("pattern match failed at {}", failed_position());
    }
    self
  }

  fn inner_assert_matches_file(
    &self,
    actual: &str,
    file_path: impl AsRef<Path>,
  ) -> &Self {
    let output_path = self.testdata_dir().join(file_path);
    println!("output path {}", output_path.display());
    let expected_text =
      std::fs::read_to_string(&output_path).unwrap_or_else(|err| {
        panic!("failed loading {}\n\n{err:#}", output_path.display())
      });
    self.inner_assert_matches_text(actual, expected_text)
  }
}

fn failed_position() -> String {
  let backtrace = Backtrace::new();

  for frame in backtrace.frames() {
    for symbol in frame.symbols() {
      if let Some(filename) = symbol.filename() {
        if !filename.to_string_lossy().ends_with("builders.rs") {
          let line_num = symbol.lineno().unwrap_or(0);
          let line_col = symbol.colno().unwrap_or(0);
          return format!("{}:{}:{}", filename.display(), line_num, line_col);
        }
      }
    }
  }

  "<unknown>".to_string()
}
