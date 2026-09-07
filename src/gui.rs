//! The remote configuration page.
//!
//! Deliberately narrow. `mk3-gui` is the full editor: a panel map that lights
//! up as you press things, which needs a screen in front of the hardware. This
//! exists for the other case -- a machine you are only logged into over the
//! network -- and covers the two jobs that still make sense there: switching
//! preset, and editing the file.
//!
//! It does **not** reimplement the panel editor. An earlier version did, and
//! went stale the moment the config grew a device profile: it kept writing
//! fields that no longer existed, producing files the driver refused. A page
//! that edits TOML as text cannot drift that way, because it knows nothing
//! about the schema -- the driver validates, and the page shows what it says.
//!
//! There is no authentication, so it binds to loopback by default. Pointing
//! `general.gui_bind` at a routable address exposes config editing to the
//! network.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The single page served at `/`.
const PAGE: &str = include_str!("gui/index.html");

/// What the page needs from the driver.
pub struct Backend {
    /// Current config text and its path.
    pub get: Box<dyn Fn() -> (String, String) + Send + Sync>,
    /// Validate, persist and apply a config.
    pub set: Box<dyn Fn(String) -> Result<()> + Send + Sync>,
    /// A short description of the device in force.
    pub device: Box<dyn Fn() -> String + Send + Sync>,
    /// Presets available, as `(name, description, builtin)`, plus the active
    /// one and the directory they live in.
    pub presets: Box<dyn Fn() -> (Vec<(String, String, bool)>, Option<String>, String) + Send + Sync>,
    /// Load a preset by name.
    pub load_preset: Box<dyn Fn(&str) -> Result<()> + Send + Sync>,
    /// Save the current config under a name.
    pub save_preset: Box<dyn Fn(&str, &str) -> Result<()> + Send + Sync>,
}

/// Serve the configuration page until `running` clears.
pub fn serve(bind: &str, backend: Backend, running: &AtomicBool) -> Result<()> {
    let server =
        tiny_http::Server::http(bind).map_err(|e| anyhow::anyhow!("binding {bind}: {e}"))?;
    eprintln!("[gui] http://{bind}/");

    while running.load(Ordering::Relaxed) {
        let Some(mut req) = server
            .recv_timeout(Duration::from_millis(300))
            .context("accepting HTTP request")?
        else {
            continue;
        };

        let method = req.method().as_str().to_string();
        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();

        let response = match (method.as_str(), path.as_str()) {
            ("GET", "/") => text(200, "text/html; charset=utf-8", PAGE.to_string()),
            ("GET", "/api/state") => {
                let (toml, cfg_path) = (backend.get)();
                let (list, active, dir) = (backend.presets)();
                let presets: Vec<serde_json::Value> = list
                    .into_iter()
                    .map(|(name, description, builtin)| {
                        serde_json::json!({
                            "name": name,
                            "description": description,
                            "builtin": builtin,
                        })
                    })
                    .collect();
                let body = serde_json::json!({
                    "device": (backend.device)(),
                    "path": cfg_path,
                    "toml": toml,
                    "presets": presets,
                    "active": active,
                    "presets_dir": dir,
                });
                text(200, "application/json", body.to_string())
            }
            ("POST", "/api/config") => match body_of(&mut req) {
                Err(e) => text(400, "text/plain", e),
                Ok(b) => report((backend.set)(b), "applied"),
            },
            ("POST", "/api/preset/load") => match body_of(&mut req) {
                Err(e) => text(400, "text/plain", e),
                Ok(name) => report((backend.load_preset)(name.trim()), "loaded"),
            },
            ("POST", "/api/preset/save") => match body_of(&mut req) {
                Err(e) => text(400, "text/plain", e),
                Ok(b) => {
                    // `name\ndescription`: the description may contain
                    // anything, so only the first newline separates them.
                    let (name, description) = b.split_once('\n').unwrap_or((b.as_str(), ""));
                    report((backend.save_preset)(name.trim(), description.trim()), "saved")
                }
            },
            _ => text(404, "text/plain", "not found".to_string()),
        };

        if let Err(e) = req.respond(response) {
            eprintln!("[gui] responding: {e}");
        }
    }
    Ok(())
}

fn body_of(req: &mut tiny_http::Request) -> Result<String, String> {
    let mut body = String::new();
    req.as_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("reading body: {e}"))?;
    Ok(body)
}

fn report(r: Result<()>, ok: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    match r {
        Ok(()) => text(200, "text/plain", ok.to_string()),
        Err(e) => text(400, "text/plain", format!("{e:#}")),
    }
}

fn text(
    status: u16,
    content_type: &str,
    body: String,
) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("static content-type header");
    tiny_http::Response::from_data(body.into_bytes())
        .with_status_code(status)
        .with_header(header)
}
