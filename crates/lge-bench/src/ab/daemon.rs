//! The part of the daemon's page API the A/B runner uses.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// What the runner needs from `lge serve`. [`HttpDaemon`] talks to a real one;
/// tests use a stand-in.
pub trait DaemonApi {
    /// `GET /lge/api/status`: settings, workspaces, repositories, sessions.
    fn status(&self) -> Result<Value>;
    /// `POST /lge/api/settings` with the whole settings object.
    fn set_settings(&self, settings: &Value) -> Result<()>;
    /// `POST /lge/api/repos`: register a folder and index it in the
    /// background. Returns the root the daemon lists it under.
    fn add_repo(&self, path: &Path) -> Result<String>;
    /// `DELETE /lge/api/repos`: drop a folder from the daemon's list.
    fn remove_repo(&self, root: &str) -> Result<()>;
}

/// The daemon at a local `http://` address.
pub struct HttpDaemon {
    base: String,
    agent: ureq::Agent,
}

impl HttpDaemon {
    /// `base` is the page's address, such as `http://127.0.0.1:8787`.
    pub fn new(base: &str) -> Result<HttpDaemon> {
        let base = base.trim().trim_end_matches('/').to_string();
        if !base.starts_with("http://") {
            bail!("the daemon address must start with http:// (got `{base}`)");
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                // The daemon is local: an HTTP_PROXY in the environment must not catch it.
                .proxy(None)
                .timeout_global(Some(Duration::from_secs(60)))
                .build(),
        );
        Ok(HttpDaemon { base, agent })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let uri = format!("{}{path}", self.base);
        let builder = ureq::http::Request::builder().method(method).uri(&uri);
        let sent = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(body.to_string().into_bytes())
                .map(|r| self.agent.run(r)),
            None => builder.body(()).map(|r| self.agent.run(r)),
        }
        .with_context(|| format!("bad request {method} {uri}"))?;
        let response =
            sent.with_context(|| format!("cannot reach the lge daemon at {}; is `lge serve` running?", self.base))?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .read_to_vec()
            .with_context(|| format!("{method} {path}: the answer could not be read"))?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 200 {
            let why = value["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(200).collect());
            bail!("{method} {path}: the daemon answered {status}: {why}");
        }
        Ok(value)
    }
}

impl DaemonApi for HttpDaemon {
    fn status(&self) -> Result<Value> {
        self.call("GET", "/lge/api/status", None)
    }

    fn set_settings(&self, settings: &Value) -> Result<()> {
        self.call("POST", "/lge/api/settings", Some(settings)).map(|_| ())
    }

    fn add_repo(&self, path: &Path) -> Result<String> {
        let answer = self.call(
            "POST",
            "/lge/api/repos",
            Some(&json!({ "path": path.to_string_lossy() })),
        )?;
        Ok(answer["root"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| path.to_string_lossy().to_string()))
    }

    fn remove_repo(&self, root: &str) -> Result<()> {
        // The path goes in the body: nothing to escape in a query string.
        self.call("DELETE", "/lge/api/repos", Some(&json!({ "path": root })))
            .map(|_| ())
    }
}
