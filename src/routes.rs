use regex::Regex;
use serde::Serialize;
use serde_json::value::RawValue;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::OnceLock;
use url::Url;

pub(crate) const MAX_METADATA_BODY_LEN: usize = 128 * 1024 * 1024;

struct LimitedOutput {
    bytes: Vec<u8>,
    max_len: usize,
}

impl LimitedOutput {
    fn new(capacity: usize, max_len: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity.min(max_len)),
            max_len,
        }
    }

    fn extend(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes
            .len()
            .checked_add(bytes.len())
            .filter(|len| *len <= self.max_len)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "rewritten metadata body exceeds output limit",
                )
            })?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for LimitedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.extend(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct RegistryOrigins {
    pub(crate) cargo_download: Url,
    pub(crate) cargo_index: Url,
    pub(crate) github: Url,
    pub(crate) npm: Url,
    pub(crate) pypi_files: Url,
    pub(crate) pypi_simple: Url,
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

impl RegistryOrigins {
    pub(crate) fn loopback(addr: SocketAddr) -> Self {
        let origin = Url::parse(&format!("http://{addr}/")).expect("loopback origin");
        Self {
            cargo_download: origin.clone(),
            cargo_index: origin.clone(),
            github: origin.clone(),
            npm: origin.clone(),
            pypi_files: origin.clone(),
            pypi_simple: origin,
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
        Some(project) => canonical_segment(project)
            .then(|| format!("simple/{project}/"))
            .and_then(|path| join_url(&upstreams.pypi_simple, &path)),
    }
}

pub fn pypi_file_url(path: &str, upstreams: &RegistryOrigins) -> Option<Url> {
    join_url(&upstreams.pypi_files, path)
}

pub fn npm_packument_url(upstreams: &RegistryOrigins, package: &str) -> Option<Url> {
    let package = if package.starts_with('@') {
        let separator = package.find("%2F").or_else(|| package.find("%2f"))?;
        let scope = &package[..separator];
        let name = &package[separator + 3..];
        if scope.len() <= 1 || !canonical_segment(scope) || !canonical_segment(name) {
            return None;
        }
        format!("{scope}%2F{name}")
    } else {
        canonical_segment(package).then(|| package.to_owned())?
    };
    if package.contains('/') {
        return None;
    }
    build_url(&upstreams.npm, &package)
}

pub fn npm_tarball_url(path: &str, upstreams: &RegistryOrigins) -> Option<Url> {
    join_url(&upstreams.npm, path)
}

pub fn rewrite_pypi_html(
    body: &[u8],
    upstreams: &RegistryOrigins,
    origin: &str,
    max_len: usize,
) -> Result<Vec<u8>, String> {
    let input = std::str::from_utf8(body).map_err(|error| error.to_string())?;
    let mut output = LimitedOutput::new(input.len(), max_len.min(MAX_METADATA_BODY_LEN));
    let mut previous_end = 0;
    for captures in href_regex().captures_iter(input) {
        let whole = captures.get(0).expect("href capture");
        output
            .extend(&input.as_bytes()[previous_end..whole.start()])
            .map_err(|error| error.to_string())?;
        if let Some(href) = captures.get(1) {
            let rewritten = rewrite_pypi_href(href.as_str(), upstreams, origin);
            output
                .extend(b"href=\"")
                .and_then(|()| output.extend(rewritten.as_bytes()))
                .and_then(|()| output.extend(b"\""))
                .map_err(|error| error.to_string())?;
        } else {
            let href = captures
                .get(2)
                .map(|value| value.as_str())
                .unwrap_or_default();
            let rewritten = rewrite_pypi_href(href, upstreams, origin);
            output
                .extend(b"href='")
                .and_then(|()| output.extend(rewritten.as_bytes()))
                .and_then(|()| output.extend(b"'"))
                .map_err(|error| error.to_string())?;
        }
        previous_end = whole.end();
    }
    output
        .extend(&input.as_bytes()[previous_end..])
        .map_err(|error| error.to_string())?;
    Ok(output.finish())
}

pub fn rewrite_npm_json(
    body: &[u8],
    upstreams: &RegistryOrigins,
    origin: &str,
    max_len: usize,
) -> Result<Vec<u8>, String> {
    let mut output = LimitedOutput::new(body.len(), max_len.min(MAX_METADATA_BODY_LEN));
    let value: &RawValue = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    write_npm_package(&mut output, value, upstreams, origin, true)?;
    Ok(output.finish())
}

fn write_npm_package(
    output: &mut LimitedOutput,
    value: &RawValue,
    upstreams: &RegistryOrigins,
    origin: &str,
    rewrite_versions: bool,
) -> Result<(), String> {
    let Ok(fields) = serde_json::from_str::<BTreeMap<String, &RawValue>>(value.get()) else {
        return write_json(output, value);
    };
    output.extend(b"{").map_err(|error| error.to_string())?;
    for (index, (key, value)) in fields.into_iter().enumerate() {
        if index > 0 {
            output.extend(b",").map_err(|error| error.to_string())?;
        }
        write_json(output, &key)?;
        output.extend(b":").map_err(|error| error.to_string())?;
        if key == "dist" {
            write_npm_dist(output, value, upstreams, origin)?;
        } else if rewrite_versions && key == "versions" {
            write_npm_versions(output, value, upstreams, origin)?;
        } else {
            write_json(output, value)?;
        }
    }
    output.extend(b"}").map_err(|error| error.to_string())
}

fn write_npm_versions(
    output: &mut LimitedOutput,
    value: &RawValue,
    upstreams: &RegistryOrigins,
    origin: &str,
) -> Result<(), String> {
    let Ok(versions) = serde_json::from_str::<BTreeMap<String, &RawValue>>(value.get()) else {
        return write_json(output, value);
    };
    output.extend(b"{").map_err(|error| error.to_string())?;
    for (index, (version, value)) in versions.into_iter().enumerate() {
        if index > 0 {
            output.extend(b",").map_err(|error| error.to_string())?;
        }
        write_json(output, &version)?;
        output.extend(b":").map_err(|error| error.to_string())?;
        write_npm_package(output, value, upstreams, origin, false)?;
    }
    output.extend(b"}").map_err(|error| error.to_string())
}

fn write_npm_dist(
    output: &mut LimitedOutput,
    value: &RawValue,
    upstreams: &RegistryOrigins,
    origin: &str,
) -> Result<(), String> {
    let Ok(fields) = serde_json::from_str::<BTreeMap<String, &RawValue>>(value.get()) else {
        return write_json(output, value);
    };
    output.extend(b"{").map_err(|error| error.to_string())?;
    for (index, (key, value)) in fields.into_iter().enumerate() {
        if index > 0 {
            output.extend(b",").map_err(|error| error.to_string())?;
        }
        write_json(output, &key)?;
        output.extend(b":").map_err(|error| error.to_string())?;
        if key == "tarball"
            && let Ok(url) = serde_json::from_str::<String>(value.get())
            && let Some(rewritten) = rewrite_npm_tarball(&url, upstreams, origin)
        {
            write_json(output, &rewritten)?;
        } else {
            write_json(output, value)?;
        }
    }
    output.extend(b"}").map_err(|error| error.to_string())
}

fn write_json<T: Serialize + ?Sized>(output: &mut LimitedOutput, value: &T) -> Result<(), String> {
    serde_json::to_writer(output, value).map_err(|error| error.to_string())
}

fn rewrite_pypi_href(href: &str, upstreams: &RegistryOrigins, origin: &str) -> String {
    if let Ok(url) = Url::parse(href) {
        if matches_origin(&url, &upstreams.pypi_files)
            || url.host_str() == Some("files.pythonhosted.org")
        {
            if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
                return href.to_owned();
            }
            let fragment = url
                .fragment()
                .map(|fragment| format!("#{fragment}"))
                .unwrap_or_default();
            let normalized = normalize_url(url, &upstreams.pypi_files);
            let Some(path) = canonical_rewrite_path(normalized.path()) else {
                return href.to_owned();
            };
            return format!("{origin}/pypi/files/{path}{fragment}");
        }
        if (matches_origin(&url, &upstreams.pypi_simple) || url.host_str() == Some("pypi.org"))
            && url.path().starts_with("/simple/")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
        {
            let Some(path) = canonical_rewrite_path(url.path()) else {
                return href.to_owned();
            };
            return format!("{origin}/{path}");
        }
    }
    href.to_owned()
}

fn rewrite_npm_tarball(input: &str, upstreams: &RegistryOrigins, origin: &str) -> Option<String> {
    let url = Url::parse(input).ok()?;
    if (!matches_origin(&url, &upstreams.npm) && url.host_str() != Some("registry.npmjs.org"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let url = normalize_url(url, &upstreams.npm);
    let path = canonical_rewrite_path(url.path())?;
    Some(format!("{origin}/npm/tarballs/{path}"))
}

pub(crate) fn join_url(base: &Url, path: &str) -> Option<Url> {
    if !canonical_path(path) {
        return None;
    }
    build_url(base, path)
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

fn build_url(base: &Url, path: &str) -> Option<Url> {
    if !base.path().ends_with('/')
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || Url::parse(path).is_ok()
    {
        return None;
    }
    let url = base.join(path).ok()?;
    let expected_path = format!("{}{path}", base.path());
    (matches_origin(&url, base)
        && url.path() == expected_path
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

fn canonical_path(path: &str) -> bool {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains("//")
        || path.contains(['?', '#', '\\'])
    {
        return false;
    }
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segment.is_empty() {
            return segments.peek().is_none();
        }
        if !canonical_segment(segment) {
            return false;
        }
    }
    true
}

fn canonical_segment(segment: &str) -> bool {
    if segment.is_empty() || segment == "." || segment == ".." || !segment.is_ascii() {
        return false;
    }
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !(0x21..=0x7e).contains(&byte) || matches!(byte, b'?' | b'#' | b'\\' | b'/') {
            return false;
        }
        if byte != b'%' {
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return false;
        }
        let Some(high) = hex_value(bytes[index + 1]) else {
            return false;
        };
        let Some(low) = hex_value(bytes[index + 2]) else {
            return false;
        };
        if matches!(bytes[index + 1], b'a'..=b'f') || matches!(bytes[index + 2], b'a'..=b'f') {
            return false;
        }
        let decoded = high * 16 + low;
        if decoded.is_ascii_alphanumeric()
            || matches!(
                decoded,
                b'-' | b'.' | b'_' | b'~' | b'/' | b'\\' | b'?' | b'#'
            )
        {
            return false;
        }
        index += 3;
    }
    true
}

fn canonical_rewrite_path(path: &str) -> Option<String> {
    let path = path.strip_prefix('/')?;
    let bytes = path.as_bytes();
    let mut output = String::with_capacity(path.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            output.push(char::from(bytes[index]));
            index += 1;
            continue;
        }
        let high = hex_value(*bytes.get(index + 1)?)?;
        let low = hex_value(*bytes.get(index + 2)?)?;
        let decoded = high * 16 + low;
        if decoded.is_ascii_alphanumeric() || matches!(decoded, b'-' | b'.' | b'_' | b'~') {
            output.push(char::from(decoded));
        } else {
            output.push('%');
            output.push(char::from_digit(u32::from(high), 16)?.to_ascii_uppercase());
            output.push(char::from_digit(u32::from(low), 16)?.to_ascii_uppercase());
        }
        index += 3;
    }
    canonical_path(&output).then_some(output)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn href_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r#"href="([^"]+)"|href='([^']+)'"#).unwrap())
}

#[cfg(test)]
mod tests {
    use super::{
        LimitedOutput, MAX_METADATA_BODY_LEN, RegistryOrigins, cargo_config, cargo_download_url,
        cargo_index_url, npm_packument_url, npm_tarball_url, pypi_file_url, pypi_simple_url,
        rewrite_npm_json, rewrite_pypi_html,
    };
    use serde_json::json;

    #[test]
    fn rewritten_output_stops_at_limit() {
        let mut output = LimitedOutput::new(0, 4);
        output.extend(b"1234").unwrap();
        assert!(output.extend(b"5").is_err());
    }

    #[test]
    fn builds_urls() {
        let upstreams = RegistryOrigins::default();
        assert!(cargo_index_url(&upstreams, "config.json").is_some());
        assert!(cargo_download_url(&upstreams, "serde", "1.0.0").is_some());
        assert!(npm_packument_url(&upstreams, "@scope%2Fname").is_some());
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
    fn rejects_pypi_project_delimiters() {
        let upstreams = RegistryOrigins::default();
        assert!(pypi_simple_url(&upstreams, Some("pkg?query")).is_none());
        assert!(pypi_simple_url(&upstreams, Some("pkg#fragment")).is_none());
    }

    #[test]
    fn rewrites_pypi_html_links() {
        let body =
            br#"<a href="https://files.pythonhosted.org/packages/pkg.whl#sha256=abc">pkg</a>"#;
        let upstreams = RegistryOrigins::default();
        let rewritten = String::from_utf8(
            rewrite_pypi_html(body, &upstreams, "http://localhost", MAX_METADATA_BODY_LEN).unwrap(),
        )
        .unwrap();
        assert!(rewritten.contains("http://localhost/pypi/files/packages/pkg.whl#sha256=abc"));
    }

    #[test]
    fn canonicalizes_safe_encoded_rewrite_paths() {
        let upstreams = RegistryOrigins::default();
        let body =
            br#"<a href="https://files.pythonhosted.org/packages/%70kg%20caf%c3%a9.whl">pkg</a>"#;
        let rewritten = String::from_utf8(
            rewrite_pypi_html(body, &upstreams, "http://localhost", MAX_METADATA_BODY_LEN).unwrap(),
        )
        .unwrap();
        assert!(rewritten.contains("/pypi/files/packages/pkg%20caf%C3%A9.whl"));
    }

    #[test]
    fn leaves_encoded_separator_links_direct() {
        let upstreams = RegistryOrigins::default();
        let body = br#"<a href="https://files.pythonhosted.org/packages/pkg%2Falias.whl">pkg</a>"#;
        let rewritten = String::from_utf8(
            rewrite_pypi_html(body, &upstreams, "http://localhost", MAX_METADATA_BODY_LEN).unwrap(),
        )
        .unwrap();
        assert!(rewritten.contains("https://files.pythonhosted.org/packages/pkg%2Falias.whl"));
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
            &rewrite_npm_json(&body, &upstreams, "http://localhost", MAX_METADATA_BODY_LEN)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            rewritten["versions"]["1.0.0"]["dist"]["tarball"]
                .as_str()
                .unwrap(),
            "http://localhost/npm/tarballs/pkg/-/pkg-1.0.0.tgz"
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
            &rewrite_npm_json(&body, &upstreams, "http://localhost", MAX_METADATA_BODY_LEN)
                .unwrap(),
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
        for package in ["@scope%2Fname", "@scope%2fname"] {
            let url = npm_packument_url(&upstreams, package).unwrap();
            assert_eq!(url.as_str(), "https://registry.npmjs.org/@scope%2Fname");
        }
    }

    #[test]
    fn rejects_noncanonical_paths() {
        let upstreams = RegistryOrigins::default();
        for path in [
            "pkg#one",
            "pkg?one",
            "pkg\\name",
            "pkg//name",
            "pkg/../name",
            "pkg/%2E%2E/name",
            "pkg/%2Fname",
            "pkg/%5Cname",
            "pkg/%23one",
            "pkg/%3Fone",
            "pkg/%6Eame",
            "pkg/%zz",
            "pkg/%2f",
        ] {
            assert!(npm_tarball_url(path, &upstreams).is_none(), "{path}");
        }
    }

    #[test]
    fn accepts_canonical_safe_encoded_paths() {
        let upstreams = RegistryOrigins::default();
        let url = pypi_file_url("packages/pkg%20caf%C3%A9.whl", &upstreams).unwrap();
        assert_eq!(
            url.as_str(),
            "https://files.pythonhosted.org/packages/pkg%20caf%C3%A9.whl"
        );
    }

    #[test]
    fn rejects_scoped_npm_aliases() {
        let upstreams = RegistryOrigins::default();
        for package in [
            "@scope/name",
            "@scope%252Fname",
            "@scope%2Fna%6De",
            "@scope%2Fname%2Fextra",
        ] {
            assert!(
                npm_packument_url(&upstreams, package).is_none(),
                "{package}"
            );
        }
    }

    #[test]
    fn cargo_config_uses_origin() {
        let body = String::from_utf8(cargo_config("https://mirror.example")).unwrap();
        assert!(body.contains("https://mirror.example/cargo/api/v1/crates"));
    }
}
