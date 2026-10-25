use serde::Serialize;

/// An incoming HTTP request, as templates see it through the `getRequest*` APIs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IncomingRequest {
    pub method: String,
    pub path: String,
    /// Query string without the leading `?`, as Google sGTM returns it.
    pub query: String,
    /// Header names are lowercase. Repeated headers keep one entry each.
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub remote_address: String,
}

impl IncomingRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Picks the visitor's IP: `CF-Connecting-IP`, then `X-Real-IP`, then the
/// left-most `X-Forwarded-For` entry, then the socket address.
pub fn remote_address(headers: &[(String, String)], socket_ip: &str) -> String {
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    find("cf-connecting-ip")
        .or_else(|| find("x-real-ip"))
        .or_else(|| {
            find("x-forwarded-for").and_then(|v| v.split(',').next().map(|s| s.trim().to_string()))
        })
        .unwrap_or_else(|| socket_ip.to_string())
}

/// Header carrying the shared secret between the edge proxy and the server.
pub const PROXY_SECRET_HEADER: &str = "x-proxy-secret";
/// Header the edge proxy sets to the visitor's IP.
pub const CLIENT_IP_HEADER: &str = "x-sst-client-ip";

/// Applies the edge proxy's headers. Without a secret configured, headers
/// pass through and the IP comes from [`remote_address`]. With one, a request
/// that doesn't carry it is refused (`None`). A request that does gets the
/// proxy's forwarded host and client IP, and the secret is removed so
/// templates never see it.
pub fn from_proxy(
    mut headers: Vec<(String, String)>,
    secret: Option<&str>,
    socket_ip: &str,
) -> Option<(Vec<(String, String)>, String)> {
    let Some(secret) = secret else {
        let ip = remote_address(&headers, socket_ip);
        return Some((headers, ip));
    };
    let value = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_string())
    };
    if !value(PROXY_SECRET_HEADER).is_some_and(|given| same(&given, secret)) {
        return None;
    }
    let forwarded_host = value("x-forwarded-host")
        .and_then(|v| v.split(',').next().map(|s| s.trim().to_string()))
        .filter(|v| !v.is_empty());
    let client_ip = value(CLIENT_IP_HEADER).filter(|v| !v.is_empty());

    headers.retain(|(k, _)| k != PROXY_SECRET_HEADER && k != CLIENT_IP_HEADER);
    if let Some(host) = forwarded_host {
        headers.retain(|(k, _)| k != "host");
        headers.push(("host".into(), host));
    }
    let ip = client_ip.unwrap_or_else(|| remote_address(&headers, socket_ip));
    Some((headers, ip))
}

/// Compares without stopping at the first difference.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn prefers_cloudflare_header() {
        let headers = h(&[
            ("x-forwarded-for", "1.1.1.1, 2.2.2.2"),
            ("cf-connecting-ip", "3.3.3.3"),
        ]);
        assert_eq!(remote_address(&headers, "9.9.9.9"), "3.3.3.3");
    }

    #[test]
    fn falls_back_to_forwarded_for_then_socket() {
        assert_eq!(
            remote_address(&h(&[("x-forwarded-for", "1.1.1.1, 2.2.2.2")]), "9.9.9.9"),
            "1.1.1.1"
        );
        assert_eq!(remote_address(&[], "9.9.9.9"), "9.9.9.9");
    }

    #[test]
    fn proxy_secret_gates_and_rewrites() {
        let headers = h(&[
            ("host", "tagging-abc.a.run.app"),
            ("x-forwarded-host", "example.com"),
            ("x-sst-client-ip", "198.51.100.4"),
            ("x-proxy-secret", "s3cret-s3cret-s3cret"),
            ("cf-connecting-ip", "10.0.0.1"),
        ]);
        assert!(from_proxy(headers.clone(), Some("other-secret-value"), "9.9.9.9").is_none());

        let (out, ip) =
            from_proxy(headers.clone(), Some("s3cret-s3cret-s3cret"), "9.9.9.9").unwrap();
        assert_eq!(ip, "198.51.100.4");
        assert!(out.contains(&("host".into(), "example.com".into())));
        assert!(
            !out.iter()
                .any(|(k, _)| k == "x-proxy-secret" || k == "x-sst-client-ip")
        );

        let (out, ip) = from_proxy(headers, None, "9.9.9.9").unwrap();
        assert_eq!(ip, "10.0.0.1");
        assert!(out.contains(&("host".into(), "tagging-abc.a.run.app".into())));
    }
}
