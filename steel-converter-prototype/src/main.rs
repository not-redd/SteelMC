//! `steel-converter --input <vanilla save> --output <steel save_path>`

use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use steel_converter_prototype::{ConvertOptions, convert};
use tokio::runtime;

const USAGE: &str =
    "usage: steel-converter --input <vanilla save folder> --output <steel save_path>

Converts a vanilla Minecraft 26.3 save into a new Steel save path. Stop vanilla first.
The output must not already hold a Steel save. Afterwards, point Steel's
config/worlds.toml at the output; the suggested file is printed at the end.";

fn main() -> ExitCode {
    let options = match parse_args(env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(io::stderr(), "{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(io::stderr(), "failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(convert(&options)) {
        Ok(report) => {
            let save_path = options.output.display().to_string();
            let _ = writeln!(
                io::stdout(),
                "{report}\nSuggested config/worlds.toml:\n\n{}",
                report.worlds_toml(&save_path)
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "conversion failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<ConvertOptions, String> {
    let mut input = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        let slot = match arg.as_str() {
            "--input" | "-i" => &mut input,
            "--output" | "-o" => &mut output,
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        };
        *slot = Some(PathBuf::from(
            args.next().ok_or_else(|| format!("{arg} needs a path"))?,
        ));
    }
    match (input, output) {
        (Some(input), Some(output)) => Ok(ConvertOptions::new(input, output)),
        _ => Err("both --input and --output are required".to_owned()),
    }
}
