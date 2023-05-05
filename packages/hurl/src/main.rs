/*
 * Hurl (https://hurl.dev)
 * Copyright (C) 2023 Orange
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *          http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 */
mod cli;

use std::collections::HashMap;
use std::env;
use std::io::prelude::*;
use std::path::Path;
use std::time::Instant;

use atty::Stream;
use clap::Command;
use colored::control;
use hurl::report::{html, junit};
use hurl::runner::HurlResult;
use hurl::util::logger::{BaseLogger, Logger, LoggerBuilder};
use hurl::{libcurl_version_info, output, runner};

const EXIT_OK: i32 = 0;
const EXIT_ERROR_COMMANDLINE: i32 = 1;
const EXIT_ERROR_PARSING: i32 = 2;
const EXIT_ERROR_RUNTIME: i32 = 3;
const EXIT_ERROR_ASSERT: i32 = 4;
const EXIT_ERROR_UNDEFINED: i32 = 127;

/// Structure that stores the result of an Hurl file execution, and the content of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HurlRun {
    /// Source string for this [`HurlFile`]
    content: String,
    /// Filename of the content
    filename: String,
    hurl_result: HurlResult,
}

/// Executes Hurl entry point.
fn main() {
    init_colored();

    let libcurl_version = libcurl_version_info();
    let version_info = format!(
        "{} {}\nFeatures (libcurl):  {}\nFeatures (built-in): brotli",
        clap::crate_version!(),
        libcurl_version.libraries.join(" "),
        libcurl_version.features.join(" "),
    );
    let mut app = cli::app(&version_info);
    let matches = app.clone().get_matches();

    // We create a basic logger that can just display info, warning or error generic messages.
    // We'll use a more advanced logger for rich error report when running Hurl files.
    let verbose = cli::has_flag(&matches, "verbose")
        || cli::has_flag(&matches, "very_verbose")
        || cli::has_flag(&matches, "interactive");
    let color = cli::output_color(&matches);
    let base_logger = BaseLogger::new(color, verbose);

    let cli_options = cli::parse_options(&matches);
    let cli_options = unwrap_or_exit(cli_options, EXIT_ERROR_UNDEFINED, &base_logger);

    // We aggregate the input files from the positional arguments and the glob
    // options. If we've no file input (either from the standard input or from
    // the command line arguments), we just print help and exit.
    let files = cli::get_strings(&matches, "FILE");
    let glob_files = &cli_options.glob_files;
    let filenames = get_input_files(&files, glob_files, &mut app, &base_logger);

    if cli_options.cookie_output_file.is_some() && filenames.len() > 1 {
        exit_with_error(
            "Only save cookies for a unique session",
            EXIT_ERROR_UNDEFINED,
            &base_logger,
        );
    }

    let progress_bar = cli_options.test && !verbose && !is_ci() && atty::is(Stream::Stderr);
    let current_dir = env::current_dir();
    let current_dir = unwrap_or_exit(current_dir, EXIT_ERROR_UNDEFINED, &base_logger);
    let current_dir = current_dir.as_path();

    let start = Instant::now();
    let mut runs = vec![];

    for (current, filename) in filenames.iter().enumerate() {
        // We check the input file existence and check that we can read its contents.
        // Once the preconditions succeed, we can parse the Hurl file, and run it.
        if filename != "-" && !Path::new(filename).exists() {
            let message = format!("hurl: cannot access '{filename}': No such file or directory");
            exit_with_error(&message, EXIT_ERROR_PARSING, &base_logger);
        }
        let content = cli::read_to_string(filename);
        let content = unwrap_or_exit(content, EXIT_ERROR_PARSING, &base_logger);

        let logger = LoggerBuilder::new()
            .filename(filename)
            .color(color)
            .verbose(verbose)
            .test(cli_options.test)
            .progress_bar(progress_bar)
            .build();

        let total = filenames.len();
        logger.test_running(current + 1, total);

        // Run our Hurl file now
        let hurl_result = execute(&content, filename, current_dir, &cli_options, &logger);
        let hurl_result = match hurl_result {
            Ok(h) => h,
            Err(_) => std::process::exit(EXIT_ERROR_PARSING),
        };
        logger.test_completed(&hurl_result);
        let success = hurl_result.success;

        // We can output the result, either the raw body or a structured JSON representation.
        let output_body = success
            && !cli_options.interactive
            && matches!(cli_options.output_type, cli::OutputType::ResponseBody);
        if output_body {
            let include_headers = cli_options.include;
            let result = output::write_body(
                &hurl_result,
                filename,
                include_headers,
                color,
                &cli_options.output,
                &logger,
            );
            unwrap_or_exit(result, EXIT_ERROR_RUNTIME, &base_logger);
        }

        if matches!(cli_options.output_type, cli::OutputType::Json) {
            let result = output::write_json(&hurl_result, &content, filename, &cli_options.output);
            unwrap_or_exit(result, EXIT_ERROR_RUNTIME, &base_logger);
        }

        let run = HurlRun {
            content,
            filename: filename.to_string(),
            hurl_result,
        };
        runs.push(run);
    }

    if let Some(filename) = cli_options.junit_file {
        base_logger.debug(format!("Writing JUnit report to {filename}").as_str());
        let result = create_junit_report(&runs, &filename);
        unwrap_or_exit(result, EXIT_ERROR_UNDEFINED, &base_logger);
    }

    if let Some(dir) = cli_options.html_dir {
        base_logger.debug(format!("Writing HTML report to {}", dir.display()).as_str());
        let result = create_html_report(&runs, &dir);
        unwrap_or_exit(result, EXIT_ERROR_UNDEFINED, &base_logger);
    }

    if let Some(filename) = cli_options.cookie_output_file {
        base_logger.debug(format!("Writing cookies to {filename}").as_str());
        let result = create_cookies_file(&runs, &filename);
        unwrap_or_exit(result, EXIT_ERROR_UNDEFINED, &base_logger);
    }

    if cli_options.test {
        let duration = start.elapsed().as_millis();
        let summary = get_summary(&runs, duration);
        base_logger.info(summary.as_str());
    }

    std::process::exit(exit_code(&runs));
}

