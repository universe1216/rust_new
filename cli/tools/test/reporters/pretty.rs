// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use super::*;

pub struct PrettyTestReporter {
  parallel: bool,
  echo_output: bool,
  in_new_line: bool,
  scope_test_id: Option<usize>,
  cwd: Url,
  did_have_user_output: bool,
  started_tests: bool,
  child_results_buffer:
    HashMap<usize, IndexMap<usize, (TestStepDescription, TestStepResult, u64)>>,
  summary: TestSummary,
}

impl PrettyTestReporter {
  pub fn new(parallel: bool, echo_output: bool) -> PrettyTestReporter {
    PrettyTestReporter {
      parallel,
      echo_output,
      in_new_line: true,
      scope_test_id: None,
      cwd: Url::from_directory_path(std::env::current_dir().unwrap()).unwrap(),
      did_have_user_output: false,
      started_tests: false,
      child_results_buffer: Default::default(),
      summary: TestSummary::new(),
    }
  }

  fn force_report_wait(&mut self, description: &TestDescription) {
    if !self.in_new_line {
      println!();
    }
    if self.parallel {
      print!(
        "{}",
        colors::gray(format!(
          "{} => ",
          self.to_relative_path_or_remote_url(&description.origin)
        ))
      );
    }
    print!("{} ...", description.name);
    self.in_new_line = false;
    // flush for faster feedback when line buffered
    std::io::stdout().flush().unwrap();
    self.scope_test_id = Some(description.id);
  }

  fn to_relative_path_or_remote_url(&self, path_or_url: &str) -> String {
    let url = Url::parse(path_or_url).unwrap();
    if url.scheme() == "file" {
      if let Some(mut r) = self.cwd.make_relative(&url) {
        if !r.starts_with("../") {
          r = format!("./{r}");
        }
        return r;
      }
    }
    path_or_url.to_string()
  }

  fn force_report_step_wait(&mut self, description: &TestStepDescription) {
    self.write_output_end();
    if !self.in_new_line {
      println!();
    }
    print!("{}{} ...", "  ".repeat(description.level), description.name);
    self.in_new_line = false;
    // flush for faster feedback when line buffered
    std::io::stdout().flush().unwrap();
    self.scope_test_id = Some(description.id);
  }

  fn force_report_step_result(
    &mut self,
    description: &TestStepDescription,
    result: &TestStepResult,
    elapsed: u64,
  ) {
    self.write_output_end();
    if self.in_new_line || self.scope_test_id != Some(description.id) {
      self.force_report_step_wait(description);
    }

    if !self.parallel {
      let child_results = self
        .child_results_buffer
        .remove(&description.id)
        .unwrap_or_default();
      for (desc, result, elapsed) in child_results.values() {
        self.force_report_step_result(desc, result, *elapsed);
      }
      if !child_results.is_empty() {
        self.force_report_step_wait(description);
      }
    }

    let status = match &result {
      TestStepResult::Ok => colors::green("ok").to_string(),
      TestStepResult::Ignored => colors::yellow("ignored").to_string(),
      TestStepResult::Failed(failure) => failure.format_label(),
    };
    print!(" {}", status);
    if let TestStepResult::Failed(failure) = result {
      if let Some(inline_summary) = failure.format_inline_summary() {
        print!(" ({})", inline_summary)
      }
    }
    if !matches!(result, TestStepResult::Failed(TestFailure::Incomplete)) {
      print!(
        " {}",
        colors::gray(format!("({})", display::human_elapsed(elapsed.into())))
      );
    }
    println!();
    self.in_new_line = true;
    if self.parallel {
      self.scope_test_id = None;
    } else {
      self.scope_test_id = Some(description.parent_id);
    }
    self
      .child_results_buffer
      .entry(description.parent_id)
      .or_default()
      .remove(&description.id);
  }

  fn write_output_end(&mut self) {
    if self.did_have_user_output {
      println!("{}", colors::gray("----- output end -----"));
      self.in_new_line = true;
      self.did_have_user_output = false;
    }
  }

