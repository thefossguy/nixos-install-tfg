use std::io;
use std::process::{Command, Output};

const NIX3_COMMAND_ARGS: [&str; 4] = [
    "--extra-experimental-features",
    "nix-command",
    "--extra-experimental-features",
    "flakes",
];

const NIX_INSTANTIATE_COMMAND_ARGS: [&str; 3] =
    ["--option", "extra-experimental-features", "flakes"];

pub fn did_command_exit_successfully(process_result: &Result<Output, io::Error>) -> bool {
    match process_result {
        Err(_) => false,
        Ok(process_result) => match process_result.status.code() {
            None => false,
            Some(process_exit_code) => process_exit_code == 0,
        },
    }
}

pub fn create_nix3_command() -> Command {
    let mut nix3_command = Command::new("nix");
    nix3_command.args(NIX3_COMMAND_ARGS);
    nix3_command
}

pub fn create_nix_instantiate_command() -> Command {
    let mut nix_instantiate_command = Command::new("nix-instantiate");
    nix_instantiate_command.args(NIX_INSTANTIATE_COMMAND_ARGS);
    nix_instantiate_command
}

pub fn get_command_argv(command: &Command) -> Vec<String> {
    let mut argv: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect();
    argv.insert(0, command.get_program().to_string_lossy().to_string());
    argv
}

fn stderr_indicates_ebusy(stderr: &[u8]) -> bool {
    let stderr_lowercase = String::from_utf8_lossy(stderr).to_lowercase();
    stderr_lowercase.contains("ebusy")
        || stderr_lowercase.contains("device or resource busy")
        || stderr_lowercase.contains("mount point is busy")
        || stderr_lowercase.contains("resource temporarily unavailable")
        || stderr_lowercase.contains("target is busy")
}

pub fn retry_command_on_ebusy(
    max_attempts: u8,
    mut run: impl FnMut() -> Result<Output, io::Error>,
) -> Result<Output, io::Error> {
    for attempt in 0..max_attempts {
        let process_result = run();
        let is_ebusy = match process_result {
            Ok(ref process_output) => stderr_indicates_ebusy(&process_output.stderr),
            Err(_) => false,
        };
        if is_ebusy {
            nanologger::warn!(
                "Encountered EBUSY while running the command, retrying; attempt {} of {}",
                attempt + 1,
                max_attempts
            );
            if attempt + 1 < max_attempts {
                std::thread::sleep(std::time::Duration::from_secs(1));
            } else {
                return process_result;
            }
        } else {
            return process_result;
        }
    }
    Err(io::Error::other("No attempts were made"))
}

#[macro_export]
macro_rules! log_then_output {
    ($command:expr) => {{
        nanologger::trace!(
            "Running: {:?}",
            $crate::command_helpers::get_command_argv(&$command)
        );
        $command.output()
    }};
}

#[macro_export]
macro_rules! log_then_status {
    ($command:expr) => {{
        nanologger::trace!(
            "Running: {:?}",
            $crate::command_helpers::get_command_argv(&$command)
        );
        $command.status()
    }};
}

#[macro_export]
macro_rules! get_process_stdout {
    ($process_output:expr) => {
        String::from_utf8_lossy(&$process_output.stdout)
            .trim()
            .to_string()
    };
}

#[macro_export]
macro_rules! get_process_stderr {
    ($process_output:expr) => {
        String::from_utf8_lossy(&$process_output.stderr)
            .trim()
            .to_string()
    };
}

#[macro_export]
macro_rules! get_formatted_process_stderr {
    ($process_output:expr) => {
        $crate::make_formatted_error!($crate::get_process_stderr!($process_output))
    };
}

#[macro_export]
macro_rules! make_formatted_error {
    ($passed_error:expr) => {
        format!("\n```\n{}\n```", $passed_error)
    };
}
