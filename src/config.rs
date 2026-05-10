use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    pub servers: HashMap<String, ServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub socket: Option<String>,
    pub cmd: Option<String>,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub max_active_clients: Option<usize>,
    pub tray: Option<bool>,
    pub service_name: Option<String>,
    pub log_level: Option<String>,
    pub lazy_start: Option<bool>,
    pub max_request_bytes: Option<usize>,
    pub request_timeout_ms: Option<u64>,
    pub restart_backoff_ms: Option<u64>,
    pub restart_backoff_max_ms: Option<u64>,
    pub max_restarts: Option<u64>,
    pub status_file: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub heartbeat_interval_ms: Option<u64>,
    pub heartbeat_timeout_ms: Option<u64>,
    pub heartbeat_max_failures: Option<u32>,
    pub heartbeat_enabled: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ResolvedParams {
    pub socket: PathBuf,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub max_clients: usize,
    pub tray_enabled: bool,
    pub log_level: String,
    pub service_name: String,
    pub lazy_start: bool,
    pub max_request_bytes: usize,
    pub request_timeout: std::time::Duration,
    pub restart_backoff: std::time::Duration,
    pub restart_backoff_max: std::time::Duration,
    pub max_restarts: u64,
    pub status_file: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub heartbeat_max_failures: u32,
    pub heartbeat_enabled: bool,
}

pub fn expand_path(raw: impl AsRef<str>) -> PathBuf {
    let s = raw.as_ref();
    if let Some(stripped) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(s)
}

fn reject_parent_components(path: &Path) -> Result<()> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(anyhow!(
            "refusing path with parent traversal component: {}",
            path.display()
        ));
    }
    Ok(())
}

pub fn vetted_existing_file(path: &Path) -> Result<PathBuf> {
    reject_parent_components(path)?;
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    if !canonical.is_file() {
        return Err(anyhow!("not a regular file: {}", canonical.display()));
    }
    Ok(canonical)
}

fn vetted_output_file(path: &Path) -> Result<PathBuf> {
    reject_parent_components(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("output path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let safe_parent = fs::canonicalize(parent)
        .with_context(|| format!("failed to canonicalize {}", parent.display()))?;
    if !safe_parent.is_dir() {
        return Err(anyhow!(
            "output parent is not a directory: {}",
            safe_parent.display()
        ));
    }
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("output path has no file name: {}", path.display()))?;
    Ok(safe_parent.join(name))
}

pub fn safe_read_to_string(path: &Path) -> Result<String> {
    let safe_path = vetted_existing_file(path)?;
    let mut data = String::new();
    // `safe_path` is canonicalized, must be a regular file, and rejects `..`.
    // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
    let mut input = File::open(&safe_path)
        .with_context(|| format!("failed to open {}", safe_path.display()))?;
    input
        .read_to_string(&mut data)
        .with_context(|| format!("failed to read {}", safe_path.display()))?;
    Ok(data)
}

pub fn safe_copy_file(src: &Path, dst: &Path) -> Result<()> {
    let safe_src = vetted_existing_file(src)?;
    let safe_dst = vetted_output_file(dst)?;
    // Both paths have passed the canonical file/output boundary above.
    let mut input = File::open(&safe_src) // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
        .with_context(|| format!("failed to open {}", safe_src.display()))?;
    // `safe_dst` is anchored under a canonicalized directory and rejects `..`.
    // nosemgrep: rust.actix.path-traversal.tainted-path.tainted-path
    let mut output = File::create(&safe_dst)
        .with_context(|| format!("failed to create {}", safe_dst.display()))?;
    io::copy(&mut input, &mut output).with_context(|| {
        format!(
            "failed to copy {} to {}",
            safe_src.display(),
            safe_dst.display()
        )
    })?;
    Ok(())
}

pub fn load_config(path: &Path) -> Result<Option<Config>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = safe_read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let cfg: Config = match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_str(&data)
            .with_context(|| format!("failed to parse yaml config {}", path.display()))?,
        "toml" => toml::from_str(&data)
            .with_context(|| format!("failed to parse toml config {}", path.display()))?,
        _ => serde_json::from_str(&data)
            .with_context(|| format!("failed to parse json config {}", path.display()))?,
    };
    Ok(Some(cfg))
}

pub fn safe_copy(src: &Path, dst: &Path) -> Result<()> {
    safe_copy_file(src, dst)
}