/// Runs a Hurl `content` and returns a result.
fn execute(
    content: &str,
    filename: &str,
    current_dir: &Path,
    cli_options: &cli::CliOptions,
    logger: &Logger,
) -> Result<HurlResult, String> {
    let variables = &cli_options.variables;
    let runner_options = cli_options.to(filename, current_dir);

    runner::run(content, &runner_options, variables, state_modifier, logger)
}

#[cfg(target_family = "unix")]
fn init_colored() {
    control::set_override(true);
}

#[cfg(target_family = "windows")]
fn init_colored() {
    control::set_override(true);
    control::set_virtual_terminal(true).expect("set virtual terminal");
}

/// Unwraps a `result` or exit with message.
fn unwrap_or_exit<T, E>(result: Result<T, E>, code: i32, logger: &BaseLogger) -> T
where
    E: std::fmt::Display,
{
    match result {
        Ok(v) => v,
        Err(e) => exit_with_error(&e.to_string(), code, logger),
    }
}

/// Prints an error message and exits the current process with an exit code.
fn exit_with_error(message: &str, code: i32, logger: &BaseLogger) -> ! {
    if !message.is_empty() {
        logger.error(message);
    }
    std::process::exit(code);
}

/// Create a JUnit report for this run.
fn create_junit_report(runs: &[HurlRun], filename: &str) -> Result<(), cli::CliError> {
    let testcases: Vec<junit::Testcase> = runs
        .iter()
        .map(|r| junit::Testcase::from(&r.hurl_result, &r.content, &r.filename))
        .collect();
    junit::write_report(filename, &testcases)?;
    Ok(())
}

/// Create an HTML report for this run.
fn create_html_report(runs: &[HurlRun], dir_path: &Path) -> Result<(), cli::CliError> {
    // We ensure that the containing folder exists.
    std::fs::create_dir_all(dir_path.join("store")).unwrap();

    let mut testcases = vec![];
    for run in runs.iter() {
        let testcase = html::Testcase::from(&run.hurl_result, &run.filename);
        testcase.write_html(&run.content, dir_path)?;
        testcases.push(testcase);
    }
    html::write_report(dir_path, &testcases)?;
    Ok(())
}

/// Returns an exit code for a list of HurlResult.
fn exit_code(runs: &[HurlRun]) -> i32 {
    let mut count_errors_runner = 0;
    let mut count_errors_assert = 0;
    for run in runs.iter() {
        let errors = run.hurl_result.errors();
        if errors.is_empty() {
        } else if errors.iter().filter(|e| !e.assert).count() == 0 {
            count_errors_assert += 1;
        } else {
            count_errors_runner += 1;
        }
    }
    if count_errors_runner > 0 {
        EXIT_ERROR_RUNTIME
    } else if count_errors_assert > 0 {
        EXIT_ERROR_ASSERT
    } else {
        EXIT_OK
    }
}

