use reqwest::Client;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::process::Output;
use tempfile::TempDir;
use tokio::fs;
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::time::{Duration, Instant, sleep};
use vampire::{App, AppStats, Config, StatsSnapshot};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live package registries"]
async fn pypi_real_e2e_cold_warm_concurrent() {
    let fixture = RealFixture::new().await.unwrap();

    run_pip_install(&fixture.base_url, fixture.temp.path(), "cold-a")
        .await
        .unwrap();
    assert_has_artifact_fetches(&fixture.stats.snapshot(), "pypi cold");

    fixture.stats.reset();
    run_pip_install(&fixture.base_url, fixture.temp.path(), "warm-a")
        .await
        .unwrap();
    assert_no_artifact_fetches(&fixture.stats.snapshot(), "pypi warm");

    let concurrent = RealFixture::new().await.unwrap();
    let first = run_pip_install(&concurrent.base_url, concurrent.temp.path(), "concurrent-a");
    let second = run_pip_install(&concurrent.base_url, concurrent.temp.path(), "concurrent-b");
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_duplicate_artifact_fetches(&concurrent.stats.snapshot(), "pypi concurrent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live package registries"]
async fn npm_real_e2e_cold_warm_concurrent() {
    let fixture = RealFixture::new().await.unwrap();

    run_npm_install(&fixture.base_url, fixture.temp.path(), "cold-a")
        .await
        .unwrap();
    assert_has_artifact_fetches(&fixture.stats.snapshot(), "npm cold");

    fixture.stats.reset();
    run_npm_install(&fixture.base_url, fixture.temp.path(), "warm-a")
        .await
        .unwrap();
    assert_no_artifact_fetches(&fixture.stats.snapshot(), "npm warm");

    let concurrent = RealFixture::new().await.unwrap();
    let first = run_npm_install(&concurrent.base_url, concurrent.temp.path(), "concurrent-a");
    let second = run_npm_install(&concurrent.base_url, concurrent.temp.path(), "concurrent-b");
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_duplicate_artifact_fetches(&concurrent.stats.snapshot(), "npm concurrent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live package registries"]
async fn cargo_real_e2e_cold_warm_concurrent() {
    let fixture = RealFixture::new().await.unwrap();

    run_cargo_build(&fixture.base_url, fixture.temp.path(), "cold-a")
        .await
        .unwrap();
    assert_has_artifact_fetches(&fixture.stats.snapshot(), "cargo cold");

    fixture.stats.reset();
    run_cargo_build(&fixture.base_url, fixture.temp.path(), "warm-a")
        .await
        .unwrap();
    assert_no_artifact_fetches(&fixture.stats.snapshot(), "cargo warm");

    let concurrent = RealFixture::new().await.unwrap();
    let first = run_cargo_build(&concurrent.base_url, concurrent.temp.path(), "concurrent-a");
    let second = run_cargo_build(&concurrent.base_url, concurrent.temp.path(), "concurrent-b");
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_duplicate_artifact_fetches(&concurrent.stats.snapshot(), "cargo concurrent");
}

struct RealFixture {
    temp: TempDir,
    base_url: String,
    stats: AppStats,
}

impl RealFixture {
    async fn new() -> io::Result<Self> {
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&cache_dir).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bind = listener.local_addr()?;
        let config = Config {
            bind,
            cache_dir,
            max_cache_size: 3 * (1 << 30),
            max_upstream_fetches: 32,
            upstream_timeout: Duration::from_secs(300),
        };
        let app = App::new(config).await?;
        let stats = app.stats();
        tokio::spawn(async move {
            let _ = app.serve(listener).await;
        });
        let base_url = format!("http://{bind}");
        wait_ready(&base_url).await?;
        Ok(Self {
            temp,
            base_url,
            stats,
        })
    }
}

async fn wait_ready(base_url: &str) -> io::Result<()> {
    let client = Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client
            .get(format!("{base_url}/cargo/index/config.json"))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            _ if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
            Ok(response) => {
                return Err(io::Error::other(format!(
                    "proxy did not become ready: {}",
                    response.status()
                )));
            }
            Err(error) => return Err(io::Error::other(error)),
        }
    }
}

async fn run_pip_install(base_url: &str, root: &Path, label: &str) -> io::Result<()> {
    let run_dir = root.join(label);
    let target_dir = run_dir.join("site");
    fs::create_dir_all(&run_dir).await?;
    let host = localhost_host(base_url)?;
    let install = run_command(
        "python3",
        [
            "-m",
            "pip",
            "install",
            "--index-url",
            &format!("{base_url}/pypi/simple/"),
            "--trusted-host",
            &host,
            "--target",
            target_dir.to_str().expect("utf-8 path"),
            "--disable-pip-version-check",
            "--no-cache-dir",
            "--no-input",
            "numpy",
            "pandas",
        ],
        None,
        vec![("PIP_CONFIG_FILE", "/dev/null".to_owned())],
    )
    .await?;
    ensure_success("pip install", &install)?;
    let validate = run_command(
        "python3",
        [
            "-c",
            "import numpy, pandas; print(numpy.__version__); print(pandas.__version__)",
        ],
        None,
        vec![(
            "PYTHONPATH",
            target_dir.to_str().expect("utf-8 path").to_owned(),
        )],
    )
    .await?;
    ensure_success("pip validate", &validate)?;
    Ok(())
}

