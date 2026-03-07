use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub cache_dir: PathBuf,
    pub max_cache_size: u64,
    pub max_upstream_fetches: usize,
    pub upstream_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind = env::var("VAMPIRE_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
            .parse()
            .map_err(|error| format!("invalid VAMPIRE_BIND: {error}"))?;
        let cache_dir = env::var("VAMPIRE_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./.cache/vampire"));
        let max_cache_size = env::var("VAMPIRE_MAX_CACHE_SIZE")
            .map_err(|_| "VAMPIRE_MAX_CACHE_SIZE is required".to_owned())
            .and_then(|value| parse_size(&value))?;
        let max_upstream_fetches = env::var("VAMPIRE_MAX_UPSTREAM_FETCHES")
            .unwrap_or_else(|_| "32".to_owned())
            .parse()
            .map_err(|error| format!("invalid VAMPIRE_MAX_UPSTREAM_FETCHES: {error}"))?;
        let upstream_timeout =
            env::var("VAMPIRE_UPSTREAM_TIMEOUT").unwrap_or_else(|_| "30s".to_owned());
        let upstream_timeout = parse_duration(&upstream_timeout)?;
        Ok(Self {
            bind,
            cache_dir,
            max_cache_size,
            max_upstream_fetches,
            upstream_timeout,
        })
    }
}

fn parse_size(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("VAMPIRE_MAX_CACHE_SIZE cannot be empty".to_owned());
    }
    let split = trimmed
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(split);
    let base: u64 = number
        .parse()
        .map_err(|error| format!("invalid size {trimmed:?}: {error}"))?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "ki" | "kib" => 1 << 10,
        "mi" | "mib" => 1 << 20,
        "gi" | "gib" => 1 << 30,
        other => return Err(format!("unsupported size suffix: {other}")),
    };
    base.checked_mul(multiplier)
        .ok_or_else(|| format!("size is too large: {trimmed}"))
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    if let Some(value) = trimmed.strip_suffix("ms") {
        return value
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|error| format!("invalid duration {trimmed:?}: {error}"));
    }
    if let Some(value) = trimmed.strip_suffix('s') {
        return value
            .parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|error| format!("invalid duration {trimmed:?}: {error}"));
    }
    if let Some(value) = trimmed.strip_suffix('m') {
        return value
            .parse::<u64>()
            .map(|minutes| Duration::from_secs(minutes * 60))
            .map_err(|error| format!("invalid duration {trimmed:?}: {error}"));
    }
    trimmed
        .parse::<u64>()
        .map(Duration::from_secs)
        .map_err(|error| format!("invalid duration {trimmed:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_duration, parse_size};
    use std::time::Duration;

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_size("1GiB").unwrap(), 1 << 30);
        assert_eq!(parse_size("10mb").unwrap(), 10_000_000);
    }

    #[test]
    fn parse_durations() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    }
}
