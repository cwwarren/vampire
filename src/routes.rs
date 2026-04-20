use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use url::Url;

#[derive(Clone, Debug)]
pub struct RegistryOrigins {
    pub cargo_download: Url,
    pub cargo_index: Url,
    pub github: Url,
    pub npm: Url,
    pub pypi_files: Url,
    pub pypi_simple: Url,
}

impl Default for RegistryOrigins {
    fn default() -> Self {
        Self {
            cargo_download: Url::parse("https://static.crates.io/").unwrap(),
            cargo_index: Url::parse("https://index.crates.io/").unwrap(),
            github: Url::parse("https://github.com/").unwrap(),
            npm: Url::parse("https://registry.npmjs.org/").unwrap(),
            pypi_files: Url::parse("https://files.pythonhosted.org/").unwrap(),
            pypi_simple: Url::parse("https://pypi.org/").unwrap(),
        }
    }
}

pub fn cargo_config(origin: &str) -> Vec<u8> {
    serde_json::json!({ "dl": format!("{origin}/cargo/api/v1/crates") })
        .to_string()
        .into_bytes()
}

pub fn cargo_index_url(upstreams: &RegistryOrigins, path: &str) -> Option<Url> {
    join_url(&upstreams.cargo_index, path)
}

pub fn cargo_download_url(
    upstreams: &RegistryOrigins,
    crate_name: &str,
    version: &str,
) -> Option<Url> {
    join_url(
        &upstreams.cargo_download,
        &format!("crates/{crate_name}/{crate_name}-{version}.crate"),
    )
}

pub fn pypi_simple_url(upstreams: &RegistryOrigins, project: Option<&str>) -> Option<Url> {
    match project {
        None => join_url(&upstreams.pypi_simple, "simple/"),
        Some(project) => {
            if project.is_empty() || project.contains('/') {
                return None;
            }
            let mut url = join_url(&upstreams.pypi_simple, "simple/")?;
            url.path_segments_mut()
                .ok()?
                .pop_if_empty()
                .push(project)
                .push("");
            Some(url)
        }
    }
}

pub fn pypi_file_url(path: &str, upstreams: &RegistryOrigins) -> Option<Url> {
    join_url(&upstreams.pypi_files, path)
}

pub fn npm_packument_url(upstreams: &RegistryOrigins, package: &str) -> Option<Url> {
    if package.is_empty() {
        return None;
    }
    join_url(&upstreams.npm, package)
}

pub fn npm_tarball_url(path: &str, upstreams: &RegistryOrigins) -> Option<Url> {
    join_url(&upstreams.npm, path)
}

pub fn rewrite_pypi_html(
    body: &[u8],
    upstreams: &RegistryOrigins,
    origin: &str,
) -> Result<Vec<u8>, String> {
    let input = std::str::from_utf8(body).map_err(|error| error.to_string())?;
    let output = href_regex().replace_all(input, |captures: &regex::Captures<'_>| {
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

pub fn rewrite_npm_json(
    body: &[u8],
    upstreams: &RegistryOrigins,
    origin: &str,
) -> Result<Vec<u8>, String> {
    let mut value: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    rewrite_npm_dist(&mut value, upstreams, origin);
    if let Some(versions) = value
        .get_mut("versions")
        .and_then(|value| value.as_object_mut())
    {
        for version in versions.values_mut() {
            rewrite_npm_dist(version, upstreams, origin);
        }
    }
    serde_json::to_vec(&value).map_err(|error| error.to_string())
}

fn rewrite_npm_dist(value: &mut Value, upstreams: &RegistryOrigins, origin: &str) {
    let Some(dist) = value
        .get_mut("dist")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    let Some(Value::String(url)) = dist.get_mut("tarball") else {
        return;
    };
    if let Some(rewritten) = rewrite_npm_tarball(url, upstreams, origin) {
        *url = rewritten;
    }
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
            let normalized = normalize_url(url, &upstreams.pypi_files);
            let path = normalized
                .path()
                .strip_prefix('/')
                .unwrap_or(normalized.path());
            return format!("{origin}/pypi/files/{path}{fragment}");
        }
        if (matches_origin(&url, &upstreams.pypi_simple) || url.host_str() == Some("pypi.org"))
            && url.path().starts_with("/simple/")
        {
            return format!("{origin}{}", url.path());
        }
    }
    href.to_owned()
}

fn rewrite_npm_tarball(input: &str, upstreams: &RegistryOrigins, origin: &str) -> Option<String> {
    let url = Url::parse(input).ok()?;
    if !matches_origin(&url, &upstreams.npm) && url.host_str() != Some("registry.npmjs.org") {
        return None;
    }
    let url = normalize_url(url, &upstreams.npm);
    let path = url.path().strip_prefix('/').unwrap_or(url.path());
    Some(format!("{origin}/npm/tarballs/{path}"))
}

