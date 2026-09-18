use clap::Parser;
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "ENGRAM_BIN", default_value = "/root/.claude")]
    bin: PathBuf,
    #[arg(long, value_parser = ["harvest", "maintenance", "curate"])]
    mode: String,
    #[arg(long, env = "ENGRAM_VECTOR_PYTHON")]
    vector_python: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let status = match args.mode.as_str() {
        "harvest" => Command::new("bash")
            .arg(args.bin.join("memory_pipeline.sh"))
            .status(),
        "maintenance" => Command::new("bash")
            .arg(args.bin.join("memory_fixate_cron.sh"))
            .status(),
        "curate" => {
            let python = args
                .vector_python
                .unwrap_or_else(|| args.bin.join("vector/venv/bin/python"));
            let interpreter = if python.is_file() {
                python
            } else {
                PathBuf::from("python3")
            };
            Command::new(interpreter)
                .arg(args.bin.join("memory_auto_curate.py"))
                .arg("--apply")
                .status()
        }
        _ => unreachable!(),
    };
    match status {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1).clamp(1, 255) as u8),
        Err(error) => {
            eprintln!("engram-lifecycle: {error}");
            ExitCode::FAILURE
        }
    }
}
