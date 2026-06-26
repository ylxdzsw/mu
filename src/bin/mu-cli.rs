use std::io::{self, Write};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;

#[cfg(not(unix))]
compile_error!("mu-cli is supported only on Unix-like systems");

#[derive(Parser, Debug)]
#[command(name = "mu-cli", about = "Interactive REPL wrapper around mu")]
struct Args {
    #[arg(short = 's', long)]
    session: Option<String>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    effort: Option<String>,

    #[arg(long, default_value = "terminal")]
    output: String,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let mut session_id = args.session;
    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        print!("mu> ");
        io::stdout().flush()?;
        line.clear();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }

        let prompt = line.trim_end_matches(['\r', '\n']);
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "exit" | "quit" | ":q") {
            break;
        }

        let session_file = std::env::temp_dir().join(format!("mu-cli-{}.session", process::id()));
        let mut command = process::Command::new("mu");
        if let Some(id) = &session_id {
            command.arg("-s").arg(id);
        } else {
            command.env("MU_SESSION_FILE", &session_file);
        }
        if let Some(model) = &args.model {
            command.arg("--model").arg(model);
        }
        if let Some(effort) = &args.effort {
            command.arg("--effort").arg(effort);
        }
        command.arg("--output").arg(&args.output);
        command.stdin(process::Stdio::piped());

        let mut child = command.spawn().context("spawning mu turn")?;
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdin unavailable"))?
            .write_all(prompt.as_bytes())?;
        let status = child.wait().context("waiting for mu turn")?;
        if !status.success() {
            eprintln!("mu exited with {status}");
        }
        if session_id.is_none() && session_file.exists() {
            session_id = Some(std::fs::read_to_string(&session_file)?.trim().to_string());
        }
    }

    Ok(())
}
