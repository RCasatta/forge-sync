use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::manifest::RateLimit;
use crate::{Error, Result};

const HOST: &str = "api.github.com";
const ACCEPT_VALUE: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPolicy {
    pub method: &'static str,
    pub host: &'static str,
    pub authorization: bool,
    pub redirects: bool,
    pub proxies: bool,
}

pub fn request_policy() -> RequestPolicy {
    RequestPolicy {
        method: "GET",
        host: HOST,
        authorization: false,
        redirects: false,
        proxies: false,
    }
}

pub trait GithubApi {
    fn get(&mut self, endpoint: &str) -> Result<Value>;
    fn prime_rate_limit(&mut self) -> Result<()> {
        Ok(())
    }
    fn rate_limit(&self) -> RateLimit;
    fn requests(&self) -> u64;
}

pub struct GithubClient {
    client: Client,
    rate_limit: RateLimit,
    requests: u64,
}

impl GithubClient {
    pub fn new() -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(ACCEPT_VALUE));
        headers.insert(USER_AGENT, HeaderValue::from_static("forge-sync/0.1"));
        headers.insert(
            "x-github-api-version",
            HeaderValue::from_static(API_VERSION),
        );
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .https_only(true)
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::Remote("could not initialize GitHub HTTPS client".to_owned()))?;
        Ok(Self {
            client,
            rate_limit: RateLimit::default(),
            requests: 0,
        })
    }

    fn update_rate(&mut self, headers: &HeaderMap) {
        let number = |name: &str| headers.get(name)?.to_str().ok()?.parse().ok();
        self.rate_limit = RateLimit {
            limit: number("x-ratelimit-limit"),
            remaining: number("x-ratelimit-remaining"),
            reset_epoch: number("x-ratelimit-reset").map(i64::from),
        };
    }
}

impl GithubApi for GithubClient {
    fn get(&mut self, endpoint: &str) -> Result<Value> {
        if !endpoint.starts_with('/') || endpoint.starts_with("//") || endpoint.contains("://") {
            return Err(Error::Remote(
                "refused invalid internally generated GitHub endpoint".to_owned(),
            ));
        }
        if self
            .rate_limit
            .remaining
            .is_some_and(|remaining| remaining <= 2)
        {
            return Err(Error::RateLimited(self.rate_limit.remaining.unwrap_or(0)));
        }
        let url = format!("https://{HOST}{endpoint}");
        let response = self
            .client
            .request(Method::GET, url)
            .send()
            .map_err(|_| Error::Remote("GitHub HTTPS request failed".to_owned()))?;
        self.requests += 1;
        self.update_rate(response.headers());
        if response.status().is_redirection() {
            return Err(Error::Remote("GitHub redirect refused".to_owned()));
        }
        if response.status() == StatusCode::FORBIDDEN && self.rate_limit.remaining == Some(0) {
            return Err(Error::RateLimited(0));
        }
        if !response.status().is_success() {
            return Err(Error::Remote(format!(
                "GitHub returned HTTP {}",
                response.status().as_u16()
            )));
        }
        response.json().map_err(|_| Error::Json {
            context: "GitHub response".to_owned(),
        })
    }

    fn prime_rate_limit(&mut self) -> Result<()> {
        let value = self.get("/rate_limit")?;
        let core = value
            .pointer("/resources/core")
            .ok_or_else(|| Error::Json {
                context: "GitHub rate-limit response".to_owned(),
            })?;
        self.rate_limit = RateLimit {
            limit: core
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|v| v.try_into().ok()),
            remaining: core
                .get("remaining")
                .and_then(Value::as_u64)
                .and_then(|v| v.try_into().ok()),
            reset_epoch: core.get("reset").and_then(Value::as_i64),
        };
        Ok(())
    }

    fn rate_limit(&self) -> RateLimit {
        self.rate_limit.clone()
    }
    fn requests(&self) -> u64 {
        self.requests
    }
}

pub fn encode_path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_fixed_read_only_and_anonymous() {
        assert_eq!(
            request_policy(),
            RequestPolicy {
                method: "GET",
                host: "api.github.com",
                authorization: false,
                redirects: false,
                proxies: false,
            }
        );
    }
}