  fn format_test_step_ancestry(
    &self,
    desc: &TestStepDescription,
    tests: &IndexMap<usize, TestDescription>,
    test_steps: &IndexMap<usize, TestStepDescription>,
  ) -> String {
    let root;
    let mut ancestor_names = vec![];
    let mut current_desc = desc;
    loop {
      if let Some(step_desc) = test_steps.get(&current_desc.parent_id) {
        ancestor_names.push(&step_desc.name);
        current_desc = step_desc;
      } else {
        root = tests.get(&current_desc.parent_id).unwrap();
        break;
      }
    }
    ancestor_names.reverse();
    let mut result = String::new();
    result.push_str(&root.name);
    result.push_str(" ... ");
    for name in ancestor_names {
      result.push_str(name);
      result.push_str(" ... ");
    }
    result.push_str(&desc.name);
    result
  }

  fn format_test_for_summary(&self, desc: &TestDescription) -> String {
    format!(
      "{} {}",
      &desc.name,
      colors::gray(format!(
        "=> {}:{}:{}",
        self.to_relative_path_or_remote_url(&desc.location.file_name),
        desc.location.line_number,
        desc.location.column_number
      ))
    )
  }

  fn format_test_step_for_summary(
    &self,
    desc: &TestStepDescription,
    tests: &IndexMap<usize, TestDescription>,
    test_steps: &IndexMap<usize, TestStepDescription>,
  ) -> String {
    let long_name = self.format_test_step_ancestry(desc, tests, test_steps);
    format!(
      "{} {}",
      long_name,
      colors::gray(format!(
        "=> {}:{}:{}",
        self.to_relative_path_or_remote_url(&desc.location.file_name),
        desc.location.line_number,
        desc.location.column_number
      ))
    )
  }
}

impl TestReporter for PrettyTestReporter {
  fn report_register(&mut self, _description: &TestDescription) {}
  fn report_plan(&mut self, plan: &TestPlan) {
    self.summary.total += plan.total;
    self.summary.filtered_out += plan.filtered_out;
    if self.parallel {
      return;
    }
    let inflection = if plan.total == 1 { "test" } else { "tests" };
    println!(
      "{}",
      colors::gray(format!(
        "running {} {} from {}",
        plan.total,
        inflection,
        self.to_relative_path_or_remote_url(&plan.origin)
      ))
    );
    self.in_new_line = true;
  }

  fn report_wait(&mut self, description: &TestDescription) {
    if !self.parallel {
      self.force_report_wait(description);
    }
    self.started_tests = true;
  }

  fn report_output(&mut self, output: &[u8]) {
    if !self.echo_output {
      return;
    }

    if !self.did_have_user_output && self.started_tests {
      self.did_have_user_output = true;
      if !self.in_new_line {
        println!();
      }
      println!("{}", colors::gray("------- output -------"));
      self.in_new_line = true;
    }

    // output everything to stdout in order to prevent
    // stdout and stderr racing
    std::io::stdout().write_all(output).unwrap();
  }

  fn report_result(
    &mut self,
    description: &TestDescription,
    result: &TestResult,
    elapsed: u64,
  ) {
    match &result {
      TestResult::Ok => {
        self.summary.passed += 1;
      }
      TestResult::Ignored => {
        self.summary.ignored += 1;
      }
      TestResult::Failed(failure) => {
        self.summary.failed += 1;
        self
          .summary
          .failures
          .push((description.clone(), failure.clone()));
      }
      TestResult::Cancelled => {
        self.summary.failed += 1;
      }
    }

    if self.parallel {
      self.force_report_wait(description);
    }

    self.write_output_end();
    if self.in_new_line || self.scope_test_id != Some(description.id) {
      self.force_report_wait(description);
    }

    let status = match result {
      TestResult::Ok => colors::green("ok").to_string(),
      TestResult::Ignored => colors::yellow("ignored").to_string(),
      TestResult::Failed(failure) => failure.format_label(),
      TestResult::Cancelled => colors::gray("cancelled").to_string(),
    };
    print!(" {}", status);
    if let TestResult::Failed(failure) = result {
      if let Some(inline_summary) = failure.format_inline_summary() {
        print!(" ({})", inline_summary)
      }
    }
    println!(
      " {}",
      colors::gray(format!("({})", display::human_elapsed(elapsed.into())))
    );
    self.in_new_line = true;
    self.scope_test_id = None;
  }