async fn run_npm_install(base_url: &str, root: &Path, label: &str) -> io::Result<()> {
    let run_dir = root.join(label);
    let home_dir = run_dir.join("home");
    let cache_dir = run_dir.join("npm-cache");
    fs::create_dir_all(&home_dir).await?;
    fs::create_dir_all(&cache_dir).await?;
    fs::write(
        run_dir.join("package.json"),
        r#"{
  "name": "vampire-real-e2e",
  "private": true,
  "dependencies": {
    "axios": "latest",
    "lodash": "latest"
  }
}
"#,
    )
    .await?;
    let install = run_command(
        "npm",
        ["install", "--ignore-scripts", "--no-audit", "--no-fund"],
        Some(&run_dir),
        vec![
            ("HOME", home_dir.to_str().expect("utf-8 path").to_owned()),
            (
                "NPM_CONFIG_CACHE",
                cache_dir.to_str().expect("utf-8 path").to_owned(),
            ),
            ("NPM_CONFIG_REGISTRY", format!("{base_url}/npm/")),
            ("NPM_CONFIG_AUDIT", "false".to_owned()),
            ("NPM_CONFIG_FUND", "false".to_owned()),
            ("NPM_CONFIG_MAXSOCKETS", "8".to_owned()),
            ("NPM_CONFIG_PROGRESS", "false".to_owned()),
        ],
    )
    .await?;
    ensure_success("npm install", &install)?;
    let validate = run_command(
        "node",
        [
            "-e",
            "const axios = require('axios'); const _ = require('lodash'); if (typeof axios.get !== 'function') process.exit(1); if (_.chunk([1,2,3], 2).length !== 2) process.exit(1);",
        ],
        Some(&run_dir),
        vec![],
    )
    .await?;
    ensure_success("npm validate", &validate)?;
    Ok(())
}

async fn run_cargo_build(base_url: &str, root: &Path, label: &str) -> io::Result<()> {
    let run_dir = root.join(label);
    let cargo_home = run_dir.join("cargo-home");
    let target_dir = run_dir.join("target");
    let src_dir = run_dir.join("src");
    fs::create_dir_all(&cargo_home).await?;
    fs::create_dir_all(&target_dir).await?;
    fs::create_dir_all(&src_dir).await?;
    fs::write(
        run_dir.join("Cargo.toml"),
        r#"[package]
name = "vampire-real-e2e"
version = "0.1.0"
edition = "2024"

[dependencies]
hyper = { version = "1", features = ["client", "http1"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
"#,
    )
    .await?;
    fs::write(
        src_dir.join("main.rs"),
        r#"use hyper::http::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
struct Payload {
    value: String,
}

fn main() {
    let request = Request::builder()
        .uri("http://example.com")
        .body(())
        .expect("request");
    assert_eq!(request.uri().host(), Some("example.com"));
    let payload = Payload {
        value: "ok".to_owned(),
    };
    let encoded = serde_json::to_string(&payload).expect("encode");
    assert!(encoded.contains("ok"));
}
"#,
    )
    .await?;
    fs::create_dir_all(cargo_home.join(".cargo")).await?;
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"vampire\"\n\n[source.vampire]\nregistry = \"sparse+{base_url}/cargo/index/\"\n"
        ),
    )
    .await?;
    let run = run_command(
        "cargo",
        ["run", "--quiet"],
        Some(&run_dir),
        vec![
            (
                "CARGO_HOME",
                cargo_home.to_str().expect("utf-8 path").to_owned(),
            ),
            (
                "CARGO_TARGET_DIR",
                target_dir.to_str().expect("utf-8 path").to_owned(),
            ),
        ],
    )
    .await?;
    ensure_success("cargo run", &run)?;
    Ok(())
}

async fn run_command<I, S>(
    program: &str,
    args: I,
    cwd: Option<&Path>,
    envs: Vec<(&str, String)>,
) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().await
}

fn ensure_success(context: &str, output: &Output) -> io::Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "{context} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn localhost_host(base_url: &str) -> io::Result<String> {
    let url = reqwest::Url::parse(base_url).map_err(io::Error::other)?;
    Ok(url
        .socket_addrs(|| None)
        .map_err(io::Error::other)?
        .first()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_owned()))
}

fn assert_has_artifact_fetches(snapshot: &StatsSnapshot, context: &str) {
    assert!(
        !snapshot.artifact_fetches.is_empty(),
        "{context}: expected artifact fetches, got none"
    );
}

fn assert_no_artifact_fetches(snapshot: &StatsSnapshot, context: &str) {
    assert!(
        snapshot.artifact_fetches.is_empty(),
        "{context}: expected no artifact fetches, got {:?}",
        snapshot.artifact_fetches
    );
}

fn assert_no_duplicate_artifact_fetches(snapshot: &StatsSnapshot, context: &str) {
    assert_has_artifact_fetches(snapshot, context);
    let duplicates: HashMap<_, _> = snapshot
        .artifact_fetches
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(url, count)| (url.clone(), *count))
        .collect();
    assert!(
        duplicates.is_empty(),
        "{context}: duplicate artifact fetches detected: {duplicates:?}"
    );
}
