use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CacheClass {
    Artifact,
    Metadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    CargoConfig { origin: String },
    CargoDownload { upstream: Url },
    CargoIndex { upstream: Url },
    NpmPackument { origin: String, upstream: Url },
    NpmTarball { upstream: Url },
    PypiFile { upstream: Url },
    PypiSimpleProject { origin: String, upstream: Url },
    PypiSimpleRoot { origin: String, upstream: Url },
}

#[derive(Clone, Debug)]
pub struct RegistryOrigins {
    pub cargo_download: Url,
    pub cargo_index: Url,
    pub npm: Url,
    pub pypi_files: Url,
    pub pypi_simple: Url,
}

impl Default for RegistryOrigins {
    fn default() -> Self {
        Self {
            cargo_download: Url::parse("https://static.crates.io/").unwrap(),
            cargo_index: Url::parse("https://index.crates.io/").unwrap(),
            npm: Url::parse("https://registry.npmjs.org/").unwrap(),
            pypi_files: Url::parse("https://files.pythonhosted.org/").unwrap(),
            pypi_simple: Url::parse("https://pypi.org/").unwrap(),
        }
    }
}

impl Route {
    pub fn cache_class(&self) -> Option<CacheClass> {
        match self {
            Self::CargoConfig { .. } => None,
            Self::CargoDownload { .. } | Self::NpmTarball { .. } | Self::PypiFile { .. } => {
                Some(CacheClass::Artifact)
            }
            Self::CargoIndex { .. }
            | Self::NpmPackument { .. }
            | Self::PypiSimpleProject { .. }
            | Self::PypiSimpleRoot { .. } => Some(CacheClass::Metadata),
        }
    }

    pub fn upstream(&self) -> Option<&Url> {
        match self {
            Self::CargoConfig { .. } => None,
            Self::CargoDownload { upstream }
            | Self::CargoIndex { upstream }
            | Self::NpmPackument { upstream, .. }
            | Self::NpmTarball { upstream }
            | Self::PypiFile { upstream }
            | Self::PypiSimpleProject { upstream, .. }
            | Self::PypiSimpleRoot { upstream, .. } => Some(upstream),
        }
    }
}

pub fn route_request(
    path: &str,
    query: Option<&str>,
    origin: String,
    upstreams: &RegistryOrigins,
) -> Option<Route> {
    if path == "/cargo/index/config.json" {
        return Some(Route::CargoConfig { origin });
    }
    if let Some(raw) = path.strip_prefix("/cargo/index/") {
        return Some(Route::CargoIndex {
            upstream: join_url(&upstreams.cargo_index, raw)?,
        });
    }
    if let Some(raw) = path.strip_prefix("/cargo/api/v1/crates/") {
        let mut pieces = raw.split('/');
        let crate_name = pieces.next()?;
        let version = pieces.next()?;
        if pieces.next()? != "download" || pieces.next().is_some() {
            return None;
        }
        return Some(Route::CargoDownload {
            upstream: join_url(
                &upstreams.cargo_download,
                &format!("crates/{crate_name}/{crate_name}-{version}.crate"),
            )?,
        });
    }
    if path == "/pypi/simple/" {
        return Some(Route::PypiSimpleRoot {
            origin,
            upstream: join_url(&upstreams.pypi_simple, "simple/")?,
        });
    }
    if let Some(project) = path.strip_prefix("/pypi/simple/") {
        if let Some(project) = project.strip_suffix('/') {
            if !project.is_empty() && !project.contains('/') {
                return Some(Route::PypiSimpleProject {
                    origin,
                    upstream: join_url(&upstreams.pypi_simple, &format!("simple/{project}/"))?,
                });
            }
        }
        return None;
    }
    if let Some(filename) = path.strip_prefix("/pypi/files/") {
        if filename.is_empty() || filename.contains('/') {
            return None;
        }
        let upstream = upstream_from_query(query?, &upstreams.pypi_files)?;
        return Some(Route::PypiFile { upstream });
    }
    if let Some(filename) = path.strip_prefix("/npm/tarballs/") {
        if filename.is_empty() || filename.contains('/') {
            return None;
        }
        let upstream = upstream_from_query(query?, &upstreams.npm)?;
        return Some(Route::NpmTarball { upstream });
    }
    if let Some(package) = path.strip_prefix("/npm/") {
        if package.is_empty() {
            return None;
        }
        return Some(Route::NpmPackument {
            origin,
            upstream: join_url(&upstreams.npm, package)?,
        });
    }
    None
}

