//! Crate-internal test doubles shared by the `lib.rs` and `access` test
//! modules: a scripted [`HttpClient`] and a throwaway RSA key for JWT
//! signing.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{HttpClient, HttpMethod, HttpResponse};

pub(crate) fn test_rsa_key() -> &'static str {
    include_str!("testdata/rsa_private.pem")
}

pub(crate) struct MockRoute {
    method:           HttpMethod,
    path:             String,
    status:           u16,
    response_body:    String,
    error_message:    Option<String>,
    max_hits:         Option<usize>,
    hits:             AtomicUsize,
    assert_header:    Option<(String, MockHeaderCheck)>,
    assert_body_json: Option<serde_json::Value>,
}

pub(crate) enum MockHeaderCheck {
    Equals(String),
}

pub(crate) struct MockHttpClient {
    routes:        Vec<MockRoute>,
    request_count: AtomicUsize,
}

impl MockHttpClient {
    pub(crate) fn new() -> Self {
        Self {
            routes:        vec![],
            request_count: AtomicUsize::new(0),
        }
    }

    pub(crate) fn on(mut self, method: HttpMethod, path: &str, status: u16, body: &str) -> Self {
        self.routes.push(MockRoute {
            method,
            path: path.to_string(),
            status,
            response_body: body.to_string(),
            error_message: None,
            max_hits: None,
            hits: AtomicUsize::new(0),
            assert_header: None,
            assert_body_json: None,
        });
        self
    }

    pub(crate) fn on_once(
        mut self,
        method: HttpMethod,
        path: &str,
        status: u16,
        body: &str,
    ) -> Self {
        self.routes.push(MockRoute {
            method,
            path: path.to_string(),
            status,
            response_body: body.to_string(),
            error_message: None,
            max_hits: Some(1),
            hits: AtomicUsize::new(0),
            assert_header: None,
            assert_body_json: None,
        });
        self
    }

    pub(crate) fn fails(mut self, method: HttpMethod, path: &str, message: &str) -> Self {
        self.routes.push(MockRoute {
            method,
            path: path.to_string(),
            status: 0,
            response_body: String::new(),
            error_message: Some(message.to_string()),
            max_hits: None,
            hits: AtomicUsize::new(0),
            assert_header: None,
            assert_body_json: None,
        });
        self
    }

    pub(crate) fn with_req_header(mut self, name: &str, value: &str) -> Self {
        self.routes.last_mut().unwrap().assert_header =
            Some((name.to_string(), MockHeaderCheck::Equals(value.to_string())));
        self
    }

    pub(crate) fn with_req_body(mut self, json_str: &str) -> Self {
        self.routes.last_mut().unwrap().assert_body_json =
            Some(serde_json::from_str(json_str).unwrap());
        self
    }

    pub(crate) fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

impl HttpClient for MockHttpClient {
    async fn request(
        &self,
        method: HttpMethod,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<HttpResponse> {
        self.request_count.fetch_add(1, Ordering::SeqCst);
        for route in &self.routes {
            if method == route.method && url.ends_with(&route.path) {
                if route
                    .max_hits
                    .is_some_and(|max_hits| route.hits.load(Ordering::SeqCst) >= max_hits)
                {
                    continue;
                }
                route.hits.fetch_add(1, Ordering::SeqCst);
                if let Some(message) = &route.error_message {
                    anyhow::bail!("{message}");
                }
                if let Some((name, MockHeaderCheck::Equals(expected))) = &route.assert_header {
                    let (_, v) = headers
                        .iter()
                        .find(|(k, _)| *k == name.as_str())
                        .unwrap_or_else(|| {
                            panic!("Expected header '{name}' not found in request to {url}")
                        });
                    assert_eq!(*v, expected.as_str(), "Header '{name}' mismatch for {url}");
                }
                if let Some(expected_body) = &route.assert_body_json {
                    let actual = body.expect("Expected request body");
                    assert_eq!(actual, expected_body, "Request body mismatch for {url}");
                }
                return Ok(HttpResponse::new(route.status, route.response_body.clone()));
            }
        }
        panic!(
            "No mock route for {:?} {url}\nRegistered routes: {:?}",
            method,
            self.routes
                .iter()
                .map(|r| format!("{:?} {}", r.method, r.path))
                .collect::<Vec<_>>()
        );
    }
}