  fn report_uncaught_error(&mut self, origin: &str, error: Box<JsError>) {
    self.summary.failed += 1;
    self
      .summary
      .uncaught_errors
      .push((origin.to_string(), error));

    if !self.in_new_line {
      println!();
    }
    println!(
      "Uncaught error from {} {}",
      self.to_relative_path_or_remote_url(origin),
      colors::red("FAILED")
    );
    self.in_new_line = true;
    self.did_have_user_output = false;
  }

  fn report_step_register(&mut self, _description: &TestStepDescription) {}

  fn report_step_wait(&mut self, description: &TestStepDescription) {
    if !self.parallel && self.scope_test_id == Some(description.parent_id) {
      self.force_report_step_wait(description);
    }
  }

  fn report_step_result(
    &mut self,
    desc: &TestStepDescription,
    result: &TestStepResult,
    elapsed: u64,
    tests: &IndexMap<usize, TestDescription>,
    test_steps: &IndexMap<usize, TestStepDescription>,
  ) {
    match &result {
      TestStepResult::Ok => {
        self.summary.passed_steps += 1;
      }
      TestStepResult::Ignored => {
        self.summary.ignored_steps += 1;
      }
      TestStepResult::Failed(failure) => {
        self.summary.failed_steps += 1;
        self.summary.failures.push((
          TestDescription {
            id: desc.id,
            name: self.format_test_step_ancestry(desc, tests, test_steps),
            ignore: false,
            only: false,
            origin: desc.origin.clone(),
            location: desc.location.clone(),
          },
          failure.clone(),
        ))
      }
    }

    if self.parallel {
      self.write_output_end();
      print!(
        "{} {} ...",
        colors::gray(format!(
          "{} =>",
          self.to_relative_path_or_remote_url(&desc.origin)
        )),
        self.format_test_step_ancestry(desc, tests, test_steps)
      );
      self.in_new_line = false;
      self.scope_test_id = Some(desc.id);
      self.force_report_step_result(desc, result, elapsed);
    } else {
      let sibling_results =
        self.child_results_buffer.entry(desc.parent_id).or_default();
      if self.scope_test_id == Some(desc.id)
        || self.scope_test_id == Some(desc.parent_id)
      {
        let sibling_results = std::mem::take(sibling_results);
        self.force_report_step_result(desc, result, elapsed);
        // Flush buffered sibling results.
        for (desc, result, elapsed) in sibling_results.values() {
          self.force_report_step_result(desc, result, *elapsed);
        }
      } else {
        sibling_results
          .insert(desc.id, (desc.clone(), result.clone(), elapsed));
      }
    }
  }

