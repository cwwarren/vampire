use crate::state::App;
use crate::{cargo, git, npm, proxy, pypi};
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use std::io;
use tokio::net::TcpListener;

impl App {
    pub async fn serve(
        self,
        pkg_listener: TcpListener,
        git_listener: TcpListener,
        management_listener: TcpListener,
    ) -> io::Result<()> {
        let pkg_app = self.clone();
        let git_app = self.clone();
        let pkg_server = async move {
            axum::serve(pkg_listener, pkg_app.pkg_router())
                .await
                .map_err(io::Error::other)
        };
        let git_server = async move {
            axum::serve(git_listener, git_app.git_router())
                .await
                .map_err(io::Error::other)
        };
        let management_server = async move {
            axum::serve(management_listener, self.management_router())
                .await
                .map_err(io::Error::other)
        };
        tokio::try_join!(pkg_server, git_server, management_server)?;
        Ok(())
    }

    fn pkg_router(self) -> Router {
        Router::new()
            .merge(cargo::router())
            .merge(pypi::router())
            .merge(npm::router())
            .fallback(|| async { proxy::not_found() })
            .with_state(self)
    }

    fn git_router(self) -> Router {
        Router::new().fallback(git::request).with_state(self)
    }

    fn management_router(self) -> Router {
        Router::new()
            .route("/stats", get(management_stats))
            .fallback(|| async { proxy::not_found() })
            .with_state(self)
    }
}

async fn management_stats(State(app): State<App>) -> Response {
    proxy::simple_response(
        StatusCode::OK,
        "text/plain; version=0.0.4; charset=utf-8",
        app.stats().render_prometheus(),
    )
}