/// Returns the input files from the positional arguments and the glob options.
fn get_input_files(
    files: &Option<Vec<String>>,
    glob_files: &[String],
    app: &mut Command,
    logger: &BaseLogger,
) -> Vec<String> {
    let mut filenames = vec![];
    if let Some(values) = files {
        for value in values {
            filenames.push(value.to_string());
        }
    };
    for filename in glob_files {
        filenames.push(filename.to_string());
    }
    if filenames.is_empty() {
        if atty::is(Stream::Stdin) {
            let error = if app.print_help().is_err() {
                "Panic during printing help"
            } else {
                ""
            };
            exit_with_error(error, EXIT_ERROR_COMMANDLINE, logger);
        } else {
            filenames.push("-".to_string());
        }
    }
    filenames
}

fn create_cookies_file(runs: &[HurlRun], filename: &str) -> Result<(), cli::CliError> {
    let mut file = match std::fs::File::create(filename) {
        Err(why) => {
            return Err(cli::CliError {
                message: format!("Issue writing to {filename}: {why:?}"),
            });
        }
        Ok(file) => file,
    };
    let mut s = r#"# Netscape HTTP Cookie File
# This file was generated by Hurl

"#
    .to_string();
    match runs.first() {
        None => {
            return Err(cli::CliError {
                message: "Issue fetching results".to_string(),
            });
        }
        Some(run) => {
            for cookie in run.hurl_result.cookies.iter() {
                s.push_str(&cookie.to_string());
                s.push('\n');
            }
        }
    }

    if let Err(why) = file.write_all(s.as_bytes()) {
        return Err(cli::CliError {
            message: format!("Issue writing to {filename}: {why:?}"),
        });
    }
    Ok(())
}

/// Returns the text summary of this Hurl runs.
fn get_summary(runs: &[HurlRun], duration: u128) -> String {
    let total = runs.len();
    let success = runs.iter().filter(|r| r.hurl_result.success).count();
    let success_percent = 100.0 * success as f32 / total as f32;
    let failed = total - success;
    let failed_percent = 100.0 * failed as f32 / total as f32;
    format!(
        "--------------------------------------------------------------------------------\n\
             Executed files:  {total}\n\
             Succeeded files: {success} ({success_percent:.1}%)\n\
             Failed files:    {failed} ({failed_percent:.1}%)\n\
             Duration:        {duration} ms\n"
    )
}

/// Whether or not this running in a Continuous Integration environment.
/// Code borrowed from <https://github.com/rust-lang/cargo/blob/master/crates/cargo-util/src/lib.rs>
fn is_ci() -> bool {
    env::var("CI").is_ok() || env::var("TF_BUILD").is_ok()
}

/// Function to add functions into the variables set
fn state_modifier(variables: &mut HashMap<String, runner::Value>) {
    variables.insert(
        "-uuid".to_string(),
        hurl::runner::Value::Function(|| runner::Value::String(uuid::Uuid::new_v4().to_string())),
    );
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use hurl::runner::EntryResult;

    #[test]
    fn create_run_summary() {
        fn new_run(success: bool, entries_count: usize) -> HurlRun {
            let dummy_entry = EntryResult {
                entry_index: 0,
                calls: vec![],
                captures: vec![],
                asserts: vec![],
                errors: vec![],
                time_in_ms: 0,
                compressed: false,
            };
            HurlRun {
                content: "".to_string(),
                filename: "".to_string(),
                hurl_result: HurlResult {
                    entries: vec![dummy_entry; entries_count],
                    time_in_ms: 0,
                    success,
                    cookies: vec![],
                },
            }
        }

        let runs = vec![new_run(true, 10), new_run(true, 20), new_run(true, 4)];
        let duration = 128;
        let summary = get_summary(&runs, duration);
        assert_eq!(
            summary,
            "--------------------------------------------------------------------------------\n\
             Executed files:  3\n\
             Succeeded files: 3 (100.0%)\n\
             Failed files:    0 (0.0%)\n\
             Duration:        128 ms\n"
        );

        let runs = vec![new_run(true, 10), new_run(false, 10), new_run(true, 40)];
        let duration = 200;
        let summary = get_summary(&runs, duration);
        assert_eq!(
            summary,
            "--------------------------------------------------------------------------------\n\
            Executed files:  3\n\
            Succeeded files: 2 (66.7%)\n\
            Failed files:    1 (33.3%)\n\
            Duration:        200 ms\n"
        );
    }
}