pub trait CliOptions {
    fn socket(&self) -> Option<PathBuf>;
    fn cmd(&self) -> Option<String>;
    fn args(&self) -> Vec<String>;
    fn max_active_clients(&self) -> usize;
    fn lazy_start(&self) -> Option<bool>;
    fn max_request_bytes(&self) -> Option<usize>;
    fn request_timeout_ms(&self) -> Option<u64>;
    fn restart_backoff_ms(&self) -> Option<u64>;
    fn restart_backoff_max_ms(&self) -> Option<u64>;
    fn max_restarts(&self) -> Option<u64>;
    fn log_level(&self) -> String;
    fn tray(&self) -> bool;
    fn service_name(&self) -> Option<String>;
    fn service(&self) -> Option<String>;
    fn status_file(&self) -> Option<PathBuf>;
    fn heartbeat_interval_ms(&self) -> Option<u64>;
    fn heartbeat_timeout_ms(&self) -> Option<u64>;
    fn heartbeat_max_failures(&self) -> Option<u32>;
    fn heartbeat_enabled(&self) -> Option<bool>;
    fn only(&self) -> Option<Vec<String>>;
    fn except(&self) -> Option<Vec<String>>;
}

pub fn resolve_params_multi(cli: &dyn CliOptions, config: &Config) -> Result<Vec<ResolvedParams>> {
    let mut results = Vec::new();
    let only = cli.only();
    let except = cli.except();

    for name in config.servers.keys() {
        if let Some(only_list) = &only
            && !only_list.contains(name)
        {
            continue;
        }
        if let Some(except_list) = &except
            && except_list.contains(name)
        {
            continue;
        }

        // Create a temporary CLI-like object for single resolution
        struct SingleCli<'a> {
            parent: &'a dyn CliOptions,
            service: String,
        }
        impl<'a> CliOptions for SingleCli<'a> {
            fn socket(&self) -> Option<PathBuf> {
                self.parent.socket()
            }
            fn cmd(&self) -> Option<String> {
                self.parent.cmd()
            }
            fn args(&self) -> Vec<String> {
                self.parent.args()
            }
            fn max_active_clients(&self) -> usize {
                self.parent.max_active_clients()
            }
            fn lazy_start(&self) -> Option<bool> {
                self.parent.lazy_start()
            }
            fn max_request_bytes(&self) -> Option<usize> {
                self.parent.max_request_bytes()
            }
            fn request_timeout_ms(&self) -> Option<u64> {
                self.parent.request_timeout_ms()
            }
            fn restart_backoff_ms(&self) -> Option<u64> {
                self.parent.restart_backoff_ms()
            }
            fn restart_backoff_max_ms(&self) -> Option<u64> {
                self.parent.restart_backoff_max_ms()
            }
            fn max_restarts(&self) -> Option<u64> {
                self.parent.max_restarts()
            }
            fn log_level(&self) -> String {
                self.parent.log_level()
            }
            fn tray(&self) -> bool {
                self.parent.tray()
            }
            fn service_name(&self) -> Option<String> {
                self.parent.service_name()
            }
            fn service(&self) -> Option<String> {
                Some(self.service.clone())
            }
            fn status_file(&self) -> Option<PathBuf> {
                self.parent.status_file()
            }
            fn heartbeat_interval_ms(&self) -> Option<u64> {
                self.parent.heartbeat_interval_ms()
            }
            fn heartbeat_timeout_ms(&self) -> Option<u64> {
                self.parent.heartbeat_timeout_ms()
            }
            fn heartbeat_max_failures(&self) -> Option<u32> {
                self.parent.heartbeat_max_failures()
            }
            fn heartbeat_enabled(&self) -> Option<bool> {
                self.parent.heartbeat_enabled()
            }
            fn only(&self) -> Option<Vec<String>> {
                None
            }
            fn except(&self) -> Option<Vec<String>> {
                None
            }
        }

        let single = SingleCli {
            parent: cli,
            service: name.clone(),
        };
        results.push(resolve_params(&single, Some(config))?);
    }
    Ok(results)
}
pub fn resolve_params(cli: &dyn CliOptions, config: Option<&Config>) -> Result<ResolvedParams> {
    let service_cfg = if let Some(cfg) = config {
        if let Some(name) = &cli.service() {
            let found = cfg
                .servers
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("service '{name}' not found in config"))?;
            Some((name.clone(), found))
        } else {
            None
        }
    } else {
        None
    };

    if config.is_some() && cli.service().is_none() {
        return Err(anyhow!("--service is required when using --config"));
    }

    let socket = cli
        .socket()
        .clone()
        .or_else(|| {
            service_cfg
                .as_ref()
                .and_then(|(_, c)| c.socket.clone().map(expand_path))
        })
        .ok_or_else(|| anyhow!("socket path not provided (use --socket or config)"))?;

    let cmd = cli
        .cmd()
        .clone()
        .or_else(|| service_cfg.as_ref().and_then(|(_, c)| c.cmd.clone()))
        .ok_or_else(|| anyhow!("cmd not provided (use --cmd or config)"))?;

    let args = if !cli.args().is_empty() {
        cli.args().clone()
    } else {
        service_cfg
            .as_ref()
            .and_then(|(_, c)| c.args.clone())
            .unwrap_or_default()
    };

    let cwd = service_cfg
        .as_ref()
        .and_then(|(_, c)| c.cwd.as_deref().map(expand_path));

    let max_clients = service_cfg
        .as_ref()
        .and_then(|(_, c)| c.max_active_clients)
        .unwrap_or_else(|| cli.max_active_clients());

    let tray_enabled = if cli.tray() {
        true
    } else {
        service_cfg
            .as_ref()
            .and_then(|(_, c)| c.tray)
            .unwrap_or(false)
    };

    let log_level = service_cfg
        .as_ref()
        .and_then(|(_, c)| c.log_level.clone())
        .unwrap_or_else(|| cli.log_level().clone());

    let lazy_start = cli.lazy_start().unwrap_or_else(|| {
        service_cfg
            .as_ref()
            .and_then(|(_, c)| c.lazy_start)
            .unwrap_or(false)
    });

    let max_request_bytes = cli.max_request_bytes().unwrap_or_else(|| {
        service_cfg
            .as_ref()
            .and_then(|(_, c)| c.max_request_bytes)
            .unwrap_or(1_048_576)
    });

    let request_timeout = Duration::from_millis(cli.request_timeout_ms().unwrap_or_else(|| {
        service_cfg
            .as_ref()
            .and_then(|(_, c)| c.request_timeout_ms)
            .unwrap_or(30_000)
    }));

    let restart_backoff = Duration::from_millis(
        cli.restart_backoff_ms()
            .or_else(|| service_cfg.as_ref().and_then(|(_, c)| c.restart_backoff_ms))
            .unwrap_or(1_000),
    );
    let restart_backoff_max = Duration::from_millis(
        cli.restart_backoff_max_ms()
            .or_else(|| {
                service_cfg
                    .as_ref()
                    .and_then(|(_, c)| c.restart_backoff_max_ms)
            })
            .unwrap_or(30_000),
    );
    let max_restarts = cli
        .max_restarts()
        .or_else(|| service_cfg.as_ref().and_then(|(_, c)| c.max_restarts))
        .unwrap_or(5);

    let status_file = cli.status_file().clone().or_else(|| {
        service_cfg
            .as_ref()
            .and_then(|(_, c)| c.status_file.as_deref().map(expand_path))
    });

    let env = service_cfg.as_ref().and_then(|(_, c)| c.env.clone());

    let service_name_raw = cli
        .service_name()
        .clone()
        .or_else(|| {
            service_cfg
                .as_ref()
                .and_then(|(_, c)| c.service_name.clone())
        })
        .or_else(|| {
            socket
                .file_name()
                .and_then(|n| n.to_string_lossy().split('.').next().map(|s| s.to_string()))
        })
        .unwrap_or_else(|| "rust_mux".to_string());

    // Heartbeat configuration
    let heartbeat_interval = Duration::from_millis(
        cli.heartbeat_interval_ms()
            .or_else(|| {
                service_cfg
                    .as_ref()
                    .and_then(|(_, c)| c.heartbeat_interval_ms)
            })
            .unwrap_or(30_000),
    );
    let heartbeat_timeout = Duration::from_millis(
        cli.heartbeat_timeout_ms()
            .or_else(|| {
                service_cfg
                    .as_ref()
                    .and_then(|(_, c)| c.heartbeat_timeout_ms)
            })
            .unwrap_or(30_000),
    );
    let heartbeat_max_failures = cli
        .heartbeat_max_failures()
        .or_else(|| {
            service_cfg
                .as_ref()
                .and_then(|(_, c)| c.heartbeat_max_failures)
        })
        .unwrap_or(3);
    // Default `false`: MCP protocol does not mandate ping/pong; most upstream
    // servers do not implement rust-mux heartbeats and would be killed in a
    // restart loop. Operators opt in per-service via CLI flag or config.
    let heartbeat_enabled = cli
        .heartbeat_enabled()
        .or_else(|| service_cfg.as_ref().and_then(|(_, c)| c.heartbeat_enabled))
        .unwrap_or(false);

    Ok(ResolvedParams {
        socket,
        cmd,
        args,
        cwd,
        max_clients,
        tray_enabled,
        log_level,
        service_name: service_name_raw,
        lazy_start,
        max_request_bytes,
        request_timeout,
        restart_backoff,
        restart_backoff_max,
        max_restarts,
        status_file,
        env,
        heartbeat_interval,
        heartbeat_timeout,
        heartbeat_max_failures,
        heartbeat_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn safe_read_rejects_parent_traversal() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("config.json");
        fs::write(&file, "{}").expect("write");
        let traversing = dir.path().join("nested/../config.json");

        let err = safe_read_to_string(&traversing).expect_err("must reject parent traversal");

        assert!(
            err.to_string().contains("parent traversal"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn safe_copy_rejects_parent_traversal_destination() {
        let dir = tempdir().expect("tempdir");
        let src = dir.path().join("config.json");
        fs::write(&src, "{}").expect("write");
        let dst = dir.path().join("backup/../config.bak");

        let err = safe_copy_file(&src, &dst).expect_err("must reject parent traversal");

        assert!(
            err.to_string().contains("parent traversal"),
            "unexpected error: {err}"
        );
    }
}
