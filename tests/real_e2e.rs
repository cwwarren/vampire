use reqwest::{Client, StatusCode};
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
use vampire::{App, Config, StatsSnapshot};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live package registries"]
async fn pypi_real_e2e_cold_warm_concurrent() {
    let fixture = RealFixture::new().await.unwrap();

    run_pip_install(&fixture.pkg_base_url, fixture.temp.path(), "cold-a")
        .await
        .unwrap();
    assert_has_artifact_fetches(&fixture.app.stats().snapshot(), "pypi cold");

    fixture.app.stats().reset();
    run_pip_install(&fixture.pkg_base_url, fixture.temp.path(), "warm-a")
        .await
        .unwrap();
    assert_no_artifact_fetches(&fixture.app.stats().snapshot(), "pypi warm");

    let concurrent = RealFixture::new().await.unwrap();
    let first = run_pip_install(
        &concurrent.pkg_base_url,
        concurrent.temp.path(),
        "concurrent-a",
    );
    let second = run_pip_install(
        &concurrent.pkg_base_url,
        concurrent.temp.path(),
        "concurrent-b",
    );
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_duplicate_artifact_fetches(&concurrent.app.stats().snapshot(), "pypi concurrent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live package registries"]
async fn npm_real_e2e_cold_warm_concurrent() {
    let fixture = RealFixture::new().await.unwrap();

    run_npm_install(&fixture.pkg_base_url, fixture.temp.path(), "cold-a")
        .await
        .unwrap();
    assert_has_artifact_fetches(&fixture.app.stats().snapshot(), "npm cold");

    fixture.app.stats().reset();
    run_npm_install(&fixture.pkg_base_url, fixture.temp.path(), "warm-a")
        .await
        .unwrap();
    assert_no_artifact_fetches(&fixture.app.stats().snapshot(), "npm warm");

    let concurrent = RealFixture::new().await.unwrap();
    let first = run_npm_install(
        &concurrent.pkg_base_url,
        concurrent.temp.path(),
        "concurrent-a",
    );
    let second = run_npm_install(
        &concurrent.pkg_base_url,
        concurrent.temp.path(),
        "concurrent-b",
    );
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_duplicate_artifact_fetches(&concurrent.app.stats().snapshot(), "npm concurrent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live package registries"]
async fn cargo_real_e2e_cold_warm_concurrent() {
    let fixture = RealFixture::new().await.unwrap();

    run_cargo_build(&fixture.pkg_base_url, fixture.temp.path(), "cold-a")
        .await
        .unwrap();
    assert_has_artifact_fetches(&fixture.app.stats().snapshot(), "cargo cold");

    fixture.app.stats().reset();
    run_cargo_build(&fixture.pkg_base_url, fixture.temp.path(), "warm-a")
        .await
        .unwrap();
    assert_no_artifact_fetches(&fixture.app.stats().snapshot(), "cargo warm");

    let concurrent = RealFixture::new().await.unwrap();
    let first = run_cargo_build(
        &concurrent.pkg_base_url,
        concurrent.temp.path(),
        "concurrent-a",
    );
    let second = run_cargo_build(
        &concurrent.pkg_base_url,
        concurrent.temp.path(),
        "concurrent-b",
    );
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();
    assert_no_duplicate_artifact_fetches(&concurrent.app.stats().snapshot(), "cargo concurrent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live GitHub through the local proxy"]
async fn github_git_real_e2e_clone_and_fetch_through_rewrite() {
    let fixture = RealFixture::new().await.unwrap();

    run_git_clone_and_fetch(&fixture.git_base_url, fixture.temp.path(), "git-flow")
        .await
        .unwrap();
    assert_has_git_forwards(&fixture.app.stats().snapshot(), "git clone and fetch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live PyPI and GitHub through the local proxy"]
async fn pip_git_pinned_dependency_through_proxy() {
    let fixture = RealFixture::new().await.unwrap();
    let run_dir = fixture.temp.path().join("pip-git");
    let target_dir = run_dir.join("site");
    fs::create_dir_all(&run_dir).await.unwrap();
    let host = localhost_host(&fixture.pkg_base_url).unwrap();
    let git_config = write_git_proxy_config(&run_dir, &fixture.git_base_url)
        .await
        .unwrap();
    let mut envs = vec![("PIP_CONFIG_FILE", "/dev/null".to_owned())];
    envs.extend(git_proxy_envs(&git_config, false));
    let install = run_command(
        "python3",
        [
            "-m",
            "pip",
            "install",
            "--index-url",
            &format!("{}/pypi/simple/", fixture.pkg_base_url),
            "--trusted-host",
            &host,
            "--target",
            target_dir.to_str().expect("utf-8 path"),
            "--disable-pip-version-check",
            "--no-cache-dir",
            "--no-input",
            "git+https://github.com/cwwarren/test-pkgs.git@v0.1.0",
        ],
        None,
        envs,
    )
    .await
    .unwrap();
    ensure_success("pip install git dep", &install).unwrap();
    let validate = run_command(
        "python3",
        ["-c", "from test_pkgs import add; assert add(2, 3) == 5"],
        None,
        vec![(
            "PYTHONPATH",
            target_dir.to_str().expect("utf-8 path").to_owned(),
        )],
    )
    .await
    .unwrap();
    ensure_success("pip git dep validate", &validate).unwrap();
    assert_has_git_forwards(&fixture.app.stats().snapshot(), "pip git dep");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live npm and GitHub through the local proxy"]
async fn npm_git_pinned_dependency_through_proxy() {
    let fixture = RealFixture::new().await.unwrap();
    let run_dir = fixture.temp.path().join("npm-git");
    let home_dir = run_dir.join("home");
    let cache_dir = run_dir.join("npm-cache");
    fs::create_dir_all(&home_dir).await.unwrap();
    fs::create_dir_all(&cache_dir).await.unwrap();
    let git_config = write_git_proxy_config(&run_dir, &fixture.git_base_url)
        .await
        .unwrap();
    fs::write(
        run_dir.join("package.json"),
        r#"{
  "name": "vampire-git-e2e",
  "private": true,
  "dependencies": {
    "test-pkgs": "git+https://github.com/cwwarren/test-pkgs.git#v0.1.0"
  }
}
"#,
    )
    .await
    .unwrap();
    let mut envs = vec![
        ("HOME", home_dir.to_str().expect("utf-8 path").to_owned()),
        (
            "NPM_CONFIG_CACHE",
            cache_dir.to_str().expect("utf-8 path").to_owned(),
        ),
        (
            "NPM_CONFIG_REGISTRY",
            format!("{}/npm/", fixture.pkg_base_url),
        ),
        ("NPM_CONFIG_AUDIT", "false".to_owned()),
        ("NPM_CONFIG_FUND", "false".to_owned()),
        ("NPM_CONFIG_PROGRESS", "false".to_owned()),
        ("NPM_CONFIG_UPDATE_NOTIFIER", "false".to_owned()),
    ];
    envs.extend(git_proxy_envs(&git_config, false));
    let install = run_command(
        "npm",
        ["install", "--ignore-scripts", "--no-audit", "--no-fund"],
        Some(&run_dir),
        envs,
    )
    .await
    .unwrap();
    ensure_success("npm install git dep", &install).unwrap();
    let validate = run_command(
        "node",
        [
            "-e",
            "const {add} = require('test-pkgs'); if (add(2, 3) !== 5) process.exit(1);",
        ],
        Some(&run_dir),
        vec![],
    )
    .await
    .unwrap();
    ensure_success("npm git dep validate", &validate).unwrap();
    assert_has_git_forwards(&fixture.app.stats().snapshot(), "npm git dep");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "hits live crates.io and GitHub through the local proxy"]
async fn cargo_git_pinned_dependency_through_proxy() {
    let fixture = RealFixture::new().await.unwrap();
    let run_dir = fixture.temp.path().join("cargo-git");
    let cargo_home = run_dir.join("cargo-home");
    let target_dir = run_dir.join("target");
    let src_dir = run_dir.join("src");
    fs::create_dir_all(&cargo_home).await.unwrap();
    fs::create_dir_all(&target_dir).await.unwrap();
    fs::create_dir_all(&src_dir).await.unwrap();
    let git_config = write_git_proxy_config(&run_dir, &fixture.git_base_url)
        .await
        .unwrap();
    fs::write(
        run_dir.join("Cargo.toml"),
        r#"[package]
name = "vampire-git-e2e"
version = "0.1.0"
edition = "2021"

[dependencies]
test-pkgs = { git = "https://github.com/cwwarren/test-pkgs.git", tag = "v0.1.0" }
"#,
    )
    .await
    .unwrap();
    fs::write(
        src_dir.join("main.rs"),
        r"fn main() {
    assert_eq!(test_pkgs::add(2, 3), 5);
}
",
    )
    .await
    .unwrap();
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"vampire\"\n\n[source.vampire]\nregistry = \"sparse+{}/cargo/index/\"\n\n[net]\ngit-fetch-with-cli = true\n",
            fixture.pkg_base_url
        ),
    )
    .await
    .unwrap();
    let mut envs = vec![
        (
            "CARGO_HOME",
            cargo_home.to_str().expect("utf-8 path").to_owned(),
        ),
        (
            "CARGO_TARGET_DIR",
            target_dir.to_str().expect("utf-8 path").to_owned(),
        ),
    ];
    envs.extend(git_proxy_envs(&git_config, false));
    let run = run_command("cargo", ["run", "--quiet"], Some(&run_dir), envs)
        .await
        .unwrap();
    ensure_success("cargo run git dep", &run).unwrap();
    assert_has_git_forwards(&fixture.app.stats().snapshot(), "cargo git dep");
}

struct RealFixture {
    temp: TempDir,
    pkg_base_url: String,
    git_base_url: String,
    app: App,
}

impl RealFixture {
    async fn new() -> io::Result<Self> {
        let temp = tempfile::tempdir()?;
        let cache_dir = temp.path().join("cache");
        fs::create_dir_all(&cache_dir).await?;
        let pkg_listener = TcpListener::bind("127.0.0.1:0").await?;
        let git_listener = TcpListener::bind("127.0.0.1:0").await?;
        let pkg_bind = pkg_listener.local_addr()?;
        let git_bind = git_listener.local_addr()?;
        let config = Config {
            pkg_bind,
            git_bind,
            public_base_url: format!("http://{pkg_bind}"),
            cache_dir,
            max_cache_size: 3 * (1 << 30),
            max_upstream_fetches: 32,
            upstream_timeout: Duration::from_mins(5),
        };
        let app = App::new(config).await?;
        let app_serve = app.clone();
        tokio::spawn(async move {
            let _ = app_serve.serve(pkg_listener, git_listener).await;
        });
        let pkg_base_url = format!("http://{pkg_bind}");
        let git_base_url = format!("http://{git_bind}");
        wait_ready(&pkg_base_url).await?;
        wait_git_ready(&git_base_url).await?;
        Ok(Self {
            temp,
            pkg_base_url,
            git_base_url,
            app,
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

async fn wait_git_ready(git_base_url: &str) -> io::Result<()> {
    let client = Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match client.get(format!("{git_base_url}/")).send().await {
            Ok(response) if response.status() == StatusCode::NOT_FOUND => return Ok(()),
            _ if Instant::now() < deadline => sleep(Duration::from_millis(50)).await,
            Ok(response) => {
                return Err(io::Error::other(format!(
                    "git listener did not become ready: {}",
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
            ("NPM_CONFIG_UPDATE_NOTIFIER", "false".to_owned()),
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

async fn run_git_clone_and_fetch(base_url: &str, root: &Path, label: &str) -> io::Result<()> {
    let run_dir = root.join(label);
    let repo_dir = run_dir.join("repo");
    fs::create_dir_all(&run_dir).await?;
    let git_config = write_git_proxy_config(&run_dir, base_url).await?;

    let ls_remote = run_command(
        "git",
        [
            "ls-remote",
            "https://github.com/octocat/Hello-World.git",
            "HEAD",
        ],
        Some(&run_dir),
        git_proxy_envs(&git_config, true),
    )
    .await?;
    ensure_success("git ls-remote", &ls_remote)?;
    let ls_remote_stdout = String::from_utf8_lossy(&ls_remote.stdout);
    assert!(
        ls_remote_stdout.contains("\tHEAD"),
        "expected ls-remote to print HEAD, stdout:\n{ls_remote_stdout}"
    );
    let ls_remote_stderr = String::from_utf8_lossy(&ls_remote.stderr);
    assert!(
        ls_remote_stderr.contains(base_url),
        "expected git trace to mention the local proxy, stderr:\n{ls_remote_stderr}"
    );

    let clone = run_command(
        "git",
        [
            "clone",
            "--depth=1",
            "https://github.com/octocat/Hello-World.git",
            repo_dir.to_str().expect("utf-8 path"),
        ],
        Some(&run_dir),
        git_proxy_envs(&git_config, false),
    )
    .await?;
    ensure_success("git clone", &clone)?;
    assert!(
        repo_dir.join(".git").is_dir(),
        "expected cloned repository at {}",
        repo_dir.display()
    );

    let fetch = run_command(
        "git",
        [
            "-C",
            repo_dir.to_str().expect("utf-8 path"),
            "fetch",
            "--depth=1",
            "origin",
        ],
        Some(&run_dir),
        git_proxy_envs(&git_config, false),
    )
    .await?;
    ensure_success("git fetch", &fetch)?;

    let rev_parse = run_command(
        "git",
        [
            "-C",
            repo_dir.to_str().expect("utf-8 path"),
            "rev-parse",
            "HEAD",
        ],
        Some(&run_dir),
        git_proxy_envs(&git_config, false),
    )
    .await?;
    ensure_success("git rev-parse", &rev_parse)?;
    let head = String::from_utf8_lossy(&rev_parse.stdout);
    assert_eq!(
        head.trim().len(),
        40,
        "expected a full commit SHA, stdout:\n{head}"
    );
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

async fn write_git_proxy_config(
    run_dir: &Path,
    git_base_url: &str,
) -> io::Result<std::path::PathBuf> {
    let git_config = run_dir.join("gitconfig");
    let configure = run_command(
        "git",
        [
            "config",
            "--file",
            git_config.to_str().expect("utf-8 path"),
            &format!("url.{git_base_url}/.insteadOf"),
            "https://github.com/",
        ],
        Some(run_dir),
        vec![],
    )
    .await?;
    ensure_success("git config", &configure)?;
    Ok(git_config)
}

fn git_proxy_envs(git_config: &Path, trace: bool) -> Vec<(&'static str, String)> {
    let mut envs = vec![
        (
            "GIT_CONFIG_GLOBAL",
            git_config.to_str().expect("utf-8 path").to_owned(),
        ),
        ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
        ("GIT_TERMINAL_PROMPT", "0".to_owned()),
    ];
    if trace {
        envs.push(("GIT_TRACE", "1".to_owned()));
        envs.push(("GIT_CURL_VERBOSE", "1".to_owned()));
    }
    envs
}

fn localhost_host(base_url: &str) -> io::Result<String> {
    let url = reqwest::Url::parse(base_url).map_err(io::Error::other)?;
    Ok(url
        .socket_addrs(|| None)
        .map_err(io::Error::other)?
        .first()
        .map_or_else(|| "127.0.0.1".to_owned(), |addr| addr.ip().to_string()))
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

fn assert_has_git_forwards(snapshot: &StatsSnapshot, context: &str) {
    assert!(
        !snapshot.git_forwards.is_empty(),
        "{context}: expected git forwards, got none"
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