pub fn cargo_config(origin: &str) -> Vec<u8> {
    serde_json::json!({ "dl": format!("{origin}/cargo/api/v1/crates") })
        .to_string()
        .into_bytes()
}

pub fn rewrite_metadata(
    route: &Route,
    body: &[u8],
    upstreams: &RegistryOrigins,
) -> Result<Vec<u8>, String> {
    match route {
        Route::PypiSimpleRoot { origin, .. } | Route::PypiSimpleProject { origin, .. } => {
            rewrite_pypi_html(body, upstreams, origin)
        }
        Route::NpmPackument { origin, .. } => rewrite_npm_json(body, upstreams, origin),
        Route::CargoIndex { .. } => Ok(body.to_vec()),
        _ => Ok(body.to_vec()),
    }
}

fn rewrite_pypi_html(
    body: &[u8],
    upstreams: &RegistryOrigins,
    origin: &str,
) -> Result<Vec<u8>, String> {
    let input = String::from_utf8(body.to_vec()).map_err(|error| error.to_string())?;
    let output = href_regex().replace_all(&input, |captures: &regex::Captures<'_>| {
        if let Some(href) = captures.get(1) {
            let rewritten = rewrite_pypi_href(href.as_str(), upstreams, origin);
            return format!("href=\"{rewritten}\"");
        }
        let href = captures
            .get(2)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let rewritten = rewrite_pypi_href(href, upstreams, origin);
        format!("href='{rewritten}'")
    });
    Ok(output.into_owned().into_bytes())
}

fn rewrite_pypi_href(href: &str, upstreams: &RegistryOrigins, origin: &str) -> String {
    if let Ok(url) = Url::parse(href) {
        if matches_origin(&url, &upstreams.pypi_files)
            || url.host_str() == Some("files.pythonhosted.org")
        {
            let fragment = url
                .fragment()
                .map(|fragment| format!("#{fragment}"))
                .unwrap_or_default();
            let mut stripped = normalize_url(url, &upstreams.pypi_files);
            stripped.set_fragment(None);
            let filename = stripped
                .path_segments()
                .and_then(|segments| segments.last())
                .unwrap_or("artifact");
            return format!(
                "{origin}/pypi/files/{filename}?u={}",
                url::form_urlencoded::byte_serialize(stripped.as_str().as_bytes())
                    .collect::<String>()
            ) + &fragment;
        }
        if (matches_origin(&url, &upstreams.pypi_simple) || url.host_str() == Some("pypi.org"))
            && url.path().starts_with("/simple/")
        {
            return format!("{origin}{}", url.path());
        }
    }
    href.to_owned()
}

fn rewrite_npm_json(
    body: &[u8],
    upstreams: &RegistryOrigins,
    origin: &str,
) -> Result<Vec<u8>, String> {
    let mut value: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    rewrite_npm_value(&mut value, upstreams, origin);
    serde_json::to_vec(&value).map_err(|error| error.to_string())
}

fn rewrite_npm_value(value: &mut Value, upstreams: &RegistryOrigins, origin: &str) {
    match value {
        Value::Object(map) => {
            if let Some(dist) = map.get_mut("dist") {
                if let Some(dist_map) = dist.as_object_mut() {
                    if let Some(Value::String(url)) = dist_map.get_mut("tarball") {
                        if let Some(rewritten) = rewrite_npm_tarball(url, upstreams, origin) {
                            *url = rewritten;
                        }
                    }
                }
            }
            for child in map.values_mut() {
                rewrite_npm_value(child, upstreams, origin);
            }
        }
        Value::Array(values) => {
            for child in values {
                rewrite_npm_value(child, upstreams, origin);
            }
        }
        _ => {}
    }
}

