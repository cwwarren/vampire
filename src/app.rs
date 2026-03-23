use crate::state::App;
use crate::{cargo, git, npm, proxy, pypi};
use axum::Router;
use std::io;
use tokio::net::TcpListener;

impl App {
    pub async fn serve(
        self,
        pkg_listener: TcpListener,
        git_listener: TcpListener,
    ) -> io::Result<()> {
        let pkg_app = self.clone();
        let pkg_server = async move {
            axum::serve(pkg_listener, pkg_app.pkg_router())
                .await
                .map_err(io::Error::other)
        };
        let git_server = async move {
            axum::serve(git_listener, self.git_router())
                .await
                .map_err(io::Error::other)
        };
        tokio::try_join!(pkg_server, git_server)?;
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
}
