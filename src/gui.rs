//! The configuration web interface.
//!
//! A small HTTP server on loopback, serving a single self-contained page. It
//! exists so the driver is configurable without opening a text editor, but the
//! TOML file stays the source of truth: the page reads it, writes it back, and
//! the same validation runs on both paths. Anything the GUI cannot express is
//! still editable in the raw TOML pane on the same page.
//!
//! There is no authentication, so it binds to `127.0.0.1` by default. Pointing
//! `general.gui_bind` at a routable address exposes config editing to the
//! network.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::Config;

/// The single page served at `/`.
const PAGE: &str = include_str!("gui/index.html");

/// Serve the configuration interface until `running` clears.
///
/// `get` returns the config currently in force; `set` validates, persists and
/// hands it to the driver.
pub fn serve<G, S>(bind: &str, get: G, set: S, running: &AtomicBool) -> Result<()>
where
    G: Fn() -> Config + Send + 'static,
    S: Fn(Config) -> Result<()> + Send + 'static,
{
    let server = tiny_http::Server::http(bind)
        .map_err(|e| anyhow::anyhow!("binding {bind}: {e}"))?;
    eprintln!("[gui] http://{bind}/");

    while running.load(Ordering::Relaxed) {
        let Some(mut req) = server
            .recv_timeout(Duration::from_millis(300))
            .context("accepting HTTP request")?
        else {
            continue;
        };

        let method = req.method().clone();
        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();

        let response = match (method.as_str(), path.as_str()) {
            ("GET", "/") => html(PAGE),
            ("GET", "/api/config.toml") => match get().to_toml() {
                Ok(t) => text(200, "text/plain; charset=utf-8", t),
                Err(e) => text(500, "text/plain", format!("{e:#}")),
            },
            ("GET", "/api/config.json") => match serde_json::to_string_pretty(&get()) {
                Ok(j) => text(200, "application/json", j),
                Err(e) => text(500, "text/plain", format!("{e:#}")),
            },
            ("GET", "/api/schema.json") => text(200, "application/json", schema().to_string()),
            ("POST", "/api/config.toml") => {
                let mut body = String::new();
                match req.as_reader().read_to_string(&mut body) {
                    Err(e) => text(400, "text/plain", format!("reading body: {e}")),
                    Ok(_) => apply(&set, toml::from_str::<Config>(&body).map_err(Into::into)),
                }
            }
            ("POST", "/api/config.json") => {
                let mut body = String::new();
                match req.as_reader().read_to_string(&mut body) {
                    Err(e) => text(400, "text/plain", format!("reading body: {e}")),
                    Ok(_) => apply(
                        &set,
                        serde_json::from_str::<Config>(&body).map_err(Into::into),
                    ),
                }
            }
            _ => text(404, "text/plain", "not found".to_string()),
        };

        if let Err(e) = req.respond(response) {
            eprintln!("[gui] responding: {e}");
        }
    }
    Ok(())
}

fn apply<S>(set: &S, parsed: Result<Config>) -> tiny_http::Response<std::io::Cursor<Vec<u8>>>
where
    S: Fn(Config) -> Result<()>,
{
    match parsed {
        Err(e) => text(400, "text/plain", format!("{e:#}")),
        Ok(cfg) => match cfg.validate().and_then(|()| set(cfg)) {
            Ok(()) => text(200, "text/plain", "ok".to_string()),
            Err(e) => text(400, "text/plain", format!("{e:#}")),
        },
    }
}

fn html(body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    text(200, "text/html; charset=utf-8", body.to_string())
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

/// Enumerations the page offers as dropdowns, so the two stay in step.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "curve": ["linear", "soft", "hard", "fixed"],
        "aftertouch": ["off", "poly", "channel"],
        "knob_mode": ["absolute", "relative"],
        "relative_format": ["twos", "bin-offset", "sign-bit"],
        "pickup": ["jump", "pickup"],
        "button_mode": ["momentary", "toggle", "trigger"],
        "led_mode": ["follow", "midi", "always", "off"],
        "pads": crate::hid::PADS,
        "knobs": crate::hid::KNOBS,
        "button_bits": crate::hid::BUTTON_BITS,
        "led_count": crate::leds::LED_COUNT,
    })
}
