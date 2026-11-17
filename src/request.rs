use std::net::{IpAddr, SocketAddr};

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

/// The visitor's IP when no edge proxy vouches for it with the proxy secret:
/// the address the connection came from. A header says whatever its sender
/// likes, so none counts unless `trusted` (the `CLIENT_IP_HEADER` setting) names the one
/// a proxy in front of the server sets. From a list (`X-Forwarded-For`) the
/// last entry is taken, the one that proxy added itself; the entries before
/// it are the client's own. A value that isn't an address is not used.
pub fn remote_address(
    headers: &[(String, String)],
    trusted: Option<&str>,
    socket_ip: &str,
) -> String {
    let address = |v: &str| {
        let v = v.trim();
        v.parse::<IpAddr>()
            .ok()
            // Some proxies add the port.
            .or_else(|| v.parse::<SocketAddr>().ok().map(|a| a.ip()))
            .map(|ip| ip.to_string())
    };
    trusted
        .and_then(|name| headers.iter().rev().find(|(k, _)| k == name))
        .and_then(|(_, v)| v.rsplit(',').next())
        .and_then(address)
        .unwrap_or_else(|| socket_ip.to_string())
}

/// The address the usual forwarding headers give: `CF-Connecting-IP`, then
/// `X-Real-IP`, then the left-most `X-Forwarded-For` entry, then the socket
/// address. A client can set all of them, so this is only for a request
/// whose sender is already trusted: the admin's own test event.
pub fn forwarded_address(headers: &[(String, String)], socket_ip: &str) -> String {
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
            find("x-forwarded-for")
                .and_then(|v| v.split(',').next().map(|s| s.trim().to_string()))
                .filter(|v| !v.is_empty())
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
/// templates never see it. `client_ip_header` is the `CLIENT_IP_HEADER`
/// setting, for a request the proxy sent without its own IP header.
pub fn from_proxy(
    mut headers: Vec<(String, String)>,
    secret: Option<&str>,
    client_ip_header: Option<&str>,
    socket_ip: &str,
) -> Option<(Vec<(String, String)>, String)> {
    let Some(secret) = secret else {
        let ip = remote_address(&headers, client_ip_header, socket_ip);
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
    let ip = client_ip.unwrap_or_else(|| remote_address(&headers, client_ip_header, socket_ip));
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
    fn no_header_is_believed_unless_one_is_named() {
        let forged = h(&[
            ("x-forwarded-for", "1.1.1.1, 2.2.2.2"),
            ("cf-connecting-ip", "3.3.3.3"),
            ("x-real-ip", "4.4.4.4"),
            ("x-sst-client-ip", "5.5.5.5"),
        ]);
        assert_eq!(remote_address(&forged, None, "9.9.9.9"), "9.9.9.9");
        let (_, ip) = from_proxy(forged.clone(), None, None, "9.9.9.9").unwrap();
        assert_eq!(ip, "9.9.9.9");

        // The header the operator's proxy overwrites, and only that one.
        assert_eq!(
            remote_address(&forged, Some("cf-connecting-ip"), "9.9.9.9"),
            "3.3.3.3"
        );
        let (_, ip) = from_proxy(forged, None, Some("x-real-ip"), "9.9.9.9").unwrap();
        assert_eq!(ip, "4.4.4.4");
        assert_eq!(remote_address(&[], Some("x-real-ip"), "9.9.9.9"), "9.9.9.9");
    }

    #[test]
    fn a_list_gives_the_entry_the_proxy_added() {
        let last = |v: &str| {
            remote_address(
                &h(&[("x-forwarded-for", v)]),
                Some("x-forwarded-for"),
                "9.9.9.9",
            )
        };
        // What the client sent comes first; the proxy appends what it saw.
        assert_eq!(last("1.1.1.1, 2.2.2.2"), "2.2.2.2");
        assert_eq!(last("8.8.8.8"), "8.8.8.8");
        assert_eq!(last("1.1.1.1, 2001:db8::1"), "2001:db8::1");
        assert_eq!(last("1.1.1.1, 2.2.2.2:4711"), "2.2.2.2");
        // Not an address: the connection's own is used.
        for junk in ["", "1.1.1.1, ", "unknown", "2.2.2.2; Secure"] {
            assert_eq!(last(junk), "9.9.9.9", "{junk:?}");
        }
        // A proxy that adds its own header line after the client's.
        let two = h(&[
            ("x-forwarded-for", "1.1.1.1"),
            ("x-forwarded-for", "2.2.2.2"),
        ]);
        assert_eq!(
            remote_address(&two, Some("x-forwarded-for"), "9.9.9.9"),
            "2.2.2.2"
        );
    }

    #[test]
    fn the_admins_own_test_reads_the_usual_headers() {
        let headers = h(&[
            ("x-forwarded-for", "1.1.1.1, 2.2.2.2"),
            ("cf-connecting-ip", "3.3.3.3"),
        ]);
        assert_eq!(forwarded_address(&headers, "9.9.9.9"), "3.3.3.3");
        assert_eq!(
            forwarded_address(&h(&[("x-forwarded-for", "1.1.1.1, 2.2.2.2")]), "9.9.9.9"),
            "1.1.1.1"
        );
        assert_eq!(forwarded_address(&[], "9.9.9.9"), "9.9.9.9");
        // An empty first entry is no address.
        assert_eq!(
            forwarded_address(&h(&[("x-forwarded-for", ", 2.2.2.2")]), "9.9.9.9"),
            "9.9.9.9"
        );
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
        assert!(from_proxy(headers.clone(), Some("other-secret-value"), None, "9.9.9.9").is_none());

        let (out, ip) = from_proxy(
            headers.clone(),
            Some("s3cret-s3cret-s3cret"),
            None,
            "9.9.9.9",
        )
        .unwrap();
        assert_eq!(ip, "198.51.100.4");
        assert!(out.contains(&("host".into(), "example.com".into())));
        assert!(
            !out.iter()
                .any(|(k, _)| k == "x-proxy-secret" || k == "x-sst-client-ip")
        );

        // With the secret but without the proxy's own IP header, the other
        // headers it passed on count no more than from anyone else.
        let mut no_ip = headers.clone();
        no_ip.retain(|(k, _)| k != "x-sst-client-ip");
        let (_, ip) = from_proxy(no_ip, Some("s3cret-s3cret-s3cret"), None, "9.9.9.9").unwrap();
        assert_eq!(ip, "9.9.9.9");

        let (out, ip) = from_proxy(headers, None, None, "9.9.9.9").unwrap();
        assert_eq!(ip, "9.9.9.9");
        assert!(out.contains(&("host".into(), "tagging-abc.a.run.app".into())));
    }
}
