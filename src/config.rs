//! CLI configuration: arguments only (no config file), mirroring qqflow-server.

use std::path::PathBuf;

/// Runtime configuration resolved from CLI arguments.
#[derive(Debug, Clone)]
pub struct Config {
    /// HTTP listen port. Default 5033 (WeFlow uses 5031, qqflow-server 5032).
    pub port: u16,
    /// Bind host. Default 127.0.0.1 (loopback only).
    pub host: String,
    /// Log level: error | warn | info | debug.
    pub log: String,
    /// Watch event debounce in milliseconds.
    pub watch_debounce_ms: u64,
    /// Slow fallback poll interval in milliseconds (0 disables it).
    pub watch_fallback_ms: u64,
    /// Media export root (`<data-dir>/api-media` by default).
    pub media_export_dir: PathBuf,
    /// Heuristic base URL for media links; falls back to `http://<host>:<port>`.
    pub base_url: Option<String>,
    /// Per-platform data directory (token.txt lives here).
    pub data_dir: PathBuf,
}

/// Resolve the per-platform application data directory.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("weflow-server")
    }
    #[cfg(target_os = "macos")]
    {
        dirs::data_dir()
            .unwrap_or_else(|| std::env::temp_dir())
            .join("weflow-server")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs::data_dir()
            .unwrap_or_else(|| std::env::temp_dir())
            .join("weflow-server")
    }
}

impl Default for Config {
    fn default() -> Self {
        let dir = data_dir();
        Config {
            port: 5033,
            host: "127.0.0.1".to_string(),
            log: "info".to_string(),
            watch_debounce_ms: 350,
            watch_fallback_ms: 30_000,
            media_export_dir: dir.join("api-media"),
            base_url: None,
            data_dir: dir,
        }
    }
}

/// Parse CLI arguments (unknown flags are fatal).
pub fn parse_args() -> anyhow::Result<Option<Config>> {
    let mut cfg = Config::default();
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        let mut flag = arg.as_str();
        let mut inline: Option<&str> = None;
        if let Some((f, v)) = flag.split_once('=') {
            flag = f;
            inline = Some(v);
        }
        let mut value = |default: &str| -> String {
            if let Some(v) = inline {
                return v.to_string();
            }
            if it.peek().is_some() {
                it.next().unwrap()
            } else {
                default.to_string()
            }
        };
        match flag {
            "--port" => cfg.port = value("5033").parse()?,
            "--host" => cfg.host = value("127.0.0.1"),
            "--log" => cfg.log = value("info"),
            "--watch-debounce-ms" => cfg.watch_debounce_ms = value("350").parse()?,
            "--watch-fallback-ms" => cfg.watch_fallback_ms = value("30000").parse()?,
            "--media-export-dir" => cfg.media_export_dir = PathBuf::from(value("")),
            "--base-url" => cfg.base_url = Some(value("")),
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("weflow-server {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    if cfg.media_export_dir.as_os_str().is_empty() {
        cfg.media_export_dir = cfg.data_dir.join("api-media");
    }
    Ok(Some(cfg))
}

fn print_help() {
    println!(
        "weflow-server {} - headless WeChat 4.x database monitor + decrypt/extract service\n\
         \n\
         USAGE:\n    weflow-server [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20   --port <PORT>              Listen port (default 5033)\n\
         \x20   --host <HOST>              Bind host (default 127.0.0.1)\n\
         \x20   --log <LEVEL>              error|warn|info|debug (default info)\n\
         \x20   --watch-debounce-ms <MS>   Watch event debounce (default 350)\n\
         \x20   --watch-fallback-ms <MS>   Slow fallback poll (default 30000, 0 disables)\n\
         \x20   --media-export-dir <DIR>   Media export root\n\
         \x20   --base-url <URL>           Base URL for media links\n\
         \x20   -h, --help                 Print this help\n\
         \x20   -V, --version              Print version",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.port, 5033);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.watch_debounce_ms, 350);
    }
}