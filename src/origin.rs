use axum::http::{HeaderMap, header, uri::Authority};
use std::str::FromStr;
use url::Url;

#[derive(Clone)]
pub struct OriginPolicy {
    public_url: Option<String>,
}

impl OriginPolicy {
    pub fn new(public_url: Option<String>) -> Self {
        Self { public_url }
    }

    pub fn effective_origin(&self, headers: &HeaderMap) -> Result<String, ()> {
        if let Some(origin) = &self.public_url {
            return Ok(origin.clone());
        }

        if let Some(value) = headers.get("forwarded") {
            let value = value.to_str().map_err(|_| ())?;
            if value.contains(',') {
                return Err(());
            }
            let mut proto = None;
            let mut host = None;
            for parameter in value.split(';') {
                let (name, value) = parameter.trim().split_once('=').ok_or(())?;
                if value.starts_with('"') || value.ends_with('"') {
                    return Err(());
                }
                match name.to_ascii_lowercase().as_str() {
                    "proto" if proto.replace(value).is_none() => {}
                    "host" if host.replace(value).is_none() => {}
                    "proto" | "host" => return Err(()),
                    _ => {}
                }
            }
            return origin_from_parts(proto.ok_or(())?, host.ok_or(())?);
        }
        let proto = headers.get("x-forwarded-proto");
        let host = headers.get("x-forwarded-host");
        if proto.is_some() || host.is_some() {
            let proto = proto.and_then(|v| v.to_str().ok()).ok_or(())?;
            let host = host.and_then(|v| v.to_str().ok()).ok_or(())?;
            if proto.contains(',') || host.contains(',') {
                return Err(());
            }
            return origin_from_parts(proto, host);
        }

        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .ok_or(())?;
        let authority = Authority::from_str(host).map_err(|_| ())?;
        origin_from_parts("http", authority.as_str())
    }

    pub fn secure_cookies(&self) -> bool {
        self.public_url
            .as_deref()
            .and_then(|origin| Url::parse(origin).ok())
            .is_some_and(|url| url.scheme() == "https")
    }

    pub(crate) fn mcp_security(&self) -> (Vec<String>, Vec<String>) {
        if let Some(origin) = self
            .public_url
            .as_ref()
            .and_then(|value| Url::parse(value).ok())
            && let Some(authority) = origin_authority(&origin)
        {
            return (vec![authority], vec![origin.origin().ascii_serialization()]);
        }
        (Vec::new(), Vec::new())
    }
}

pub fn normalize_origin(origin: &str) -> String {
    fn strip(origin: &str, scheme: &str, port: &str) -> Option<String> {
        let rest = origin.strip_prefix(scheme)?;
        let host = rest.strip_suffix(port)?;
        if host.is_empty() || host.contains('/') {
            return None;
        }
        Some(format!("{scheme}{host}"))
    }
    strip(origin, "https://", ":443")
        .or_else(|| strip(origin, "http://", ":80"))
        .unwrap_or_else(|| origin.to_owned())
}

fn origin_authority(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    match url.port() {
        Some(port) if host.contains(':') => Some(format!("[{host}]:{port}")),
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host.to_owned()),
    }
}

fn origin_from_parts(proto: &str, host: &str) -> Result<String, ()> {
    if !matches!(proto, "http" | "https") || host.len() > 255 {
        return Err(());
    }
    let authority = Authority::from_str(host).map_err(|_| ())?;
    if authority
        .as_str()
        .bytes()
        .any(|byte| byte.is_ascii_control())
    {
        return Err(());
    }
    Ok(normalize_origin(&format!("{proto}://{authority}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(values: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.insert(
                axum::http::HeaderName::from_str(name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn forwarded_headers_apply_only_without_a_public_url() {
        let derived = OriginPolicy::new(None);
        assert_eq!(
            derived
                .effective_origin(&headers(&[("host", "127.0.0.1:3000")]))
                .unwrap(),
            "http://127.0.0.1:3000"
        );
        assert_eq!(
            derived
                .effective_origin(&headers(&[
                    ("host", "internal:3000"),
                    ("forwarded", "proto=https;host=aegis.example"),
                ]))
                .unwrap(),
            "https://aegis.example"
        );
        assert_eq!(
            derived
                .effective_origin(&headers(&[
                    ("host", "internal:3000"),
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-host", "aegis.example"),
                ]))
                .unwrap(),
            "https://aegis.example"
        );
        assert!(
            derived
                .effective_origin(&headers(&[(
                    "forwarded",
                    "proto=https;host=aegis.example,proto=http;host=evil.example",
                )]))
                .is_err()
        );

        let fixed = OriginPolicy::new(Some("https://aegis.example".into()));
        assert_eq!(
            fixed
                .effective_origin(&headers(&[
                    ("host", "evil.example"),
                    ("x-forwarded-proto", "http"),
                    ("x-forwarded-host", "evil.example"),
                ]))
                .unwrap(),
            "https://aegis.example"
        );
    }

    #[test]
    fn host_without_public_url_derives_any_valid_authority() {
        let direct = OriginPolicy::new(None);
        assert_eq!(
            direct
                .effective_origin(&headers(&[("host", "192.168.1.5:3000")]))
                .unwrap(),
            "http://192.168.1.5:3000"
        );
        assert_eq!(
            direct
                .effective_origin(&headers(&[("host", "aegis.lan:80")]))
                .unwrap(),
            "http://aegis.lan"
        );
        assert!(
            direct
                .effective_origin(&headers(&[("host", "not a host")]))
                .is_err()
        );
        assert!(direct.effective_origin(&headers(&[("host", "")])).is_err());
        assert!(direct.effective_origin(&headers(&[])).is_err());
        assert_eq!(direct.mcp_security(), (Vec::new(), Vec::new()));
    }

    #[test]
    fn normalize_origin_strips_only_default_ports() {
        assert_eq!(
            normalize_origin("https://aegis.example:443"),
            "https://aegis.example"
        );
        assert_eq!(
            normalize_origin("http://aegis.example:80"),
            "http://aegis.example"
        );
        assert_eq!(
            normalize_origin("https://aegis.example"),
            "https://aegis.example"
        );
        assert_eq!(
            normalize_origin("https://aegis.example:8443"),
            "https://aegis.example:8443"
        );
        assert_eq!(normalize_origin("http://[::1]:80"), "http://[::1]");
        assert_eq!(
            normalize_origin("http://aegis.example:443"),
            "http://aegis.example:443"
        );
    }
}
