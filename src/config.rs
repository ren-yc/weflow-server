//! CLI configuration: arguments only (no config file), mirroring qqflow-server.

use std::path::PathBuf;

use anyhow::Result;

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
    /// Per-platform data directory (media export defaults here; access token lives in the OS credential store).
    pub data_dir: PathBuf,
    /// Print the stored API token (from the OS credential store) and exit.
    pub show_token: bool,
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
            show_token: false,
        }
    }
}

/// Parse the process command line (skipping the program name).
/// `Ok(None)` when `--help` / `--version` was handled and the caller should
/// exit 0.
pub fn load() -> anyhow::Result<Option<Config>> {
    parse_args(std::env::args().skip(1).collect())
}

/// Parse `--flag value` / `--flag=value` pairs (unknown flags are fatal).
///
/// Separate from [`load`] so tests can drive it with an explicit argv instead
/// of the process environment.
pub fn parse_args(args: Vec<String>) -> anyhow::Result<Option<Config>> {
    const LOG_LEVELS: [&str; 4] = ["error", "warn", "info", "debug"];
    let mut cfg = Config::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg, None),
        };

        // Value-less switches first, so `--show-token` never consumes the
        // following argument.
        match flag {
            "--show-token" => {
                cfg.show_token = true;
                i += 1;
                continue;
            }
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("weflow-server {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            _ => {}
        }

        // Everything else takes a value. A missing value is an error rather
        // than a silent fall back to the default: `--port` with nothing after
        // it used to start the server on 5033 as if nothing were wrong.
        let value = match inline {
            Some(v) => {
                if v.is_empty() {
                    anyhow::bail!("参数 {flag} 的值为空");
                }
                i += 1;
                v
            }
            None => {
                let Some(next) = args.get(i + 1) else {
                    anyhow::bail!("参数 {flag} 缺少值");
                };
                // A flag-shaped value almost always means the real value was
                // forgotten (`--host --log debug` would set host="--log").
                if next.starts_with("--") {
                    anyhow::bail!("参数 {flag} 缺少值（其后紧跟的是另一个参数 {next}）");
                }
                i += 2;
                next.clone()
            }
        };

        match flag {
            "--port" => {
                cfg.port = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--port 需为 0-65535 的整数: {value}"))?
            }
            "--host" => cfg.host = value,
            "--log" => {
                if !LOG_LEVELS.contains(&value.as_str()) {
                    anyhow::bail!("--log 需为 error|warn|info|debug: {value}");
                }
                cfg.log = value;
            }
            "--watch-debounce-ms" => {
                cfg.watch_debounce_ms = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--watch-debounce-ms 需为非负整数: {value}"))?
            }
            "--watch-fallback-ms" => {
                cfg.watch_fallback_ms = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--watch-fallback-ms 需为非负整数: {value}"))?
            }
            "--media-export-dir" => cfg.media_export_dir = PathBuf::from(value),
            "--base-url" => cfg.base_url = Some(value),
            other => anyhow::bail!("未知参数: {other}"),
        }
    }
    Ok(Some(cfg))
}

fn print_help() {
    println!(
        "weflow-server {} — 本地微信 4.x 数据库监控 + 解密/导出服务\n\
         \n\
         用法: weflow-server [选项]\n\
         \n\
         选项:\n\
         \x20   --port <PORT>              监听端口（默认 5033）\n\
         \x20   --host <HOST>              绑定地址（默认 127.0.0.1）\n\
         \x20   --log <LEVEL>              日志级别: error|warn|info|debug（默认 info）\n\
         \x20   --watch-debounce-ms <MS>   文件事件防抖（默认 350）\n\
         \x20   --watch-fallback-ms <MS>   慢速兜底轮询，0 关闭（默认 30000）\n\
         \x20   --media-export-dir <DIR>   媒体导出根目录\n\
         \x20   --base-url <URL>           媒体导出链接 base URL\n\
         \x20   --show-token               打印已存的 API token 并退出\n\
         \x20   -h, --help                 显示本帮助\n\
         \x20   -V, --version              打印版本号",
        env!("CARGO_PKG_VERSION")
    );
}

// ---- API access token (OS credential store) ----------------------------
//
// Lives here rather than in `server` so the token source sits next to the
// rest of the process configuration (qqflow-server parity).

const TOKEN_SERVICE: &str = "weflow-server";
const TOKEN_USER: &str = "http-api-token";

