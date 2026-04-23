use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub pkg_bind: SocketAddr,
    pub git_bind: SocketAddr,
    pub management_bind: SocketAddr,
    pub public_base_url: String,
    pub cache_dir: PathBuf,
    pub max_cache_size: u64,
    pub max_upstream_fetches: usize,
    pub upstream_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_vars(|key| env::var(key).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let pkg_bind = parse_bind(
            &var,
            "VAMPIRE_PKG_BIND",
            "VAMPIRE_PKG_HOST",
            "127.0.0.1",
            "VAMPIRE_PKG_PORT",
            8080,
        )?;
        let git_bind = parse_bind(
            &var,
            "VAMPIRE_GIT_BIND",
            "VAMPIRE_GIT_HOST",
            "127.0.0.1",
            "VAMPIRE_GIT_PORT",
            8081,
        )?;
        let management_bind = parse_bind(
            &var,
            "VAMPIRE_MANAGEMENT_BIND",
            "VAMPIRE_MANAGEMENT_HOST",
            "127.0.0.1",
            "VAMPIRE_MANAGEMENT_PORT",
            8082,
        )?;
        let public_base_url = var("VAMPIRE_PUBLIC_BASE_URL")
            .ok_or_else(|| "VAMPIRE_PUBLIC_BASE_URL is required".to_owned())
            .and_then(|value| parse_public_base_url(&value))?;
        let cache_dir = var("VAMPIRE_CACHE_DIR")
            .map_or_else(|| PathBuf::from("./.cache/vampire"), PathBuf::from);
        let max_cache_size_mb: u64 = var("VAMPIRE_MAX_CACHE_SIZE_MB")
            .ok_or_else(|| "VAMPIRE_MAX_CACHE_SIZE_MB is required".to_owned())
            .and_then(|value| {
                value
                    .trim()
                    .parse()
                    .map_err(|error| format!("invalid VAMPIRE_MAX_CACHE_SIZE_MB: {error}"))
            })?;
        let max_cache_size = max_cache_size_mb * 1_000_000;
        let max_upstream_fetches = var("VAMPIRE_MAX_UPSTREAM_FETCHES")
            .unwrap_or_else(|| "32".to_owned())
            .parse()
            .map_err(|error| format!("invalid VAMPIRE_MAX_UPSTREAM_FETCHES: {error}"))?;
        let upstream_timeout_ms: u64 = var("VAMPIRE_UPSTREAM_TIMEOUT_MS")
            .unwrap_or_else(|| "30000".to_owned())
            .parse()
            .map_err(|error| format!("invalid VAMPIRE_UPSTREAM_TIMEOUT_MS: {error}"))?;
        Ok(Self {
            pkg_bind,
            git_bind,
            management_bind,
            public_base_url,
            cache_dir,
            max_cache_size,
            max_upstream_fetches,
            upstream_timeout: Duration::from_millis(upstream_timeout_ms),
        })
    }
}

fn parse_bind(
    var: &impl Fn(&str) -> Option<String>,
    bind_key: &str,
    host_key: &str,
    default_host: &str,
    port_key: &str,
    default_port: u16,
) -> Result<SocketAddr, String> {
    if let Some(value) = var(bind_key) {
        return value
            .trim()
            .parse()
            .map_err(|error| format!("invalid {bind_key}: {error}"));
    }
    let host = var(host_key).unwrap_or_else(|| default_host.to_owned());
    let port = var(port_key)
        .unwrap_or_else(|| default_port.to_string())
        .trim()
        .parse::<u16>()
        .map_err(|error| format!("invalid {port_key}: {error}"))?;
    socket_addr_from_host_port(host_key, &host, port_key, port)
}

fn socket_addr_from_host_port(
    host_key: &str,
    host: &str,
    port_key: &str,
    port: u16,
) -> Result<SocketAddr, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err(format!("invalid {host_key}: host must not be empty"));
    }
    let bind = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    bind.parse()
        .map_err(|error| format!("invalid {host_key}/{port_key}: {error}"))
}