pub(crate) fn join_url(base: &Url, path: &str) -> Option<Url> {
    if path.starts_with('/') || path.starts_with("//") || Url::parse(path).is_ok() {
        return None;
    }
    let url = Url::parse(&format!("{}{path}", base.as_str())).ok()?;
    matches_origin(&url, base).then_some(url)
}

pub(crate) fn matches_origin(url: &Url, base: &Url) -> bool {
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
    use super::{
        RegistryOrigins, cargo_config, cargo_download_url, cargo_index_url, npm_packument_url,
        npm_tarball_url, pypi_file_url, pypi_simple_url, rewrite_npm_json, rewrite_pypi_html,
    };
    use serde_json::json;

    #[test]
    fn builds_urls() {
        let upstreams = RegistryOrigins::default();
        assert!(cargo_index_url(&upstreams, "config.json").is_some());
        assert!(cargo_download_url(&upstreams, "serde", "1.0.0").is_some());
        assert!(npm_packument_url(&upstreams, "@scope%2fname").is_some());
        assert!(pypi_simple_url(&upstreams, Some("pkg")).is_some());
        assert!(pypi_file_url("packages/pkg.whl", &upstreams).is_some());
        assert!(npm_tarball_url("pkg/-/pkg-1.0.0.tgz", &upstreams).is_some());
        assert_eq!(upstreams.github.as_str(), "https://github.com/");
    }

    #[test]
    fn rejects_slash_containing_pypi_projects() {
        let upstreams = RegistryOrigins::default();
        assert!(pypi_simple_url(&upstreams, Some("../../admin")).is_none());
        assert!(pypi_simple_url(&upstreams, Some("pkg/extra")).is_none());
    }

    #[test]
    fn builds_pypi_project_as_path_segment() {
        let upstreams = RegistryOrigins::default();
        let url = pypi_simple_url(&upstreams, Some("pkg?query#fragment")).unwrap();
        assert_eq!(
            url.as_str(),
            "https://pypi.org/simple/pkg%3Fquery%23fragment/"
        );
    }

    #[test]
    fn rewrites_pypi_html_links() {
        let body =
            br#"<a href="https://files.pythonhosted.org/packages/pkg.whl#sha256=abc">pkg</a>"#;
        let upstreams = RegistryOrigins::default();
        let rewritten =
            String::from_utf8(rewrite_pypi_html(body, &upstreams, "http://localhost").unwrap())
                .unwrap();
        assert!(rewritten.contains("http://localhost/pypi/files/packages/pkg.whl#sha256=abc"));
    }

    #[test]
    fn rewrites_npm_tarballs() {
        let upstreams = RegistryOrigins::default();
        let body = serde_json::to_vec(&json!({
            "versions": {
                "1.0.0": {
                    "dist": { "tarball": "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz" }
                }
            }
        }))
        .unwrap();
        let rewritten = serde_json::from_slice::<serde_json::Value>(
            &rewrite_npm_json(&body, &upstreams, "http://localhost").unwrap(),
        )
        .unwrap();
        assert!(
            rewritten["versions"]["1.0.0"]["dist"]["tarball"]
                .as_str()
                .unwrap()
                == "http://localhost/npm/tarballs/pkg/-/pkg-1.0.0.tgz"
        );
    }

    #[test]
    fn rewrites_root_npm_tarball() {
        let upstreams = RegistryOrigins::default();
        let body = serde_json::to_vec(&json!({
            "name": "pkg",
            "version": "1.0.0",
            "dist": { "tarball": "https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz" }
        }))
        .unwrap();
        let rewritten = serde_json::from_slice::<serde_json::Value>(
            &rewrite_npm_json(&body, &upstreams, "http://localhost").unwrap(),
        )
        .unwrap();
        assert_eq!(
            rewritten["dist"]["tarball"].as_str().unwrap(),
            "http://localhost/npm/tarballs/pkg/-/pkg-1.0.0.tgz"
        );
    }

    #[test]
    fn rejects_absolute_npm_upstream_paths() {
        let upstreams = RegistryOrigins::default();
        assert!(npm_packument_url(&upstreams, "http://127.0.0.1:18080/").is_none());
        assert!(npm_packument_url(&upstreams, "//127.0.0.1:18080/").is_none());
    }

    #[test]
    fn rejects_absolute_cargo_index_paths() {
        let upstreams = RegistryOrigins::default();
        assert!(cargo_index_url(&upstreams, "http://127.0.0.1:18080/").is_none());
        assert!(cargo_index_url(&upstreams, "//127.0.0.1:18080/").is_none());
    }

    #[test]
    fn preserves_scoped_npm_package_encoding() {
        let upstreams = RegistryOrigins::default();
        let url = npm_packument_url(&upstreams, "@scope%2fname").unwrap();
        assert_eq!(url.as_str(), "https://registry.npmjs.org/@scope%2fname");
    }

    #[test]
    fn cargo_config_uses_origin() {
        let body = String::from_utf8(cargo_config("https://mirror.example")).unwrap();
        assert!(body.contains("https://mirror.example/cargo/api/v1/crates"));
    }
}
