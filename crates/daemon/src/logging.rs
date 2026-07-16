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
        "debug" => Ok("trace".to_owned()),
        other => Err(anyhow!("invalid log level {other:?}; use error, info, or debug")),
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

#[derive(Clone)]
struct SharedFile(Arc<File>);

impl Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = &*self.0;
        file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = &*self.0;
        file.flush()
    }
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
            LogWriter::File(file) => Box::new(SharedFile(file.clone())),
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
        assert_eq!(s, "error,daklak=trace,viet_ime_wayland_adapter=info");
    }

    #[test]
    fn debug_maps_to_trace() {
        let s = build_directives("debug", &["daklak=debug".to_owned()]).unwrap();
        assert_eq!(s, "trace,daklak=trace");
    }

    #[test]
    fn shared_file_writers_append_without_cloning_file_handles() {
        let path = std::env::temp_dir().join(format!("daklak-shared-log-{}", std::process::id()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        let shared = SharedFile(Arc::new(file));
        let mut first = shared.clone();
        let mut second = shared;

        first.write_all(b"first\n").unwrap();
        second.write_all(b"second\n").unwrap();
        second.flush().unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_bad_levels() {
        assert!(build_directives("warn", &[]).is_err());
        assert!(build_directives("trace", &[]).is_err());
        assert!(build_directives("error", &["daklak=warn".to_owned()]).is_err());
        assert!(build_directives("error", &["daklak=trace".to_owned()]).is_err());
    }
}
