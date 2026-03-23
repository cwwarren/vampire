use serde_json::json;
use std::io;
use tokio::net::TcpListener;
use vampire::failure_log::log_failure;
use vampire::{App, Config};

#[tokio::main]
async fn main() -> io::Result<()> {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            log_failure(
                "startup_failed",
                json!({"stage": "load_config", "error": error}),
            );
            return Err(io::Error::other(error));
        }
    };
    let pkg_listener = match TcpListener::bind(config.pkg_bind).await {
        Ok(listener) => listener,
        Err(error) => {
            log_failure(
                "startup_failed",
                json!({
                    "stage": "bind_pkg_listener",
                    "bind": config.pkg_bind.to_string(),
                    "error": error.to_string(),
                }),
            );
            return Err(error);
        }
    };
    let git_listener = match TcpListener::bind(config.git_bind).await {
        Ok(listener) => listener,
        Err(error) => {
            log_failure(
                "startup_failed",
                json!({
                    "stage": "bind_git_listener",
                    "bind": config.git_bind.to_string(),
                    "error": error.to_string(),
                }),
            );
            return Err(error);
        }
    };
    let app = match App::new(config).await {
        Ok(app) => app,
        Err(error) => {
            log_failure(
                "startup_failed",
                json!({"stage": "build_app", "error": error.to_string()}),
            );
            return Err(error);
        }
    };
    app.serve(pkg_listener, git_listener).await
}
