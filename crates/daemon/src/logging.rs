use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tracing_subscriber::{filter::EnvFilter, fmt};

use crate::config::Config;

pub fn init(config: &Config) -> Result<()> {
    let filter = EnvFilter::new(build_directives(
        &config.log_level,
        &config.log_modules,
    )?);
    let writer = make_writer(&config.log_path)?;

    fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(writer)
        .init();

    Ok(())
}

fn build_directives(log_level: &str, log_modules: &[String]) -> Result<String> {
    let mut directives = vec![normalize_level(log_level)?];
    for module in log_modules {
        let directive = normalize_directive(module)?;
        if !directive.is_empty() {
            directives.push(directive);
        }
    }
    Ok(directives.join(","))
}

fn normalize_level(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Ok("error".to_owned()),
        "info" => Ok("info".to_owned()),
        "debug" | "trace" => Ok("debug".to_owned()),
        other => Err(anyhow!("invalid log level {other:?}; use error, info, debug, or trace")),
    }
}

fn normalize_directive(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }

    let (target, level) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("invalid log directive {raw:?}; use target=level"))?;

    let target = target.trim();
    if target.is_empty() {
        return Err(anyhow!("invalid log directive {raw:?}; target is empty"));
    }

    let level = normalize_level(level)?;
    Ok(format!("{target}={level}"))
}

enum LogWriter {
    Stdout,
    File(Arc<File>),
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for LogWriter {
    type Writer = Box<dyn Write + Send + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        match self {
            LogWriter::Stdout => Box::new(io::stdout()),
            LogWriter::File(file) => Box::new(
                file.try_clone()
                    .expect("failed to clone log file handle"),
            ),
        }
    }
}

fn make_writer(path: &str) -> Result<LogWriter> {
    if path.trim() == "/dev/stdout" {
        return Ok(LogWriter::Stdout);
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open log path {path:?}"))?;

    Ok(LogWriter::File(Arc::new(file)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_directives_from_default_level_and_modules() {
        let s = build_directives(
            "error",
            &["daklak=debug".to_owned(), "viet_ime_wayland_adapter=info".to_owned()],
        )
        .unwrap();
        assert_eq!(s, "error,daklak=debug,viet_ime_wayland_adapter=info");
    }

    #[test]
    fn trace_aliases_to_debug() {
        let s = build_directives("trace", &["daklak=trace".to_owned()]).unwrap();
        assert_eq!(s, "debug,daklak=debug");
    }

    #[test]
    fn rejects_bad_levels() {
        assert!(build_directives("warn", &[]).is_err());
        assert!(build_directives("error", &["daklak=warn".to_owned()]).is_err());
    }
}