  fn report_summary(
    &mut self,
    elapsed: &Duration,
    _tests: &IndexMap<usize, TestDescription>,
    _test_steps: &IndexMap<usize, TestStepDescription>,
  ) {
    if !self.summary.failures.is_empty()
      || !self.summary.uncaught_errors.is_empty()
    {
      #[allow(clippy::type_complexity)] // Type alias doesn't look better here
      let mut failures_by_origin: BTreeMap<
        String,
        (Vec<(&TestDescription, &TestFailure)>, Option<&JsError>),
      > = BTreeMap::default();
      let mut failure_titles = vec![];
      for (description, failure) in &self.summary.failures {
        let (failures, _) = failures_by_origin
          .entry(description.origin.clone())
          .or_default();
        failures.push((description, failure));
      }

      for (origin, js_error) in &self.summary.uncaught_errors {
        let (_, uncaught_error) =
          failures_by_origin.entry(origin.clone()).or_default();
        let _ = uncaught_error.insert(js_error.as_ref());
      }
      // note: the trailing whitespace is intentional to get a red background
      println!("\n{}\n", colors::white_bold_on_red(" ERRORS "));
      for (origin, (failures, uncaught_error)) in failures_by_origin {
        for (description, failure) in failures {
          if !failure.hide_in_summary() {
            let failure_title = self.format_test_for_summary(description);
            println!("{}", &failure_title);
            println!("{}: {}", colors::red_bold("error"), failure.to_string());
            println!();
            failure_titles.push(failure_title);
          }
        }
        if let Some(js_error) = uncaught_error {
          let failure_title = format!(
            "{} (uncaught error)",
            self.to_relative_path_or_remote_url(&origin)
          );
          println!("{}", &failure_title);
          println!(
            "{}: {}",
            colors::red_bold("error"),
            format_test_error(js_error)
          );
          println!("This error was not caught from a test and caused the test runner to fail on the referenced module.");
          println!("It most likely originated from a dangling promise, event/timeout handler or top-level code.");
          println!();
          failure_titles.push(failure_title);
        }
      }
      // note: the trailing whitespace is intentional to get a red background
      println!("{}\n", colors::white_bold_on_red(" FAILURES "));
      for failure_title in failure_titles {
        println!("{failure_title}");
      }
    }

    let status = if self.summary.has_failed() {
      colors::red("FAILED").to_string()
    } else {
      colors::green("ok").to_string()
    };

    let get_steps_text = |count: usize| -> String {
      if count == 0 {
        String::new()
      } else if count == 1 {
        " (1 step)".to_string()
      } else {
        format!(" ({count} steps)")
      }
    };

    let mut summary_result = String::new();

    write!(
      summary_result,
      "{} passed{} | {} failed{}",
      self.summary.passed,
      get_steps_text(self.summary.passed_steps),
      self.summary.failed,
      get_steps_text(self.summary.failed_steps),
    )
    .unwrap();

    let ignored_steps = get_steps_text(self.summary.ignored_steps);
    if self.summary.ignored > 0 || !ignored_steps.is_empty() {
      write!(
        summary_result,
        " | {} ignored{}",
        self.summary.ignored, ignored_steps
      )
      .unwrap()
    }

    if self.summary.measured > 0 {
      write!(summary_result, " | {} measured", self.summary.measured,).unwrap();
    }

    if self.summary.filtered_out > 0 {
      write!(
        summary_result,
        " | {} filtered out",
        self.summary.filtered_out
      )
      .unwrap()
    };

    println!(
      "\n{} | {} {}\n",
      status,
      summary_result,
      colors::gray(format!(
        "({})",
        display::human_elapsed(elapsed.as_millis())
      )),
    );
    self.in_new_line = true;
  }

  fn report_sigint(
    &mut self,
    tests_pending: &HashSet<usize>,
    tests: &IndexMap<usize, TestDescription>,
    test_steps: &IndexMap<usize, TestStepDescription>,
  ) {
    if tests_pending.is_empty() {
      return;
    }
    let mut formatted_pending = BTreeSet::new();
    for id in tests_pending {
      if let Some(desc) = tests.get(id) {
        formatted_pending.insert(self.format_test_for_summary(desc));
      }
      if let Some(desc) = test_steps.get(id) {
        formatted_pending
          .insert(self.format_test_step_for_summary(desc, tests, test_steps));
      }
    }
    println!(
      "\n{} The following tests were pending:\n",
      colors::intense_blue("SIGINT")
    );
    for entry in formatted_pending {
      println!("{}", entry);
    }
    println!();
    self.in_new_line = true;
  }

  fn flush_report(
    &mut self,
    _elapsed: &Duration,
    _tests: &IndexMap<usize, TestDescription>,
    _test_steps: &IndexMap<usize, TestStepDescription>,
  ) -> anyhow::Result<()> {
    Ok(())
  }
}
