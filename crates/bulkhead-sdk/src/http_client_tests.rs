#![allow(non_snake_case)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::*;

#[test]
fn parse_url____https_with_path_and_query____split() {
    let url = parse_url("https://api.example.com:8443/v1/x?y=1#frag").unwrap();

    assert_eq!(
        url,
        Url {
            https: true,
            authority: "api.example.com:8443",
            path_with_query: "/v1/x?y=1",
        }
    );
}

#[test]
fn parse_url____no_path____slash() {
    let url = parse_url("http://10.0.0.5").unwrap();

    assert_eq!(
        (url.https, url.authority, url.path_with_query),
        (false, "10.0.0.5", "/")
    );
}

#[test]
fn parse_url____query_only____kept() {
    assert_eq!(parse_url("http://h?q=1").unwrap().path_with_query, "?q=1");
}

#[test]
fn parse_url____other_scheme_user_or_no_host____invalid() {
    for url in ["ftp://h/", "https://me@h/", "https:///x", "h/x"] {
        assert!(
            matches!(parse_url(url), Err(HttpError::Invalid(_))),
            "{url}"
        );
    }
}

#[test]
fn Request____builder____fields_set() {
    let r = Request::new("PUT", "https://h/")
        .header("x-a", "1")
        .body("hi")
        .timeout(Duration::from_secs(2));

    assert_eq!(r.headers, vec![("x-a".to_string(), b"1".to_vec())]);
    assert_eq!(r.body, b"hi");
    assert_eq!(r.timeout, Some(Duration::from_secs(2)));
}

#[test]
fn body_error_text____timeout_or_other____classified() {
    assert_eq!(
        body_error_text("connection read timeout"),
        HttpError::Timeout
    );
    assert!(matches!(body_error_text("reset"), HttpError::Other(_)));
}