fn rewrite_npm_tarball(input: &str, upstreams: &RegistryOrigins, origin: &str) -> Option<String> {
    let url = Url::parse(input).ok()?;
    if !matches_origin(&url, &upstreams.npm) && url.host_str() != Some("registry.npmjs.org") {
        return None;
    }
    let url = normalize_url(url, &upstreams.npm);
    let filename = url
        .path_segments()
        .and_then(|segments| segments.last())
        .unwrap_or("package.tgz");
    Some(format!(
        "{origin}/npm/tarballs/{filename}?u={}",
        url::form_urlencoded::byte_serialize(url.as_str().as_bytes()).collect::<String>()
    ))
}

fn upstream_from_query(query: &str, base: &Url) -> Option<Url> {
    let upstream = url::form_urlencoded::parse(query.as_bytes())
        .find_map(|(key, value)| (key == "u").then(|| value.into_owned()))?;
    let url = Url::parse(&upstream).ok()?;
    matches_origin(&url, base).then_some(url)
}

fn join_url(base: &Url, path: &str) -> Option<Url> {
    base.join(path).ok()
}

fn matches_origin(url: &Url, base: &Url) -> bool {
    url.scheme() == base.scheme()
        && url.host_str() == base.host_str()
        && url.port_or_known_default() == base.port_or_known_default()
}

fn normalize_url(mut url: Url, base: &Url) -> Url {
    let _ = url.set_scheme(base.scheme());
    let _ = url.set_host(base.host_str());
    let _ = url.set_port(base.port());
    url
}

fn href_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r#"href="([^"]+)"|href='([^']+)'"#).unwrap())
}

#[cfg(test)]
mod tests {
    use super::{RegistryOrigins, cargo_config, rewrite_metadata, route_request};
    use serde_json::json;

    #[test]
    fn routes_requests() {
        let upstreams = RegistryOrigins::default();
        let route = route_request(
            "/cargo/index/config.json",
            None,
            "http://localhost".to_owned(),
            &upstreams,
        );
        assert!(route.is_some());

        let route = route_request(
            "/npm/@scope%2fname",
            None,
            "http://localhost".to_owned(),
            &upstreams,
        );
        assert!(route.is_some());

        let route = route_request(
            "/pypi/files/pkg.whl",
            Some("u=https%3A%2F%2Ffiles.pythonhosted.org%2Fpackages%2Fpkg.whl"),
            "http://localhost".to_owned(),
            &upstreams,
        );
        assert!(route.is_some());
    }

    #[test]
    fn rewrites_pypi_html() {
        let body =
            br#"<a href="https://files.pythonhosted.org/packages/pkg.whl#sha256=abc">pkg</a>"#;
        let upstreams = RegistryOrigins::default();
        let route = route_request(
            "/pypi/simple/pkg/",
            None,
            "http://localhost".to_owned(),
            &upstreams,
        )
        .unwrap();
        let rewritten =
            String::from_utf8(rewrite_metadata(&route, body, &upstreams).unwrap()).unwrap();
        assert!(rewritten.contains("http://localhost/pypi/files/pkg.whl?u="));
        assert!(rewritten.contains("#sha256=abc"));
    }

    #[test]
    fn rewrites_npm_tarballs() {
        let upstreams = RegistryOrigins::default();
        let route =
            route_request("/npm/pkg", None, "http://localhost".to_owned(), &upstreams).unwrap();
        let body = serde_json::to_vec(&json!({
            "versions": {
                "1.0.0": {
                    "dist": { "tarball": "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz" }
                }
            }
        }))
        .unwrap();
        let rewritten = serde_json::from_slice::<serde_json::Value>(
            &rewrite_metadata(&route, &body, &upstreams).unwrap(),
        )
        .unwrap();
        assert_eq!(
            rewritten["versions"]["1.0.0"]["dist"]["tarball"]
                .as_str()
                .unwrap()
                .starts_with("http://localhost/npm/tarballs/pkg-1.0.0.tgz?u="),
            true
        );
    }

    #[test]
    fn cargo_config_uses_origin() {
        let body = String::from_utf8(cargo_config("https://mirror.example")).unwrap();
        assert!(body.contains("https://mirror.example/cargo/api/v1/crates"));
    }
}
