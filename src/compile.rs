//! Module that handles calling out to `cargo bench` and parsing the machine-readable messages
//! to compile the benchmarks and collect the information on the benchmark executables that it
//! emits.

use crate::bench_target::BenchTarget;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

#[derive(Debug)]
/// Enum representing the different ways calling Cargo might fail
pub enum CompileError {
    CompileFailed(ExitStatus),
}
impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::CompileFailed(exit_status) => write!(
                f,
                "'cargo bench' returned an error ({}); unable to continue.",
                exit_status
            ),
        }
    }
}
impl std::error::Error for CompileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CompileError::CompileFailed(_) => None,
        }
    }
}

// These structs match the parts of Cargo's message format that we care about.
#[derive(Serialize, Deserialize, Debug)]
struct Target {
    name: String,
    kind: Vec<String>,
}

/// Enum listing out the different types of messages that Cargo can send. We only care about the
/// compiler-artifact message.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "reason")]
#[allow(clippy::enum_variant_names)]
enum Message {
    #[serde(rename = "compiler-artifact")]
    CompilerArtifact {
        target: Target,
        executable: Option<PathBuf>,
    },

    // TODO: Delete these and replace with a #[serde(other)] variant
    // See https://github.com/serde-rs/serde/issues/912
    #[serde(rename = "compiler-message")]
    CompilerMessage {},

    #[serde(rename = "build-script-executed")]
    BuildScriptExecuted {},

    #[serde(rename = "build-finished")]
    BuildFinished {},
}

/// Launches `cargo bench` with the given additional arguments, with some additional arguments to
/// list out the benchmarks and their executables and parses that information. This compiles the
/// benchmarks but doesn't run them. Returns information on the compiled benchmarks that we can use
/// to run them directly.
pub fn compile(cargo_args: &[std::ffi::OsString]) -> Result<Vec<BenchTarget>> {
    let mut cargo = Command::new("cargo")
        .arg("bench")
        .args(cargo_args)
        .args(&["--no-run", "--message-format", "json"])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit()) // Cargo writes its normal compile output to stderr
        .stdout(Stdio::piped()) // Capture the JSON messages on stdout
        .spawn()?;

    // Build a message stream reading from the child process
    let cargo_stdout = cargo
        .stdout
        .take()
        .expect("Child process doesn't have a stdout handle");
    let stream = serde_json::Deserializer::from_reader(cargo_stdout).into_iter::<Message>();

    // Collect the benchmark artifacts from the message stream
    let mut benchmarks = vec![];
    for message in stream {
        let message = message.context("Failed to parse message from cargo")?;

        if let Message::CompilerArtifact { target, executable } = message {
            if target
                .kind
                .iter()
                // Benchmarks and tests have executables. Libraries might, if they expose tests.
                .any(|kind| kind == "bench" || kind == "test" || kind == "lib")
            {
                if let Some(executable) = executable {
                    benchmarks.push(BenchTarget {
                        name: target.name,
                        executable,
                    });
                }
            }
        }
    }

    let exit_status = cargo
        .wait()
        .context("Cargo compilation failed in an unexpected way")?;
    if !(exit_status.success()) {
        // If the compile failed, the user will probably want to see the error messages.
        // message-format json means that the compiler will send them to us instead of the
        // terminal, and I don't want to have to figure out how to display those messages,
        // so instead just try again without --message-format.
        error!("Compile failed; running compile again to show error messages");

        Command::new("cargo")
            .arg("bench")
            .args(cargo_args)
            .args(&["--no-run"])
            .stdin(Stdio::inherit())
            .stderr(Stdio::inherit()) // Cargo writes its normal compile output to stderr
            .stdout(Stdio::inherit()) // Capture the JSON messages on stdout
            .spawn()?
            .wait()?;

        Err(CompileError::CompileFailed(exit_status).into())
    } else {
        Ok(benchmarks)
    }
}