/// Access token kept in the OS credential store (Windows Credential
/// Manager / macOS Keychain / Linux Secret Service). Never written to a
/// token file.
///
/// The token is printed to the log **only when it is first generated**
/// (or when the credential store is unavailable and the token is
/// per-session). On subsequent launches it is fetched silently; use the
/// `--show-token` flag to retrieve it on demand.
pub fn load_token() -> Result<String> {
    let entry = keyring::Entry::new(TOKEN_SERVICE, TOKEN_USER)
        .map_err(|e| anyhow::anyhow!("凭据库初始化失败: {e}"))?;
    match entry.get_password() {
        Ok(t) if t.len() >= 16 => Ok(t),
        Ok(_) => {
            // short/corrupt value: regenerate and overwrite
            let t = new_token();
            entry
                .set_password(&t)
                .map_err(|e| anyhow::anyhow!("凭据库写入失败: {e}"))?;
            tracing::info!("[init] 生成新 API token: {t}（已存入系统凭据库）");
            Ok(t)
        }
        Err(keyring::Error::NoEntry) => {
            let t = new_token();
            entry
                .set_password(&t)
                .map_err(|e| anyhow::anyhow!("凭据库写入失败: {e}"))?;
            tracing::info!("[init] 生成新 API token: {t}（已存入系统凭据库）");
            Ok(t)
        }
        Err(e) => {
            // no credential store available: per-session token, log only
            let t = new_token();
            tracing::warn!(
                "[init] 凭据库不可用 ({e})；API token 为会话级（重启后变化）: {t}"
            );
            Ok(t)
        }
    }
}

/// Read the stored token without generating one; `None` when none exists.
/// Used by `--show-token`.
pub fn show_token() -> Result<Option<String>> {
    let entry = keyring::Entry::new(TOKEN_SERVICE, TOKEN_USER)
        .map_err(|e| anyhow::anyhow!("凭据库初始化失败: {e}"))?;
    match entry.get_password() {
        Ok(t) if !t.is_empty() => Ok(Some(t)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("凭据库读取失败: {e}")),
    }
}

fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.port, 5033);
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.log, "info");
        assert_eq!(c.watch_debounce_ms, 350);
        assert_eq!(c.watch_fallback_ms, 30_000);
        assert!(!c.show_token);
        assert_eq!(c.media_export_dir, c.data_dir.join("api-media"));
    }

    #[test]
    fn defaults_with_no_args() {
        let cfg = parse_args(vec![]).unwrap().expect("config");
        assert_eq!(cfg.port, 5033);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.log, "info");
    }

    #[test]
    fn flags_override_defaults() {
        let cfg = parse_args(args(&[
            "--port", "5999", "--host", "0.0.0.0", "--log", "debug",
            "--watch-debounce-ms", "500", "--watch-fallback-ms", "0",
            "--base-url", "http://192.168.1.10:5999",
        ]))
        .unwrap()
        .expect("config");
        assert_eq!(cfg.port, 5999);
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.log, "debug");
        assert_eq!(cfg.watch_debounce_ms, 500);
        assert_eq!(cfg.watch_fallback_ms, 0);
        assert_eq!(cfg.base_url.as_deref(), Some("http://192.168.1.10:5999"));
    }

    #[test]
    fn inline_value_syntax_supported() {
        let cfg = parse_args(args(&["--port=6001", "--log=warn"]))
            .unwrap()
            .expect("config");
        assert_eq!(cfg.port, 6001);
        assert_eq!(cfg.log, "warn");
    }

    #[test]
    fn show_token_switch_parses_without_value() {
        // The switch must not swallow the argument that follows it.
        let cfg = parse_args(args(&["--show-token", "--port", "6002"]))
            .unwrap()
            .expect("config");
        assert!(cfg.show_token, "--show-token must set the flag");
        assert_eq!(cfg.port, 6002, "--show-token must not consume --port");
        // and normal startup keeps it off
        assert!(!parse_args(vec![]).unwrap().expect("config").show_token);
    }

    #[test]
    fn help_and_version_print_and_return_none() {
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
        assert!(parse_args(args(&["-h"])).unwrap().is_none());
        assert!(parse_args(args(&["--version"])).unwrap().is_none());
        assert!(parse_args(args(&["-V"])).unwrap().is_none());
    }

    #[test]
    fn invalid_values_rejected() {
        assert!(parse_args(args(&["--port", "abc"])).is_err());
        assert!(parse_args(args(&["--watch-debounce-ms", "abc"])).is_err());

        let err = parse_args(args(&["--log", "verbose"])).unwrap_err();
        assert!(format!("{err:#}").contains("--log"), "log level is validated");

        let err = parse_args(args(&["--nope", "1"])).unwrap_err();
        assert!(format!("{err:#}").contains("未知参数"));
    }

    #[test]
    fn missing_value_is_an_error_not_a_silent_default() {
        // Trailing flag with nothing after it.
        let err = parse_args(args(&["--port"])).unwrap_err();
        assert!(format!("{err:#}").contains("缺少值"));
        // Empty inline value.
        assert!(parse_args(args(&["--host="])).is_err());
    }

    #[test]
    fn flag_shaped_value_is_rejected() {
        // `--host --log debug` must not silently set host to "--log".
        let err = parse_args(args(&["--host", "--log", "debug"])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("缺少值"), "got: {msg}");
    }
}
