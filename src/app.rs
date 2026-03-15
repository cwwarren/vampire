use crate::state::App;
use crate::{cargo, npm, proxy, pypi};
use axum::Router;
use std::io;
use tokio::net::TcpListener;

impl App {
    pub async fn serve(self, listener: TcpListener) -> io::Result<()> {
        axum::serve(listener, self.router())
            .await
            .map_err(io::Error::other)
    }

    fn router(self) -> Router {
        Router::new()
            .merge(cargo::router())
            .merge(pypi::router())
            .merge(npm::router())
            .fallback(proxy::not_found)
            .with_state(self)
    }
}
