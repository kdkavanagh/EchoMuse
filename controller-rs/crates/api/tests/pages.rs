//! The dashboard's pages and assets, and add-on ingress (§3.4).

mod common;

use std::net::{IpAddr, Ipv4Addr};

use common::{NOT_LOOPBACK, OUTSIDE_SECRET, Options, TestServer};
use echomuse_api::Ingress;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const BASE: &str = "<base href=\"/api/hassio_ingress/AbC-123_x/\" />";

async fn page(
    server: &TestServer,
    path: &str,
    ingress_path: Option<&str>,
) -> (u16, String, String) {
    let mut request = server.http.get(server.url(path));
    if let Some(ingress_path) = ingress_path {
        request = request.header("x-ingress-path", ingress_path);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    (status, content_type, response.text().await.unwrap())
}

/// One request written by hand, so the path reaches the server exactly as written.
async fn raw_get(server: &TestServer, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(server.address)
        .await
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, response)
}

#[tokio::test]
async fn pages_and_assets_are_served_with_their_types() {
    let server = TestServer::start(Options::default()).await;
    let (status, content_type, html) = page(&server, "/", None).await;
    assert_eq!(
        (status, content_type.as_str()),
        (200, "text/html; charset=utf-8")
    );
    assert!(html.contains("id=\"form-setup\""), "the landing page");
    assert!(!html.contains("<base"));
    let (status, _, html) = page(&server, "/dashboard", None).await;
    assert_eq!(status, 200);
    assert!(html.contains("static/dashboard.js"));

    for (path, expected) in [
        ("/static/favicon.svg", "image/svg+xml"),
        ("/static/dashboard.js", "text/javascript"),
        ("/static/vendor/xterm.css", "text/css"),
    ] {
        let (status, content_type, _) = page(&server, path, None).await;
        assert_eq!(status, 200, "{path}");
        assert!(content_type.starts_with(expected), "{path}: {content_type}");
    }
    assert_eq!(page(&server, "/static/missing.js", None).await.0, 404);
}

#[tokio::test]
async fn nothing_outside_the_static_directory_is_served() {
    let server = TestServer::start(Options::default()).await;
    assert_eq!(raw_get(&server, "/static/favicon.svg").await.0, 200);
    for path in [
        "/static/../secret.txt",
        "/static/%2e%2e/secret.txt",
        "/static/..%2fsecret.txt",
        "/static/%2e%2e%2fsecret.txt",
        "/static/vendor/../../secret.txt",
        "/static/vendor/%2e%2e/%2e%2e/secret.txt",
        "/static/..\\secret.txt",
        "/static//etc/passwd",
        "/../secret.txt",
    ] {
        let (status, response) = raw_get(&server, path).await;
        assert_ne!(status, 200, "{path}: {response}");
        assert!(!response.contains(OUTSIDE_SECRET), "{path}");
        assert!(!response.contains("root:"), "{path}");
    }
}

#[tokio::test]
async fn ingress_serves_only_the_proxy() {
    let server = TestServer::start(Options {
        ingress: Ingress::Only {
            proxy: NOT_LOOPBACK,
        },
        ..Options::default()
    })
    .await;
    for path in [
        "/",
        "/dashboard",
        "/static/favicon.svg",
        "/api/system/setup-state",
        "/api/none",
    ] {
        let (status, body) = server.get(path, None).await;
        assert_eq!(status, 403, "{path}");
        assert_eq!(body["code"], "forbidden", "{path}");
    }
    let mut socket = tokio_tungstenite::connect_async(server.ws_url("/api/events?token=x")).await;
    match &mut socket {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            assert_eq!(response.status().as_u16(), 403);
        }
        other => panic!("expected a refusal, got {:?}", other.as_ref().map(|_| ())),
    }
}

#[tokio::test]
async fn ingress_writes_the_path_into_the_pages_as_their_base() {
    let server = TestServer::start(Options {
        ingress: Ingress::Only { proxy: LOOPBACK },
        ..Options::default()
    })
    .await;
    for path in ["/", "/dashboard"] {
        let (status, _, html) = page(&server, path, Some("/api/hassio_ingress/AbC-123_x")).await;
        assert_eq!(status, 200);
        let base = html.find(BASE).expect("a base element");
        let head = html.find("<head>").unwrap();
        let first_relative = html.find("static/favicon.svg").unwrap();
        assert!(head < base && base < first_relative, "{path}");
        assert_eq!(html.matches("<base").count(), 1);
    }
    let (_, _, html) = page(&server, "/", None).await;
    assert!(!html.contains("<base"));
    let (status, _, _) = page(&server, "/", Some("/x\"><script>alert(1)</script>")).await;
    assert_eq!(status, 400);
    // Assets and the API are served to the proxy as usual.
    assert_eq!(page(&server, "/static/favicon.svg", None).await.0, 200);
    assert_eq!(server.get("/api/system/setup-state", None).await.0, 200);
}

#[tokio::test]
async fn without_ingress_the_header_is_ignored() {
    let server = TestServer::start(Options::default()).await;
    let (status, _, html) = page(&server, "/", Some("/api/hassio_ingress/AbC-123_x")).await;
    assert_eq!(status, 200);
    assert!(!html.contains("<base"));
}