fn parse_public_base_url(value: &str) -> Result<String, String> {
    let url = Url::parse(value.trim())
        .map_err(|error| format!("invalid VAMPIRE_PUBLIC_BASE_URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("invalid VAMPIRE_PUBLIC_BASE_URL: scheme must be http or https".to_owned());
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(
            "invalid VAMPIRE_PUBLIC_BASE_URL: credentials and missing hosts are not allowed"
                .to_owned(),
        );
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(
            "invalid VAMPIRE_PUBLIC_BASE_URL: path, query, and fragment are not allowed".to_owned(),
        );
    }
    Ok(url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;
    use std::time::Duration;

    fn config_with(overrides: &[(&str, &str)]) -> Result<Config, String> {
        let mut vars: HashMap<String, String> = HashMap::new();
        vars.insert("VAMPIRE_MAX_CACHE_SIZE_MB".into(), "100".into());
        vars.insert(
            "VAMPIRE_PUBLIC_BASE_URL".into(),
            "https://mirror.example".into(),
        );
        for (key, value) in overrides {
            vars.insert((*key).into(), (*value).into());
        }
        Config::from_vars(|key| vars.get(key).cloned())
    }

    #[test]
    fn defaults() {
        let config = config_with(&[]).unwrap();
        assert_eq!(config.pkg_bind, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.git_bind, "127.0.0.1:8081".parse().unwrap());
        assert_eq!(config.management_bind, "127.0.0.1:8082".parse().unwrap());
        assert_eq!(config.public_base_url, "https://mirror.example");
        assert_eq!(config.cache_dir.to_str().unwrap(), "./.cache/vampire");
        assert_eq!(config.max_cache_size, 100_000_000);
        assert_eq!(config.max_upstream_fetches, 32);
        assert_eq!(config.upstream_timeout, Duration::from_secs(30));
    }

    #[test]
    fn bind_overrides() {
        let config = config_with(&[
            ("VAMPIRE_PKG_BIND", "0.0.0.0:9090"),
            ("VAMPIRE_GIT_BIND", "0.0.0.0:9091"),
            ("VAMPIRE_MANAGEMENT_BIND", "0.0.0.0:9092"),
            ("VAMPIRE_PUBLIC_BASE_URL", "https://mirror.example:8443/"),
            ("VAMPIRE_CACHE_DIR", "/tmp/cache"),
            ("VAMPIRE_MAX_CACHE_SIZE_MB", "5000"),
            ("VAMPIRE_MAX_UPSTREAM_FETCHES", "8"),
            ("VAMPIRE_UPSTREAM_TIMEOUT_MS", "60000"),
        ])
        .unwrap();
        assert_eq!(config.pkg_bind, "0.0.0.0:9090".parse().unwrap());
        assert_eq!(config.git_bind, "0.0.0.0:9091".parse().unwrap());
        assert_eq!(config.management_bind, "0.0.0.0:9092".parse().unwrap());
        assert_eq!(config.public_base_url, "https://mirror.example:8443");
        assert_eq!(config.cache_dir.to_str().unwrap(), "/tmp/cache");
        assert_eq!(config.max_cache_size, 5_000_000_000);
        assert_eq!(config.max_upstream_fetches, 8);
        assert_eq!(config.upstream_timeout, Duration::from_mins(1));
    }

    #[test]
    fn host_and_port_overrides() {
        let config = config_with(&[
            ("VAMPIRE_PKG_HOST", "0.0.0.0"),
            ("VAMPIRE_PKG_PORT", "9090"),
            ("VAMPIRE_GIT_HOST", "127.0.0.2"),
            ("VAMPIRE_GIT_PORT", "9091"),
            ("VAMPIRE_MANAGEMENT_HOST", "127.0.0.3"),
            ("VAMPIRE_MANAGEMENT_PORT", "9092"),
        ])
        .unwrap();
        assert_eq!(config.pkg_bind, "0.0.0.0:9090".parse().unwrap());
        assert_eq!(config.git_bind, "127.0.0.2:9091".parse().unwrap());
        assert_eq!(config.management_bind, "127.0.0.3:9092".parse().unwrap());
    }

    #[test]
    fn requires_public_base_url() {
        let result = Config::from_vars(|key| {
            if key == "VAMPIRE_MAX_CACHE_SIZE_MB" {
                return Some("100".into());
            }
            None
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("VAMPIRE_PUBLIC_BASE_URL"));
    }

    #[test]
    fn requires_max_cache_size() {
        let result = Config::from_vars(|key| {
            if key == "VAMPIRE_PUBLIC_BASE_URL" {
                return Some("https://mirror.example".into());
            }
            None
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("VAMPIRE_MAX_CACHE_SIZE_MB"));
    }

    #[test]
    fn rejects_invalid_values() {
        assert!(config_with(&[("VAMPIRE_MAX_CACHE_SIZE_MB", "abc")]).is_err());
        assert!(config_with(&[("VAMPIRE_PKG_BIND", "not-an-addr")]).is_err());
        assert!(config_with(&[("VAMPIRE_GIT_BIND", "still-not-an-addr")]).is_err());
        assert!(config_with(&[("VAMPIRE_MANAGEMENT_BIND", "bad-addr")]).is_err());
        assert!(config_with(&[("VAMPIRE_PKG_PORT", "bad-port")]).is_err());
        assert!(config_with(&[("VAMPIRE_PUBLIC_BASE_URL", "ftp://mirror.example")]).is_err());
        assert!(config_with(&[("VAMPIRE_PUBLIC_BASE_URL", "https://mirror.example/pkg")]).is_err());
        assert!(config_with(&[("VAMPIRE_UPSTREAM_TIMEOUT_MS", "xyz")]).is_err());
        assert!(config_with(&[("VAMPIRE_MAX_UPSTREAM_FETCHES", "-1")]).is_err());
    }
}
